#!/usr/bin/env python3
"""
FrankenRust build loop.

Unattended orchestrator. GitHub Issues are the queue; agents drain them in
parallel git worktrees. Work merges only if it passes the verification gate,
which agents are forbidden to weaken.

Per issue:

    claim ─► critic ─┬─ REVISE ─► comment, label fr:questioned, move on
                     └─ PROCEED ─► implementer ─► gate ─┬─ fail ─► retry (<=3)
                                                        └─ pass ─► 2 adversarial
                                                             reviewers ─┬─ BLOCK ─► fixer
                                                                        └─ PASS ─► merge, close

The critic stage exists because the issues are written by agents, not by a
human who read the code. An agent that faithfully implements a wrong issue
produces work that passes the gate and looks like progress.

    python3 orchestrator/loop.py seed      # planner agent files the initial issues
    python3 orchestrator/loop.py run       # drain the queue
    python3 orchestrator/loop.py status    # what is open / claimed / blocked
"""

from __future__ import annotations

import json
import os
import subprocess
import shutil
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import gh  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
ORCH = ROOT / "orchestrator"
LOGS = ORCH / "logs"
WORKTREES = ORCH / "worktrees"
PROMPTS = ORCH / "prompts"
GATE = ROOT / "scripts" / "gate.sh"

# --- knobs -------------------------------------------------------------------
MAX_PARALLEL = int(os.environ.get("FR_PARALLEL", "3"))
MAX_ATTEMPTS = int(os.environ.get("FR_ATTEMPTS", "3"))
AGENT_TIMEOUT = int(os.environ.get("FR_AGENT_TIMEOUT", str(60 * 60)))
GATE_TIMEOUT = int(os.environ.get("FR_GATE_TIMEOUT", str(30 * 60)))
WALLCLOCK_LIMIT = int(os.environ.get("FR_WALLCLOCK", str(8 * 60 * 60)))
HEARTBEAT = int(os.environ.get("FR_HEARTBEAT", "60"))
RETRO_EVERY = int(os.environ.get("FR_RETRO_EVERY", "2"))   # cycles per retrospective
# -----------------------------------------------------------------------------

# Implementation is bulk mechanical translation against a spec that already
# names the files -- Sonnet handles it. Critique, review and fixing are where
# judgement matters (unsound unsafe, thread-affinity bugs a green suite misses),
# so those get Opus.
MODELS = {
    "implementer": os.environ.get("FR_MODEL_IMPL", "claude-sonnet-5"),
    "critic": os.environ.get("FR_MODEL_CRITIC", "claude-opus-5"),
    "reviewer": os.environ.get("FR_MODEL_REVIEW", "claude-opus-5"),
    "fixer": os.environ.get("FR_MODEL_FIX", "claude-opus-5"),
    "planner": os.environ.get("FR_MODEL_PLAN", "claude-opus-5"),
}
ESCALATED_MODEL = os.environ.get("FR_MODEL_ESCALATE", "claude-opus-5")

# Codex runs on a separate quota that will likely run out mid-run. When it does,
# remaining codex work becomes claude work rather than blocking the queue.
# Detection is by output pattern; the CLI gives no distinct exit code for it.
CODEX_LIMIT_PATTERNS = (
    "usage limit", "rate limit", "rate_limit", "quota", "429",
    "too many requests", "insufficient_quota", "you've hit your",
    "resource_exhausted", "please try again later",
)

_merge_lock = threading.Lock()
_claim_lock = threading.Lock()
_codex_lock = threading.Lock()
_codex_disabled = False
_start = time.time()


def now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def log(msg: str) -> None:
    line = f"[{now()}] {msg}"
    print(line, flush=True)
    LOGS.mkdir(parents=True, exist_ok=True)
    with (LOGS / "loop.log").open("a") as fh:
        fh.write(line + "\n")


_journal_lock = threading.Lock()
JOURNAL = LOGS / "events.jsonl"


def record(event: str, **fields) -> None:
    """Append a structured event.

    The retrospective reads THIS, not the agent transcripts. Transcripts are
    enormous and unstructured; asking a model to find systemic patterns in them
    produces impressions rather than findings. One JSON line per stage outcome
    makes "the critic rejected 4 issues for the same reason" a countable fact.
    """
    rec = {"ts": now(), "event": event, **fields}
    LOGS.mkdir(parents=True, exist_ok=True)
    with _journal_lock, JOURNAL.open("a") as fh:
        fh.write(json.dumps(rec) + "\n")


