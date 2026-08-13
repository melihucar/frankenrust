#!/usr/bin/env python3
"""
FrankenRust build loop.

Unattended orchestrator. GitHub Issues are the queue; agents drain them in
parallel git worktrees. Work merges only if it passes the verification gate,
which agents are forbidden to weaken.

Per issue:

    claim ─► critic ─┬─ REVISE ─► resolver ─┬─ REWRITE ─► re-scope, requeue
                     │                      ├─ CLOSE   ─► killed, with evidence
                     │                      └─ PROCEED ─┐
                     └─ PROCEED ────────────────────────┴─► implementer
                            └─► gate ─┬─ fail ─► retry (<=3)
                                      └─ pass ─► 2 adversarial
                                           reviewers ─┬─ BLOCK ─► fixer
                                                      └─ PASS ─► merge, close

The critic stage exists because the issues are written by agents, not by a
human who read the code. An agent that faithfully implements a wrong issue
produces work that passes the gate and looks like progress.

The resolver exists because there is no human on call. Parking a contested
issue as fr:questioned assumes someone will come back and re-scope it; nobody
will, so the objection has to be adjudicated by another agent instead.

After every merge a retrospective reads logs/events.jsonl and files its own
fixes -- including to this file, which the loop then re-execs into at a batch
boundary. scripts/check_orchestrator.py is what keeps that survivable.

    python3 orchestrator/loop.py seed      # planner agent files the initial issues
    python3 orchestrator/loop.py run       # drain the queue
    python3 orchestrator/loop.py status    # what is open / claimed / blocked
"""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import shutil
import sys
import threading
import time
import traceback
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
# The retrospective is triggered by merges, not by a cycle count -- see
# retro_thread(). A cycle that merges nothing produces no new evidence.
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
    "resolver": os.environ.get("FR_MODEL_RESOLVE", "claude-opus-5"),
}
# How many times an issue may be re-scoped before the resolver must decide it
# outright. Without a cap, critic and resolver can hand an issue back and forth
# forever and it never gets built.
MAX_REVISIONS = int(os.environ.get("FR_MAX_REVISIONS", "2"))
# Consecutive empty polls before believing the queue is finished. GitHub's
# issue list lags writes by seconds, and one empty read ends the whole run.
DRAIN_CONFIRMATIONS = int(os.environ.get("FR_DRAIN_CONFIRMATIONS", "3"))
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
                                env=agent_env())
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


def agent_env() -> dict[str, str]:
    """Environment for agents and the gate.

    This process is not an interactive shell, so it never sourced the profile
    that puts rustup's ~/.cargo/bin (and homebrew) on PATH. Agents inherit that
    gap, and every cargo command then fails as "command not found" -- which the
    gate reports as the agent's code being broken, three attempts per issue,
    all night.
    """
    env = {**os.environ, "FRANKENRUST_AGENT": "1"}
    have = env.get("PATH", "").split(os.pathsep)
    extra = [str(Path.home() / ".cargo" / "bin"), "/opt/homebrew/bin", "/usr/local/bin"]
    add = [p for p in extra if p not in have and Path(p).is_dir()]
    if add:
        env["PATH"] = os.pathsep.join([*add, *have])
    return env


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
                                    env=agent_env())
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