def codex_ok() -> bool:
    with _codex_lock:
        return not _codex_disabled


def disable_codex(reason: str) -> None:
    global _codex_disabled
    with _codex_lock:
        if _codex_disabled:
            return
        _codex_disabled = True
    log(f"!! codex disabled for the rest of this run ({reason}); falling back to claude")


def resolve(agent: str, role: str, escalate: bool = False) -> tuple[str, str | None]:
    """Map requested agent+role onto (actual_agent, model)."""
    if agent == "codex" and codex_ok() and not escalate:
        return "codex", None
    if escalate:
        return "claude", ESCALATED_MODEL
    return "claude", MODELS.get(role, MODELS["implementer"])


def run(cmd: list[str], cwd: Path, timeout: int, log_path: Path | None = None) -> tuple[int, str]:
    """Run a command, tee to a log, return (rc, tail)."""
    if log_path:
        log_path.parent.mkdir(parents=True, exist_ok=True)
    buf: list[str] = []
    try:
        proc = subprocess.Popen(cmd, cwd=str(cwd), stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, text=True, errors="replace",
                                env={**os.environ, "FRANKENRUST_AGENT": "1"})
    except FileNotFoundError as exc:
        return 127, f"command not found: {exc}"
    fh = log_path.open("a") if log_path else None
    try:
        deadline = time.time() + timeout
        assert proc.stdout is not None
        for line in proc.stdout:
            buf.append(line)
            if fh:
                fh.write(line)
                fh.flush()
            if time.time() > deadline:
                proc.kill()
                buf.append(f"\n!! TIMEOUT after {timeout}s\n")
                break
        proc.wait(timeout=30)
    except Exception as exc:  # noqa: BLE001
        proc.kill()
        buf.append(f"\n!! orchestrator error: {exc}\n")
    finally:
        if fh:
            fh.close()
    return (proc.returncode if proc.returncode is not None else 1), "".join(buf[-400:])


def git(args: list[str], cwd: Path = ROOT) -> tuple[int, str]:
    p = subprocess.run(["git", *args], cwd=str(cwd), capture_output=True, text=True)
    return p.returncode, (p.stdout + p.stderr).strip()


# --- agents ------------------------------------------------------------------
def agent_cmd(agent: str, model: str | None) -> list[str]:
    """Sandboxed as tightly as the work allows: these run for hours unwatched."""
    if agent == "codex":
        if os.environ.get("FR_YOLO") == "1":
            return ["codex", "exec", "--dangerously-bypass-approvals-and-sandbox",
                    "--skip-git-repo-check", "-"]
        return ["codex", "exec", "-s", "workspace-write",
                "-c", "sandbox_workspace_write.network_access=true",
                "-c", 'approval_policy="never"', "--skip-git-repo-check", "-"]
    if agent == "claude":
        # stream-json, not text. `text` buffers everything to the end and then
        # prints a bare "Execution error" on failure -- the log tells you the
        # run died but not why, which is useless at 3am into an 8h run. NDJSON
        # lands incrementally (so `tail -f` works and the log grows as proof of
        # life) and carries the real error in its final result event.
        cmd = ["claude", "-p", "--dangerously-skip-permissions",
               "--output-format", "stream-json", "--verbose"]
        if model:
            cmd += ["--model", model]
        return cmd
    raise ValueError(f"unknown agent {agent!r}")


def _final_text(logpath: Path, agent: str) -> tuple[str, str]:
    """(final message, error reason) from an agent log.

    Only the *final* message may feed verdict parsing. The full stream contains
    reasoning and tool calls, and a reviewer musing "this is not a VERDICT:
    BLOCK situation" would otherwise block a clean diff. codex writes plain
    text, so it passes through.
    """
    if not logpath.exists():
        return "", "no log"
    raw = logpath.read_text(errors="replace")
    if agent != "claude":
        return raw, ""
    final, err = "", ""
    for line in raw.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") != "result":
            continue
        final = ev.get("result") or final
        if ev.get("is_error") or ev.get("subtype") not in (None, "success"):
            err = ev.get("subtype") or "error"
            if ev.get("api_error_status"):
                err += f" ({ev['api_error_status']})"
    if not final and not err:
        # No result event at all: the process died mid-stream.
        err = "no result event (killed or crashed mid-stream)"
    return final, err


def _hit_limit(logpath: Path) -> bool:
    if not logpath.exists():
        return False
    tail = logpath.read_text(errors="replace")[-4000:].lower()
    return any(p in tail for p in CODEX_LIMIT_PATTERNS)