def retire_worktree(tid: str) -> None:
    """Free the disk without losing the evidence. Safe to call twice.

    Agent work is only committed at merge time, so an abandoned worktree holds
    the only copy of a failed attempt -- and also its target/ dir, which for a
    Rust workspace that bindgens PHP runs to gigabytes. Leaving those behind
    fills the disk partway through an overnight run, after which every
    remaining issue fails for a reason that has nothing to do with its code.
    So: commit to the branch, which costs nothing and stays inspectable with
    `git checkout issue/<n>`, then drop the working copy.
    """
    wt = WORKTREES / tid
    if not wt.exists():
        return
    git(["add", "-A"], cwd=wt)
    git(["commit", "-m", f"{tid}: abandoned attempt, never merged"], cwd=wt)
    git(["worktree", "remove", "--force", str(wt)])
    shutil.rmtree(wt, ignore_errors=True)


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
def worktree_diff(wt: Path) -> str:
    """Everything the agent changed -- committed or not, tracked or not.

    `git diff main...HEAD` shows only what was committed, but agents are not
    required to commit: merge_worktree does `add -A` for them at the end. So an
    agent that edits files and stops leaves HEAD where it was, and the diff the
    reviewers are handed is whatever the *previous* stage happened to commit --
    or nothing at all. Both failures point the same way: code merges that no
    reviewer ever read. Staging first also picks up new files, which `git diff`
    cannot see while they are untracked and which are usually the substance of
    the change rather than a detail of it.

    Staging is safe to do repeatedly here; merge_worktree and retire_worktree
    both stage everything again before they commit.
    """
    git(["add", "-A"], cwd=wt)
    _, base = git(["merge-base", "main", "HEAD"], cwd=wt)
    _, diff = git(["diff", "--cached", base.strip()], cwd=wt)
    return diff


def review_stage(issue: gh.Issue, wt: Path, logdir: Path, tag: str) -> str | None:
    """Two adversarial reviewers, independent contexts, diff only."""
    diff = worktree_diff(wt)
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


def resolve_question(issue: gh.Issue, wt: Path, logdir: Path, critique: str) -> bool:
    """Adjudicate a critic's objection. Returns True to implement anyway.

    Without this, `VERDICT: REVISE` is where work goes to die: the issue gets
    parked as fr:questioned and waits for a human who is not coming. The
    resolver has to research the objection against the code and upstream and
    come back with a decision -- re-scope it, kill it, or overrule the critic.
    Parking is not one of the options.
    """
    rounds = issue.revisions
    extra = f"\n# The critic's objection\n\n{critique[-8000:]}\n"
    if rounds >= MAX_REVISIONS:
        extra += (f"\n# This issue has already been re-scoped {rounds} times.\n"
                  "REWRITE is no longer available to you. Decide PROCEED or "
                  "CLOSE, and justify it.\n")
    _, _, out = invoke("claude", wt, prompt_for("resolver", issue, extra),
                       logdir, f"resolve.{rounds}", role="resolver")

    if "RESOLUTION: PROCEED" in out:
        gh.comment(issue.number, f"**Resolver: the objection does not hold.**\n\n{out[-30000:]}")
        record("resolved", issue=issue.number, decision="proceed", round=rounds)
        return True
    if "RESOLUTION: CLOSE" in out:
        gh.close(issue.number, f"**Resolver: this issue should not be built.**\n\n{out[-30000:]}")
        record("resolved", issue=issue.number, decision="close", round=rounds)
        return False
    if "RESOLUTION: REWRITE" in out and rounds < MAX_REVISIONS:
        if gh.requeue(issue.number, rounds + 1):
            gh.comment(issue.number, f"**Resolver: re-scoped, back in the queue.**\n\n{out[-30000:]}")
            record("resolved", issue=issue.number, decision="rewrite", round=rounds + 1)
            return False
        record("resolve_failed", issue=issue.number, reason="requeue failed")

    # No usable decision. Park it rather than spin -- but this is a loop defect,
    # so make sure the retrospective sees it.
    log(f"    !! #{issue.number} resolver gave no decision; parking")
    record("resolve_failed", issue=issue.number, round=rounds, excerpt=out[-1500:])
    gh.question(issue.number, critique[-40000:])
    return False


def work(issue: gh.Issue) -> None:
    """Process one issue. Never raises: the pool re-raises into cmd_run.

    A crash here used to propagate through f.result(), out of the executor and
    out of the run -- one unlucky issue ending the night and leaving itself in
    fr:claimed, which nobody is awake to release.
    """
    tid = f"{issue.number}"
    logdir = LOGS / tid
    log(f"== #{issue.number}: {issue.title}")
    try:
        make_worktree(tid)
    except RuntimeError as exc:
        gh.block(issue.number, f"Could not create a worktree: {exc}")
        return
    try:
        _work(issue, tid, WORKTREES / tid, logdir)
    except Exception as exc:  # noqa: BLE001
        log(f"    !! #{issue.number} crashed the worker: {exc}")
        record("work_crash", issue=issue.number, title=issue.title,
               reason=repr(exc), trace=traceback.format_exc()[-2000:])
        gh.block(issue.number, "The loop crashed while processing this issue.\n\n"
                               f"```\n{traceback.format_exc()[-20000:]}\n```")
    finally:
        retire_worktree(tid)