def invoke(agent: str, wt: Path, prompt: str, logdir: Path, tag: str,
           role: str = "implementer", escalate: bool = False) -> tuple[str, int, str]:
    """Run an agent. Returns (agent_used, rc, output_text).

    Prompt goes over stdin so we never hit argv limits. If codex dies on quota
    it is disabled for the run and the same prompt retried on claude -- a quota
    wall should cost one wasted invocation, not a whole issue.
    """
    logdir.mkdir(parents=True, exist_ok=True)
    (logdir / f"prompt.{tag}.md").write_text(prompt)
    pf = logdir / f"prompt.{tag}.md"
    use, model = resolve(agent, role, escalate)
    rc, logpath = 1, logdir / f"{use}.{tag}.log"
    for _ in range(2):
        logpath = logdir / f"{use}.{tag}.log"
        log(f"    -> {use}{f'({model})' if model else ''} {tag} ({wt.name})")
        with pf.open("rb") as stdin_fh, logpath.open("ab") as out_fh:
            proc = subprocess.Popen(agent_cmd(use, model), cwd=str(wt), stdin=stdin_fh,
                                    stdout=out_fh, stderr=subprocess.STDOUT,
                                    env={**os.environ, "FRANKENRUST_AGENT": "1"})
            # Heartbeat rather than a bare wait(). Agents emit nothing until
            # they exit -- `--output-format text` buffers, and codex is quiet
            # too -- so an hour-long task and a hung one look identical from
            # the outside. Proof of life is worth one log line a minute.
            started = time.time()
            rc = None
            while rc is None:
                try:
                    rc = proc.wait(timeout=HEARTBEAT)
                except subprocess.TimeoutExpired:
                    waited = time.time() - started
                    if waited >= AGENT_TIMEOUT:
                        proc.kill()
                        log(f"    !! {use} timed out after {AGENT_TIMEOUT}s")
                        record("agent_timeout", agent=use, tag=tag,
                               seconds=AGENT_TIMEOUT)
                        return use, 124, ""
                    kb = logpath.stat().st_size // 1024 if logpath.exists() else 0
                    log(f"    .. {tag} running {int(waited // 60)}m"
                        f" (limit {AGENT_TIMEOUT // 60}m, {kb}KB)")
        text, err = _final_text(logpath, use)
        if err or rc != 0:
            # Say what went wrong at the point it goes wrong. A silent failure
            # here surfaces later as an empty verdict and gets misread as the
            # agent declining the work.
            log(f"    !! {use} {tag} failed: rc={rc} {err or '(no error detail)'}")
            record("agent_error", agent=use, tag=tag, rc=rc, reason=err)
        # Require BOTH a failure and a quota pattern: "429"/"rate limit" appear
        # legitimately in the output of an agent writing an HTTP server.
        if use == "codex" and rc != 0 and _hit_limit(logpath):
            record("agent_fallback", agent="codex", to="claude", tag=tag, rc=rc)
            disable_codex(f"exit {rc} with a quota pattern")
            use, model = resolve("codex", role, escalate)
            continue
        break
    return use, rc, text


def prompt_for(role: str, issue: gh.Issue, extra: str = "") -> str:
    return "\n".join([
        (PROMPTS / "shared.md").read_text(),
        (PROMPTS / f"{role}.md").read_text(),
        f"\n# Issue #{issue.number}: {issue.title}\n",
        issue.body,
        extra,
    ])


# --- worktrees ---------------------------------------------------------------
def make_worktree(tid: str) -> Path:
    wt, branch = WORKTREES / tid, f"issue/{tid}"
    if wt.exists():
        git(["worktree", "remove", "--force", str(wt)])
        shutil.rmtree(wt, ignore_errors=True)
    git(["branch", "-D", branch])
    rc, out = git(["worktree", "add", "-b", branch, str(wt), "main"])
    if rc != 0:
        raise RuntimeError(f"worktree add failed: {out}")
    return wt


def merge_worktree(tid: str, logdir: Path, gate: str) -> bool:
    """Serialised; rebase onto latest main and re-gate so parallel work composes."""
    branch, wt = f"issue/{tid}", WORKTREES / tid
    with _merge_lock:
        git(["add", "-A"], cwd=wt)
        git(["commit", "-m", f"{tid}: agent work", "--allow-empty"], cwd=wt)
        rc, out = git(["rebase", "main"], cwd=wt)
        if rc != 0:
            git(["rebase", "--abort"], cwd=wt)
            log(f"    !! {tid} rebase conflict onto main")
            record("rebase_conflict", issue=tid)
            return False
        rc, _ = run(["bash", str(GATE), gate], wt, GATE_TIMEOUT, logdir / "gate.rebase.log")
        if rc != 0:
            log(f"    !! {tid} gate failed after rebase onto main")
            record("merge_regate_fail", issue=tid)
            return False
        rc, out = git(["merge", "--ff-only", branch])
        if rc != 0:
            log(f"    !! {tid} ff merge failed: {out}")
            return False
        log(f"    ++ {tid} merged into main")
        return True