def _work(issue: gh.Issue, tid: str, wt: Path, logdir: Path) -> None:
    # --- critic: is this issue worth implementing at all?
    used, _, critique = invoke(issue.agent, wt, prompt_for("critic", issue),
                               logdir, "critic", role="critic")
    if "VERDICT: REVISE" in critique:
        log(f"    ?? #{issue.number} questioned by the critic")
        record("critic_revise", issue=issue.number, title=issue.title,
               excerpt=critique[-1500:])
        gh.comment(issue.number,
                   f"**The critic challenged this issue.**\n\n{critique[-40000:]}")
        if not resolve_question(issue, wt, logdir, critique):
            return
        log(f"    -> #{issue.number} objection overruled; implementing")
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
        # A gate that passes on an empty worktree says nothing: the bootstrap
        # profile is satisfiable by changing nothing at all. Without this the
        # run's easiest path to "merged" is to do no work -- empty diff, review
        # skipped for want of anything to read, an --allow-empty commit, issue
        # closed. Spend the attempt instead.
        if not worktree_diff(wt).strip():
            log(f"    xx #{issue.number} attempt {attempt} changed nothing")
            record("empty_diff", issue=issue.number, attempt=attempt, agent=agent)
            failure = ("You changed no files. The gate passing means only that you "
                       "broke nothing; it is not evidence of work.")
            continue

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
            _merge_signal.set()
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


def retrospective(cycle: int | str) -> None:
    """Diagnose the loop, not the issues, and file fixes for itself."""
    if not JOURNAL.exists():
        return
    log(f"== retrospective ({cycle})")
    p = "\n".join([(PROMPTS / "shared.md").read_text(),
                    (PROMPTS / "retrospective.md").read_text(),
                    f"\n# This is retrospective {cycle}. Write to "
                    f"orchestrator/logs/retro-{cycle}.md\n"])
    invoke("claude", ROOT, p, LOGS / "retro", f"r{cycle}", role="critic")
    record("retrospective", cycle=cycle)


def cmd_retro() -> int:
    retrospective(int(os.environ.get("FR_CYCLE", "0")))
    return 0


# A merge is the only event that produces new evidence, so it is the only thing
# worth retrospecting on. This runs on its own thread: doing it inline would
# stall a worker for the length of an Opus call, and doing it once per batch
# would skip merges whenever several land together.
_merge_signal = threading.Event()
_retro_stop = threading.Event()


def retro_thread() -> None:
    n = 0
    while not _retro_stop.is_set():
        if not _merge_signal.wait(timeout=5):
            continue
        _merge_signal.clear()          # cleared before the run, so merges that
        n += 1                         # land during it trigger another pass
        try:
            retrospective(n)
        except Exception as exc:       # never let the retro kill the run
            log(f"!! retrospective failed: {exc}")
            record("retro_error", reason=str(exc))


def _source_fingerprint() -> str:
    h = hashlib.sha256()
    for rel in ("orchestrator/loop.py", "orchestrator/gh.py"):
        h.update((ROOT / rel).read_bytes())
    return h.hexdigest()


_source_at_start = _source_fingerprint()


def self_update_pending() -> bool:
    """Does the code on disk still match the code this process is running?

    Was `git log -1 --name-only HEAD`, which only sees the most recent commit.
    A batch that merges two issues, the loop.py one first, leaves HEAD on the
    other and the update is silently skipped -- and the same is true of a fix
    committed to main directly while the run is live. Comparing the bytes we
    started with against the bytes on disk asks the question that actually
    matters and cannot miss it, whatever put them there.
    """
    return _source_fingerprint() != _source_at_start


def restart_into_new_code() -> None:
    """Re-exec so a merged change to the loop actually takes effect.

    The loop cannot edit itself while running -- Python has the old module in
    memory -- but it can hand over to a new process. All state lives in GitHub
    issues, so the successor rebuilds it and carries on. The gate already
    proved the new code parses and runs, so this is not a leap of faith.
    """
    log("== self-update merged; restarting into the new code")
    record("self_restart", head=git(["rev-parse", "--short", "HEAD"])[1].strip())
    _retro_stop.set()
    sys.stdout.flush()
    sys.stderr.flush()
    # Hand the remaining wallclock to the successor so repeated restarts cannot
    # extend the run past the budget it was given.
    left = max(int(WALLCLOCK_LIMIT - (time.time() - _start)), 60)
    env = {**os.environ, "FR_WALLCLOCK": str(left),
           "FR_RESTARTS": str(int(os.environ.get("FR_RESTARTS", "0")) + 1)}
    os.execve(sys.executable,
              [sys.executable, str(Path(__file__).resolve()), "run"], env)


def cmd_run() -> int:
    gh.ensure_labels()
    restarts = int(os.environ.get("FR_RESTARTS", "0"))
    log(f"loop start: parallel={MAX_PARALLEL}, wallclock={WALLCLOCK_LIMIT}s"
        f"{f', restart #{restarts}' if restarts else ''}")
    # Nothing is running yet, so anything still fr:claimed is residue from a
    # crash or a self-restart. Left alone it stays claimed forever and there is
    # nobody to notice.
    for i in gh.fetch(label="fr:claimed"):
        gh.release(i.number)
        record("reclaimed", issue=i.number, title=i.title)
        log(f"reclaimed stale claim #{i.number}")

    retro = threading.Thread(target=retro_thread, daemon=True)
    retro.start()
    empty = 0
    with ThreadPoolExecutor(max_workers=MAX_PARALLEL) as pool:
        while True:
            if time.time() - _start > WALLCLOCK_LIMIT:
                log("!! wallclock limit reached, stopping")
                break
            with _claim_lock:
                # Claim lazily, up to the batch size. Claiming everything and
                # then slicing strands the remainder in fr:claimed with no
                # worker and no human -- claimable() then reads empty and the
                # run ends early believing the queue is drained.
                batch: list[gh.Issue] = []
                for cand in gh.claimable():
                    if len(batch) >= MAX_PARALLEL:
                        break
                    if gh.claim(cand.number):
                        batch.append(cand)
            if not batch:
                remaining = gh.fetch(label="fr:ready")
                if remaining:
                    empty = 0
                    log(f"waiting: {len(remaining)} ready but dependencies unmet")
                    time.sleep(30)
                    continue
                # GitHub's issue list is eventually consistent -- a queue seeded
                # seconds ago reads back empty. Quitting on the first empty poll
                # would end the run before it started.
                empty += 1
                if empty >= DRAIN_CONFIRMATIONS:
                    log("queue drained")
                    break
                log(f"queue reads empty ({empty}/{DRAIN_CONFIRMATIONS}); confirming")
                time.sleep(20)
                continue
            empty = 0
            # Disk pressure looks exactly like bad code from inside the gate:
            # cargo fails, three attempts burn, the issue is blocked. Put it in
            # the journal so the retrospective can name the real cause. The loop
            # does not prune anything itself -- deleting a user's images or
            # caches unattended is not a call it gets to make.
            free_gb = shutil.disk_usage(ROOT).free / 1e9
            if free_gb < 10:
                log(f"!! {free_gb:.1f}GB free — builds may fail for lack of disk, not for lack of correctness")
                record("low_disk", free_gb=round(free_gb, 1))
            for f in [pool.submit(work, i) for i in batch]:
                f.result()
            # Only at a batch boundary, with no agent mid-flight: swapping the
            # code out from under a running worker would orphan its worktree.
            if self_update_pending():
                restart_into_new_code()

    # Stop the thread first, then retrospect synchronously: racing a final
    # set() against the stop flag would silently skip the whole-run pass.
    _retro_stop.set()
    retro.join(timeout=AGENT_TIMEOUT)
    retrospective("final")
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