# --- stages ------------------------------------------------------------------
def review_stage(issue: gh.Issue, wt: Path, logdir: Path, tag: str) -> str | None:
    """Two adversarial reviewers, independent contexts, diff only."""
    _, diff = git(["diff", "main...HEAD"], cwd=wt)
    if not diff.strip():
        return None
    (logdir / f"diff.{tag}.patch").write_text(diff)
    p = prompt_for("reviewer", issue, f"\n## The diff\n```diff\n{diff[:120000]}\n```\n")

    results: dict[int, str] = {}

    def one(idx: int, agent: str) -> None:
        _, _, text = invoke(agent, wt, p, logdir, f"review{idx}.{tag}", role="reviewer")
        results[idx] = text

    # Cross-model on purpose: two instances of one model reviewing a diff behave
    # closer to one reviewer than to two. Once codex is out of quota both become
    # Opus, losing vendor diversity but keeping independent contexts.
    threads = [threading.Thread(target=one, args=(1, "claude")),
               threading.Thread(target=one, args=(2, "codex"))]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    found = [f"## Reviewer {i}\n{t[-8000:]}" for i, t in sorted(results.items())
             if "VERDICT: BLOCK" in t]
    return "\n\n".join(found) if found else None


def work(issue: gh.Issue) -> None:
    tid = f"{issue.number}"
    logdir = LOGS / tid
    log(f"== #{issue.number}: {issue.title}")
    try:
        wt = make_worktree(tid)
    except RuntimeError as exc:
        gh.block(issue.number, f"Could not create a worktree: {exc}")
        return

    # --- critic: is this issue worth implementing at all?
    used, _, critique = invoke(issue.agent, wt, prompt_for("critic", issue),
                               logdir, "critic", role="critic")
    if "VERDICT: REVISE" in critique:
        log(f"    ?? #{issue.number} questioned by the critic")
        record("critic_revise", issue=issue.number, title=issue.title,
               excerpt=critique[-1500:])
        gh.question(issue.number, critique[-40000:])
        git(["worktree", "remove", "--force", str(wt)])
        return
    if "VERDICT: PROCEED" not in critique:
        log(f"    .. #{issue.number} critic gave no verdict; proceeding")

    failure: str | None = None
    agents = ["codex", "claude"] if issue.agent == "duel" else [issue.agent]

    for attempt in range(1, MAX_ATTEMPTS + 1):
        agent = agents[(attempt - 1) % len(agents)]
        extra = ""
        if failure:
            extra = ("\n# The previous attempt FAILED\nFix the root cause; do not "
                     f"paper over it.\n\n```\n{failure[-6000:]}\n```\n")
        invoke(agent, wt, prompt_for("implementer", issue, extra), logdir,
               f"impl.{attempt}", role="implementer",
               escalate=(attempt >= MAX_ATTEMPTS))

        rc, tail = run(["bash", str(GATE), issue.gate], wt, GATE_TIMEOUT,
                       logdir / f"gate.{attempt}.log")
        if rc != 0:
            log(f"    xx gate failed (#{issue.number} attempt {attempt})")
            record("gate_fail", issue=issue.number, attempt=attempt, agent=agent,
                   gate=issue.gate, tail=tail[-1500:])
            failure = tail
            continue

        log(f"    ok gate passed (#{issue.number} attempt {attempt}) — review")
        blocking = review_stage(issue, wt, logdir, str(attempt))
        if blocking:
            log(f"    xx review BLOCKED #{issue.number}")
            record("review_block", issue=issue.number, attempt=attempt,
                   excerpt=blocking[-1500:])
            invoke(agent, wt, prompt_for("fixer", issue, f"\n{blocking}\n"), logdir,
                   f"fix.{attempt}", role="fixer")
            rc, tail = run(["bash", str(GATE), issue.gate], wt, GATE_TIMEOUT,
                           logdir / f"gate.fix.{attempt}.log")
            if rc != 0:
                failure = f"Fixer broke the gate:\n{tail}"
                continue
            if review_stage(issue, wt, logdir, f"post{attempt}"):
                failure = "Reviewers still blocking after the fix pass."
                continue

        if merge_worktree(tid, logdir, issue.gate):
            _, sha = git(["rev-parse", "--short", "HEAD"])
            gh.close(issue.number,
                     f"Merged as `{sha}` after {attempt} attempt(s), gate `{issue.gate}`, "
                     f"and two adversarial reviews.")
            record("merged", issue=issue.number, title=issue.title,
                   attempts=attempt, agent=agent, gate=issue.gate)
            git(["worktree", "remove", "--force", str(wt)])
            return
        failure = "Passed gate and review but could not merge (rebase conflict or "\
                  "gate regression against latest main). Re-sync and redo."

    record("blocked", issue=issue.number, title=issue.title,
           attempts=MAX_ATTEMPTS, tail=(failure or "")[-1500:])
    gh.block(issue.number, f"Failed {MAX_ATTEMPTS} attempts. Last failure:\n\n```\n{failure}\n```")


# --- commands ----------------------------------------------------------------
def cmd_seed() -> int:
    gh.ensure_labels()
    wt = ROOT
    p = "\n".join([(PROMPTS / "shared.md").read_text(),
                   (PROMPTS / "planner.md").read_text()])
    invoke("claude", wt, p, LOGS / "seed", "seed", role="planner")
    issues = gh.fetch(label="fr:ready")
    log(f"seeded: {len(issues)} ready issues")
    for i in issues:
        print(f"  #{i.number:<4} [{i.gate}/{i.agent}] {i.title}  deps={i.deps}")
    return 0 if issues else 1


def retrospective(cycle: int) -> None:
    """Diagnose the loop, not the issues, and file fixes for itself."""
    if not JOURNAL.exists():
        return
    log(f"== retrospective (cycle {cycle})")
    p = "\n".join([(PROMPTS / "shared.md").read_text(),
                    (PROMPTS / "retrospective.md").read_text(),
                    f"\n# This is cycle {cycle}. Write to "
                    f"orchestrator/logs/retro-{cycle}.md\n"])
    invoke("claude", ROOT, p, LOGS / "retro", f"cycle{cycle}", role="critic")
    record("retrospective", cycle=cycle)


def cmd_retro() -> int:
    retrospective(int(os.environ.get("FR_CYCLE", "0")))
    return 0


def cmd_run() -> int:
    gh.ensure_labels()
    log(f"loop start: parallel={MAX_PARALLEL}, wallclock={WALLCLOCK_LIMIT}s")
    cycle = 0
    with ThreadPoolExecutor(max_workers=MAX_PARALLEL) as pool:
        while True:
            if time.time() - _start > WALLCLOCK_LIMIT:
                log("!! wallclock limit reached, stopping")
                break
            with _claim_lock:
                batch = [i for i in gh.claimable() if gh.claim(i.number)][:MAX_PARALLEL]
            if not batch:
                remaining = gh.fetch(label="fr:ready")
                if not remaining:
                    log("queue drained")
                    break
                log(f"waiting: {len(remaining)} ready but dependencies unmet")
                time.sleep(30)
                continue
            for f in [pool.submit(work, i) for i in batch]:
                f.result()
            cycle += 1
            # Retrospect between cycles, while the evidence is fresh and there
            # is still queue left for the fixes to affect.
            if cycle % RETRO_EVERY == 0:
                retrospective(cycle)

    retrospective(cycle + 1)   # final pass over the whole run
    blocked = gh.fetch(label="fr:blocked")
    questioned = gh.fetch(label="fr:questioned")
    log(f"loop end: blocked={[i.number for i in blocked]} "
        f"questioned={[i.number for i in questioned]}")
    return 0 if not blocked else 1


def cmd_status() -> int:
    for label in ("fr:ready", "fr:claimed", "fr:blocked", "fr:questioned"):
        items = gh.fetch(label=label)
        print(f"\n{label} ({len(items)})")
        for i in items:
            print(f"  #{i.number:<4} {i.title}  deps={i.deps or '-'}")
    closed = gh.closed_numbers()
    print(f"\nclosed: {len(closed)}")
    return 0


def main() -> int:
    cmd = sys.argv[1] if len(sys.argv) > 1 else "run"
    return {"run": cmd_run, "seed": cmd_seed, "status": cmd_status,
            "retro": cmd_retro}.get(
        cmd, lambda: (print(__doc__), 2)[1])()


if __name__ == "__main__":
    sys.exit(main())
