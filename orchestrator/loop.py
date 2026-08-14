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
import re
import subprocess
import shutil
import sys
import threading
import time
import traceback
from concurrent.futures import FIRST_COMPLETED, Future, ThreadPoolExecutor, wait
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


def gate_for(wt: Path) -> str:
    """The gate script belonging to `wt`, not the one in the main checkout.

    gate.sh's second line is `cd "$(dirname "$0")/.."`, which is correct for a
    human running it and fatal here: passing the main checkout's copy makes it
    cd back to the main checkout and discard the cwd the loop set. Every gate
    then validated main -- which was always healthy -- instead of the agent's
    work, so a green gate proved nothing about the diff about to merge.

    It stayed invisible for a whole run because nothing needed a file that
    existed only in a worktree: bootstrap skips the build, and the docs and
    corpus checks happened to pass against main's content. The first issue to
    create a Cargo workspace failed three times on `Cargo.toml missing` with
    the file sitting committed in its worktree, and was blocked with 1,602
    working lines.
    """
    own = wt / "scripts" / "gate.sh"
    return str(own if own.exists() else GATE)

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
    "unblocker": os.environ.get("FR_MODEL_UNBLOCK", "claude-opus-5"),
}
# How many times an issue may be re-scoped before the resolver must decide it
# outright. Without a cap, critic and resolver can hand an issue back and forth
# forever and it never gets built.
MAX_REVISIONS = int(os.environ.get("FR_MAX_REVISIONS", "2"))
# How many times an issue may be rescued out of fr:blocked. Separate budget from
# MAX_REVISIONS: a bad spec and three failed gates are different failures, and
# sharing one counter means an issue the resolver already re-scoped gets fewer
# rescues than one it never touched.
MAX_RECOVERIES = int(os.environ.get("FR_MAX_RECOVERIES", "2"))
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
# Survives re-exec. A quota wall lasts days, but the flag was process-local, so
# every self-update handed the successor a clean slate and it re-learned the
# same wall by burning another invocation on it.
_codex_disabled = os.environ.get("FR_CODEX_DISABLED") == "1"
# Issues whose recovery budget is spent, so the terminal log line is printed
# once rather than on every 30s poll.
_recovery_exhausted: set[int] = set()
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


def _torn_journal_offset() -> int | None:
    """Byte length of JOURNAL if it ends mid-record, else None.

    A short write -- ENOSPC is the realistic trigger, and this loop already
    emits `low_disk` because the disk does fill -- leaves the file mid-token
    with no trailing newline. Appending onto that in the usual "a" mode would
    weld the *next*, complete record onto the tail of the broken one, so a
    line-by-line reader discards both instead of just the one that was
    already lost.
    """
    if not JOURNAL.exists():
        return None
    size = JOURNAL.stat().st_size
    if size == 0:
        return None
    with JOURNAL.open("rb") as fh:
        fh.seek(-1, os.SEEK_END)
        if fh.read(1) == b"\n":
            return None
    return size


def record(event: str, **fields) -> None:
    """Append a structured event.

    The retrospective reads THIS, not the agent transcripts. Transcripts are
    enormous and unstructured; asking a model to find systemic patterns in them
    produces impressions rather than findings. One JSON line per stage outcome
    makes "the critic rejected 4 issues for the same reason" a countable fact.
    """
    rec = {"ts": now(), "event": event, **fields}
    LOGS.mkdir(parents=True, exist_ok=True)
    with _journal_lock:
        tear = _torn_journal_offset()
        # One open()/close() per call, not one write(): close() is what
        # flushes to disk, and restart_into_new_code() hands off via
        # os.execve, which replaces the process image without running
        # Python's normal buffer flush. A write that outlived close() would
        # silently vanish, so everything below must land before this `with`
        # block exits.
        with JOURNAL.open("a") as fh:
            if tear is not None:
                # Isolate the fragment on its own line before anything else
                # touches the file, then say so in the journal itself -- a
                # hole the journal reports is recoverable evidence.
                fh.write("\n")
                fh.write(json.dumps({"ts": now(), "event": "journal_torn",
                                      "offset": tear}) + "\n")
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
    os.environ["FR_CODEX_DISABLED"] = "1"    # carried through restart_into_new_code
    log(f"!! codex disabled for the rest of this run ({reason}); falling back to claude")
    # Cross-model review is the reason two reviewers are worth more than one
    # reviewer run twice. Losing it is a change in what a merge means, so it
    # belongs in the journal where the retrospective and the morning review
    # will see it, not only in a log line nobody reads.
    record("review_diversity_lost", reason=reason)


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
def agent_cmd(agent: str, model: str | None,
              last_message: Path | None = None) -> list[str]:
    """Sandboxed as tightly as the work allows: these run for hours unwatched."""
    if agent == "codex":
        # -o is codex's answer to claude's `result` event: the final message,
        # alone, in a file. Without it the only record of what codex decided is
        # its transcript -- which opens with a verbatim echo of the prompt, and
        # our prompts necessarily quote the verdict tokens they ask for. See
        # _final_text().
        out = ["-o", str(last_message)] if last_message else []
        if os.environ.get("FR_YOLO") == "1":
            return ["codex", "exec", "--dangerously-bypass-approvals-and-sandbox",
                    "--skip-git-repo-check", *out, "-"]
        return ["codex", "exec", "-s", "workspace-write",
                "-c", "sandbox_workspace_write.network_access=true",
                "-c", 'approval_policy="never"', "--skip-git-repo-check", *out, "-"]
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


def _final_text(logpath: Path, agent: str,
                last_message: Path | None = None) -> tuple[str, str]:
    """(final message, error reason) from an agent log.

    Only the *final* message may feed verdict parsing. The full stream contains
    reasoning and tool calls, and a reviewer musing "this is not a VERDICT:
    BLOCK situation" would otherwise block a clean diff.

    That rule used to be enforced for claude only -- "codex writes plain text,
    so it passes through" -- and passing the transcript through is what killed
    #8, #11 and #20. `codex exec` opens its log with a verbatim echo of the
    prompt, and prompts/reviewer.md necessarily contains the line "`VERDICT:
    BLOCK` -- you found at least one defect". So `"VERDICT: BLOCK" in text` was
    true of every codex review ever run, whatever codex actually concluded:
    #8 passed the gate and drew PASS from all six reviews across three attempts
    and was still parked as fr:blocked. The same echo made every codex critic
    return VERDICT: REVISE (12 recorded, at ~25 minutes of resolver each).
    Nothing merged while codex was reachable; the seven that did all landed in
    windows where it was quota-walled and both reviewers were claude.

    So codex now gets read the same way claude does: `-o` writes the last
    message to `last_message`, and only that file is parsed. The transcript
    fallback covers a codex too old for `-o` by taking what follows the final
    "tokens used" marker, which is `codex exec`'s own restatement of the last
    message. If neither is readable we return no text and an error: output we
    cannot read is not evidence of a defect, and treating it as one is the bug
    being fixed here.
    """
    if agent != "claude":
        if last_message and last_message.exists():
            final = last_message.read_text(errors="replace").strip()
            if final:
                return final, ""
        if logpath.exists():
            lines = logpath.read_text(errors="replace").splitlines()
            for i in range(len(lines) - 1, -1, -1):
                if lines[i].strip() != "tokens used":
                    continue
                # +2 skips the marker and the token count under it.
                final = "\n".join(lines[i + 2:]).strip()
                if final:
                    return final, ""
                break
        return "", "no final message from codex"
    if not logpath.exists():
        return "", "no log"
    raw = logpath.read_text(errors="replace")
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
    lastmsg = logdir / f"{use}.{tag}.final.txt"
    for _ in range(2):
        logpath = logdir / f"{use}.{tag}.log"
        # Named per agent so the codex run and a claude retry of the same tag
        # cannot read each other's verdict, and unlinked first so a crashed run
        # inherits nothing from the attempt before it.
        lastmsg = logdir / f"{use}.{tag}.final.txt"
        lastmsg.unlink(missing_ok=True)
        log(f"    -> {use}{f'({model})' if model else ''} {tag} ({wt.name})")
        with pf.open("rb") as stdin_fh, logpath.open("ab") as out_fh:
            proc = subprocess.Popen(agent_cmd(use, model, lastmsg), cwd=str(wt),
                                    stdin=stdin_fh, stdout=out_fh,
                                    stderr=subprocess.STDOUT, env=agent_env())
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
        text, err = _final_text(logpath, use, lastmsg)
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
def preserve_branch(branch: str, tid: str) -> None:
    """Tag anything on `branch` that main does not already have, before deletion.

    make_worktree deletes the issue branch and starts again from main, which is
    the right default -- a fresh attempt should not inherit a failed one's tree.
    But the deletion was unconditional, and an issue branch is the ONLY copy of
    an attempt: agents work uncommitted and merge_worktree commits for them, so
    work that failed review exists nowhere else.

    That nearly cost the project its hardest result. #11 failed review three
    times and its branch carried 3,955 lines including `frankenrust-sys/shim.c`
    -- the one sound answer to a Zend bailout crossing a Rust frame, arrived at
    only after the obvious approach was implemented and rejected as UB. The next
    claim of #11 would have deleted it, and the loop would have rediscovered the
    unsound version first, because that is the one the issue described.

    Work becomes durable here by merging. Nothing else was keeping it, and the
    attempts most worth keeping are exactly the ones that failed.
    """
    rc, _ = git(["rev-parse", "--verify", "--quiet", branch])
    if rc != 0:
        return                                   # no such branch, nothing to keep
    # `--is-ancestor` is the question that matters: does main already contain
    # every commit here? Comparing tips would tag branches that are merely
    # behind, filling the tag namespace with duplicates of main.
    rc, _ = git(["merge-base", "--is-ancestor", branch, "main"])
    if rc == 0:
        return                                   # fully merged, safe to delete
    tag = f"attempt/{tid}/{datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ')}"
    rc, out = git(["tag", tag, branch])
    if rc != 0:
        log(f"    !! could not preserve {branch} before deleting it: {out}")
        record("preserve_failed", branch=branch, reason=out[-500:])
        return
    _, n = git(["rev-list", "--count", f"main..{branch}"])
    log(f"    ~~ preserved {branch} as {tag} ({n.strip()} commit(s) not on main)")
    record("branch_preserved", branch=branch, tag=tag, commits=int(n.strip() or 0))


def make_worktree(tid: str) -> Path:
    wt, branch = WORKTREES / tid, f"issue/{tid}"
    if wt.exists():
        git(["worktree", "remove", "--force", str(wt)])
        shutil.rmtree(wt, ignore_errors=True)
    preserve_branch(branch, tid)
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
        rc, _ = run(["bash", gate_for(wt), gate], wt, GATE_TIMEOUT, logdir / "gate.rebase.log")
        if rc != 0:
            log(f"    !! {tid} gate failed after rebase onto main")
            record("merge_regate_fail", issue=tid)
            return False
        rc, out = git(["merge", "--ff-only", branch])
        if rc != 0:
            log(f"    !! {tid} ff merge failed: {out}")
            return False
        log(f"    ++ {tid} merged into main")
        publish_main(tid)
        return True


def publish_main(tid: str) -> None:
    """Get the merge off this machine. Best-effort: never fails a merge.

    The loop merged into a purely local `main` and stopped there, so an eight
    hour unattended run accumulated its entire output in one directory on one
    laptop. Nothing replicated it, and the GitHub view a human checks in the
    morning showed whatever was last pushed by hand -- three hours stale on the
    night this was written, which made the branch list read as "no progress"
    while two merges sat locally.

    A push failure must not undo a merge that already passed the gate and two
    reviewers: no network is a worse reason to discard work than any of the
    reasons we discard work on purpose. So this logs and moves on, and the next
    merge pushes both.
    """
    rc, out = git(["push", "origin", "main"])
    if rc != 0:
        log(f"    !! could not push main after {tid}: {out[-200:]}")
        record("push_failed", issue=tid, reason=out[-500:])
        return
    record("pushed", issue=tid)


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


def snapshot_worktree(wt: Path) -> str:
    """Capture the exact tree state so scratch work can be discarded losslessly.

    `git write-tree` needs the index to reflect the state we want captured, so
    stage first -- that also picks up untracked files, which is exactly what a
    reviewer's scratch crate would otherwise be. write-tree only writes objects
    to the object database; it never touches the working directory, so taking
    this snapshot does not disturb what the reviewers are about to read.
    """
    git(["add", "-A"], cwd=wt)
    rc, tree = git(["write-tree"], cwd=wt)
    if rc != 0:
        log(f"    !! snapshot_worktree failed to write-tree in {wt}: {tree}")
        record("snapshot_failed", worktree=str(wt), reason=tree[-500:])
        return ""
    return tree.strip()


def restore_worktree(wt: Path, tree: str) -> None:
    """Undo everything since the matching snapshot_worktree call.

    Three steps, because none of them is sufficient alone.

    `add -A` stages whatever the reviewers left, so untracked additions enter
    the index -- `read-tree` only reconciles paths the index knows about, so
    without this an added-but-never-`git add`ed file is invisible to it.

    `read-tree --reset -u` resets the index to `tree` and rewrites the working
    tree to match: modified tracked files revert to the snapshot's content, and
    paths in the (freshly staged) index but not in `tree` are deleted.

    `git clean -ffd` collects what read-tree provably cannot. Both of these
    exit 0 today, so neither is caught by checking rc:

      * A reviewer's repro crate carries its own .gitignore -- `cargo new
        --vcs git` writes one containing `/target`. `add -A` skips the build
        output that rule covers, so read-tree never learns of it; read-tree
        then deletes the nested .gitignore, un-ignoring exactly what it was
        hiding, and the next `add -A` stages it for merge. Restoring the tree
        is what opens the leak: without the restore the ignore rule survives
        and holds the artifacts back. Same shape when a reviewer appends a
        rule to the root .gitignore and writes under it.
      * A nested repo (`git init`, `git clone` of upstream to diff against) is
        staged as a 160000 gitlink, not as its contents. read-tree drops the
        gitlink from the index, but git will not delete a repository's working
        directory: it warns "unable to rmdir ...: Directory not empty" and
        exits 0. The directory survives, the next `add -A` re-adds the
        gitlink, and main gains a submodule pointer to a commit that ceases to
        exist the moment retire_worktree deletes the worktree.

    `-ff` is what removes that nested repo; `-d`, untracked directories. No
    `-x`, deliberately: ignored paths are still ignored once the snapshot's
    .gitignore is back, so the root `target/` a reviewer's build populated
    survives -- and being ignored it cannot reach a merge anyway. Nor does
    clean touch anything the *implementer* left: after read-tree the index is
    the snapshot, so implementer paths are tracked, including a nested repo of
    their own, which stays as the gitlink the snapshot recorded.

    Then verify instead of trusting three exit codes, because the whole point
    of the restore is that a leak here is otherwise silent.
    """
    if not tree:
        return
    git(["add", "-A"], cwd=wt)
    rc, out = git(["read-tree", "--reset", "-u", tree], cwd=wt)
    if rc != 0:
        log(f"    !! restore_worktree failed in {wt}: {out}")
        record("restore_failed", worktree=str(wt), reason=out[-500:])
    git(["clean", "-ffd"], cwd=wt)

    git(["add", "-A"], cwd=wt)
    rc, actual = git(["write-tree"], cwd=wt)
    if rc != 0 or actual.strip() != tree:
        _, left = git(["diff", "--cached", "--name-status", tree], cwd=wt)
        log(f"    !! restore_worktree did not fully restore {wt}; "
            f"reviewer residue is headed for the merge:\n{left[-1000:]}")
        record("restore_incomplete", worktree=str(wt), paths=left[-1000:])


SILENT_REVIEW = (
    "No reviewer produced a verdict. Every reviewer run failed, timed out, or "
    "returned unreadable output, so this diff has not been reviewed by anything. "
    "This is a harness failure rather than a finding against your work; the diff "
    "is unchanged and the review will be retried."
)


def review_outcome(results: dict[int, str]) -> tuple[str | None, dict[int, str]]:
    """What the reviewers actually said. Returns (blocking text or None, verdicts).

    A reviewer must SAY it passed. Selecting on `"VERDICT: BLOCK" in t` alone
    read silence as approval, because `"VERDICT: BLOCK" in ""` is False -- so a
    reviewer that timed out, crashed, or hit a quota wall mid-sentence was
    indistinguishable from one that read the diff and approved it. Two dead
    reviewers merged code nothing had looked at, while gh.close() recorded "two
    adversarial reviews (claude + codex)" on the issue.

    Absence of a verdict is not evidence of a pass. It is evidence of nothing,
    which is the one thing a merge gate must never treat as consent.

    Split out of review_stage so it can be tested without a git worktree or a
    live agent: the bug lived in three lines of classification wrapped in
    sixty lines of orchestration, and nothing could reach it.
    """
    def verdict_of(t: str) -> str:
        if "VERDICT: BLOCK" in t:
            return "block"
        if "VERDICT: PASS" in t:
            return "pass"
        return "silent"

    verdicts = {i: verdict_of(t) for i, t in sorted(results.items())}
    blocked = [i for i, v in verdicts.items() if v == "block"]
    if blocked:
        return "\n\n".join(f"## Reviewer {i}\n{results[i][-8000:]}" for i in blocked), verdicts
    # Nobody blocked. That is only a pass if somebody actually reviewed.
    if not any(v == "pass" for v in verdicts.values()):
        return SILENT_REVIEW, verdicts
    return None, verdicts


def review_stage(issue: gh.Issue, wt: Path, logdir: Path, tag: str) -> str | None:
    """Two adversarial reviewers, independent contexts, diff only.

    Reviewers run with cwd=wt and full tool access, and reviewer.md tells them
    to investigate using the repo -- building a repro in the worktree is good
    review work. But whatever they leave behind would otherwise be swept into
    the *next* stage's diff by worktree_diff's `git add -A`, and from there into
    `main` by merge_worktree's. Snapshot the tree before they run and restore it
    unconditionally after, so reviewing can never change what gets merged.
    """
    diff = worktree_diff(wt)
    if not diff.strip():
        return None
    (logdir / f"diff.{tag}.patch").write_text(diff)
    p = prompt_for("reviewer", issue, f"\n## The diff\n```diff\n{diff[:120000]}\n```\n")

    results: dict[int, str] = {}

    def one(idx: int, agent: str) -> None:
        _, _, text = invoke(agent, wt, p, logdir, f"review{idx}.{tag}", role="reviewer")
        results[idx] = text

    snapshot = snapshot_worktree(wt)
    try:
        # Cross-model on purpose: two instances of one model reviewing a diff behave
        # closer to one reviewer than to two. Once codex is out of quota both become
        # Opus, losing vendor diversity but keeping independent contexts.
        threads = [threading.Thread(target=one, args=(1, "claude")),
                   threading.Thread(target=one, args=(2, "codex"))]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
    finally:
        # Unconditional: the PASS path merges this worktree, and BLOCK feeds it
        # to the fixer, then to a post-fix review_stage that stages everything
        # again -- either way, anything reviewer-authored still here gets merged.
        restore_worktree(wt, snapshot)

    blocking, verdicts = review_outcome(results)
    record("review_verdicts", issue=issue.number, tag=tag, verdicts=verdicts)
    if blocking and not any(v == "block" for v in verdicts.values()):
        log(f"    xx #{issue.number} no reviewer produced a verdict ({tag})")
        record("review_silent", issue=issue.number, tag=tag, verdicts=verdicts)
    elif not blocking and any(v == "silent" for v in verdicts.values()):
        log(f"    !! #{issue.number} only "
            f"{sum(v == 'pass' for v in verdicts.values())}/{len(verdicts)} "
            f"reviewers reported ({tag})")
        record("review_incomplete", issue=issue.number, tag=tag, verdicts=verdicts)
    return blocking


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


def recover_blocked() -> int:
    """Rescue blocked issues that other open work is waiting on.

    The counterpart to resolve_question(), for the other absorbing state. A bad
    *spec* was adjudicated and re-scoped; a bad *implementation attempt* was
    parked in fr:blocked and waited for a human who is not coming.

    Triggered by what a block gates, not by an empty queue. When #5 parked at
    02:30 the loop had fourteen claimable issues and never starved -- it stayed
    busy on housekeeping it had filed for itself while every port issue sat
    behind a label nothing removes. Starvation would have been the wrong signal;
    by the time it fired the night would have been over.

    Returns how many issues it returned to the queue, so the caller can poll
    again immediately rather than sleeping on a queue that just changed.
    """
    recovered = 0
    for issue, waiting in gh.blocked_needing_recovery():
        if issue.recoveries >= MAX_RECOVERIES:
            # Say this once. The poll that calls us re-runs every 30s while the
            # queue is waiting on dependencies, which is exactly the state a
            # spent budget produces, so logging it per poll buries the run.
            if issue.number not in _recovery_exhausted:
                _recovery_exhausted.add(issue.number)
                log(f"    -- #{issue.number} blocked, {len(waiting)} waiting, "
                    f"recovery budget spent ({issue.recoveries}) — this is terminal")
                record("recovery_exhausted", issue=issue.number,
                       gating=len(waiting), recoveries=issue.recoveries)
            continue
        log(f"    ~~ recovering #{issue.number} (gates {len(waiting)}: {waiting[:8]})")
        tid = f"unblock-{issue.number}"
        logdir = LOGS / str(issue.number)
        try:
            wt = make_worktree(tid)
        except RuntimeError as exc:
            record("recover_failed", issue=issue.number, reason=f"worktree: {exc}")
            continue
        try:
            # The transcripts are the evidence, and they live in the main
            # checkout's logs/ -- not in the worktree the unblocker is standing
            # in. Say so explicitly or it reads an empty directory and concludes
            # the failure left no trace.
            extra = (f"\n# What this issue is holding up\n\n"
                     f"{len(waiting)} open issue(s) depend on it: "
                     f"{', '.join(f'#{n}' for n in waiting)}\n\n"
                     f"# Its transcripts\n\n`{logdir}` in the main checkout "
                     f"(absolute path; you are in a worktree).\n"
                     f"Rescue {issue.recoveries + 1} of {MAX_RECOVERIES}.\n")
            _, _, out = invoke("claude", wt, prompt_for("unblocker", issue, extra),
                               logdir, f"unblock.{issue.recoveries}", role="unblocker")

            if "RECOVERY: CLOSE" in out:
                gh.close(issue.number,
                         f"**Unblocker: this issue should not be built.**\n\n{out[-30000:]}")
                record("recovered", issue=issue.number, decision="close",
                       gating=len(waiting))
                continue
            if "RECOVERY: SPLIT" in out or "RECOVERY: REQUEUE" in out:
                decision = "split" if "RECOVERY: SPLIT" in out else "requeue"
                if gh.unblock(issue.number, issue.recoveries + 1):
                    gh.comment(issue.number,
                               f"**Unblocker: {decision}, back in the queue.**\n\n{out[-30000:]}")
                    record("recovered", issue=issue.number, decision=decision,
                           gating=len(waiting), round=issue.recoveries + 1)
                    recovered += 1
                    continue
                record("recover_failed", issue=issue.number, reason="unblock failed")
                continue

            # No decision. Leave it blocked -- but this is a loop defect, and an
            # unblocker that returns nothing is how the absorbing state comes
            # back wearing a different label.
            log(f"    !! #{issue.number} unblocker gave no decision")
            record("recover_failed", issue=issue.number, round=issue.recoveries,
                   excerpt=out[-1500:])
        except Exception as exc:  # noqa: BLE001
            log(f"    !! recovery of #{issue.number} crashed: {exc}")
            record("recover_failed", issue=issue.number, reason=repr(exc),
                   trace=traceback.format_exc()[-2000:])
        finally:
            retire_worktree(tid)
    return recovered


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
    #
    # Bracketed for the same reason review_stage is. The critic and the resolver
    # both run with cwd=wt and full tool access, and both are told to research
    # against the code -- so both can leave files behind, and anything still
    # here rides the implementer's `git add -A` onto main. #24 fixed this for
    # reviewers only, which read as fixing the class and did not: these two run
    # BEFORE the implementer, so their scratch is indistinguishable from work
    # the implementer did and no reviewer has any reason to question it.
    critic_snapshot = snapshot_worktree(wt)
    try:
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
    finally:
        restore_worktree(wt, critic_snapshot)

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

        rc, tail = run(["bash", gate_for(wt), issue.gate], wt, GATE_TIMEOUT,
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
            # Head, not tail. review_outcome puts "## Reviewer N" at the front of
            # each finding, so a trailing slice cut off the one field that says
            # WHICH reviewer blocked -- and the retrospective, which reads only
            # the journal, could not tell one reviewer overruling a peer's PASS
            # from both agreeing. Eleven of twenty-one blocks last run were the
            # former and nothing could count them.
            record("review_block", issue=issue.number, attempt=attempt,
                   phase="initial", excerpt=blocking[:1500])
            invoke(agent, wt, prompt_for("fixer", issue, f"\n{blocking}\n"), logdir,
                   f"fix.{attempt}", role="fixer")
            rc, tail = run(["bash", gate_for(wt), issue.gate], wt, GATE_TIMEOUT,
                           logdir / f"gate.fix.{attempt}.log")
            if rc != 0:
                failure = f"Fixer broke the gate:\n{tail}"
                continue
            # Forward what the post-fix reviewers actually said. Calling this in
            # boolean context threw the findings away and handed the next
            # attempt the fixed string "Reviewers still blocking after the fix
            # pass" -- which names no defect, so attempts 2 and 3 re-derived
            # from scratch what attempt 1 had already been told, or guessed.
            still_blocking = review_stage(issue, wt, logdir, f"post{attempt}")
            if still_blocking:
                record("review_block", issue=issue.number, attempt=attempt,
                       phase="post-fix", excerpt=still_blocking[:1500])
                failure = ("The fixer's changes did not satisfy the reviewers. "
                           f"Their findings on the FIXED diff:\n\n{still_blocking}")
                continue

        if merge_worktree(tid, logdir, issue.gate):
            _, sha = git(["rev-parse", "--short", "HEAD"])
            # Say which reviewers actually ran. "Two adversarial reviews" reads
            # as cross-model review whether or not that is what happened, and
            # once codex is walled off it is two runs of one model -- a weaker
            # claim that the morning reader deserves to see on the issue.
            reviewed = ("two adversarial reviews (claude + codex)" if codex_ok()
                        else "two adversarial reviews, both claude — codex was unavailable")
            gh.close(issue.number,
                     f"Merged as `{sha}` after {attempt} attempt(s), gate `{issue.gate}`, "
                     f"and {reviewed}.")
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


def _retro_artifacts(cycle: int) -> list[Path]:
    return [
        LOGS / f"retro-{cycle}.md",
        LOGS / "retro" / f"prompt.r{cycle}.md",
        LOGS / "retro" / f"claude.r{cycle}.log",
        LOGS / "retro" / f"claude.r{cycle}.final.txt",
    ]


_RETRO_ARTIFACT_PATTERNS = [
    (lambda: LOGS, re.compile(r"^retro-(\d+)\.md$")),
    (lambda: LOGS / "retro", re.compile(r"^prompt\.r(\d+)\.md$")),
    (lambda: LOGS / "retro", re.compile(r"^claude\.r(\d+)\.log$")),
    (lambda: LOGS / "retro", re.compile(r"^claude\.r(\d+)\.final\.txt$")),
]


def _highest_retro_cycle_claimed() -> int:
    """The largest cycle number with any artifact on disk, or 0 if none.

    `_claim_retro_cycle` reserves a cycle by creating its report file before
    the agent that fills it in has run. If that pass is the one
    `restart_into_new_code()` kills -- `os.execve` tears down every thread but
    the caller's without running so much as a `finally`, so nothing in
    `retrospective()` gets a chance to give the slot back -- the claim is real
    but JOURNAL never learns about it: no `retrospective` event is ever
    recorded for that cycle. `_next_retro_cycle()` folds this in alongside the
    journal count so an orphaned claim like that is absorbed once, by the very
    next caller, instead of leaving the journal permanently one cycle behind
    what disk actually holds -- which would otherwise make every retrospective
    after it collide with the claimed slot and log a `retro_clobber_avoided`
    that is not describing a real disagreement.
    """
    highest = 0
    for dir_fn, pattern in _RETRO_ARTIFACT_PATTERNS:
        d = dir_fn()
        if not d.exists():
            continue
        for p in d.iterdir():
            m = pattern.match(p.name)
            if m:
                highest = max(highest, int(m.group(1)))
    return highest


def _next_retro_cycle() -> int:
    """One past the highest cycle either JOURNAL or disk already knows about.

    Not a local counter: `retro_thread()` used to keep `n` as a plain variable,
    which lives only in that thread's memory. `restart_into_new_code()` stops
    the thread and then `os.execve`s the process away to adopt a merged change
    to this file, so the successor's `retro_thread` started `n` back at 0 and
    relabelled its first retrospective "1" -- overwriting a complete, different
    retrospective that already owned that name. JOURNAL is a file, so it
    survives the restart, and re-deriving the count from it every time means a
    fresh process reads back the same number the old one would have used next.

    The journal count alone is not enough: it only grows when a pass finishes,
    but `_claim_retro_cycle` reserves a slot before the pass runs. Taking the
    max against `_highest_retro_cycle_claimed()` keeps the two from skewing
    apart when a pass is killed mid-flight -- see that function's docstring.
    """
    n = 0
    if JOURNAL.exists():
        with JOURNAL.open() as fh:
            for line in fh:
                try:
                    rec = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if rec.get("event") == "retrospective":
                    n += 1
    return max(n, _highest_retro_cycle_claimed()) + 1


def _claim_retro_cycle(wanted: int) -> int:
    """Reserve a free cycle number, deciding ties by the filesystem rather than by timing.

    `_next_retro_cycle()` can hand the same `wanted` to two callers that run
    close together -- `retro_thread()`'s automatic pass and a manual `loop.py
    retro` -- and "the number was free when I checked" is not still true by the
    time either of them writes. `O_CREAT | O_EXCL` makes the report path itself
    the tiebreaker: only one creator can succeed on a given path, so the loser
    observes `FileExistsError` and moves on to the next number instead of
    overwriting what the winner is about to produce.

    A cycle also counts as taken if any of its *other* three artifacts already
    exist even though the report does not -- the case worth catching is a run
    that crashed after `invoke()` wrote the prompt/log/transcript but before the
    agent produced its report, leaving evidence of a retrospective with no
    findings attached to it.
    """
    cycle = wanted
    while True:
        report, *siblings = _retro_artifacts(cycle)
        try:
            fd = os.open(str(report), os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644)
        except FileExistsError:
            cycle += 1
            continue
        os.close(fd)
        if any(p.exists() for p in siblings):
            report.unlink(missing_ok=True)   # give back the claim; it was stale
            cycle += 1
            continue
        break
    if cycle != wanted:
        log(f"    ~~ retro cycle {wanted} already has artifacts on disk; using {cycle} instead")
        record("retro_clobber_avoided", wanted=wanted, used=cycle)
    return cycle


def _prev_retro_through() -> int:
    """The highest `through` recorded by any prior retrospective, or 0.

    Only a pass that demonstrably analysed the journal writes `through` (see
    retrospective()); a pass that did not is still recorded, but with no
    `through` field, so plain `.get("through")` already skips it here. That is
    what makes the watermark hold still across a failed pass instead of
    advancing on a pass that read nothing: the next retrospective calls this
    again and gets back the same line a failed one was already given.
    """
    highest = 0
    if not JOURNAL.exists():
        return highest
    with JOURNAL.open() as fh:
        for line in fh:
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            if rec.get("event") == "retrospective" and "through" in rec:
                highest = max(highest, rec["through"])
    return highest


def retrospective(cycle: int | str) -> None:
    """Diagnose the loop, not the issues, and file fixes for itself."""
    if not JOURNAL.exists():
        return
    if isinstance(cycle, int):
        cycle = _claim_retro_cycle(cycle)
    log(f"== retrospective ({cycle})")
    # Counted before invoke(), not after. Events appended while the agent is
    # running may or may not have been read by it, and the two ways to be
    # wrong here are not symmetric: claiming too little only costs the next
    # pass some re-reading, while claiming too much marks evidence as
    # analysed that nothing ever actually looked at -- and that evidence is
    # what the *next* pass would skip past on the strength of this one.
    with JOURNAL.open() as fh:
        through = sum(1 for _ in fh)
    prev_through = _prev_retro_through()
    slice_note = ""
    if prev_through:
        # Advisory, not a fence: this tells the agent where NEW evidence
        # starts, not that earlier lines are off limits. jq (as used by the
        # recipes above) halts at the first malformed line; `fromjson? //
        # empty` does not, so a torn or damaged line costs this command
        # nothing and it keeps reading -- matching what `through` already
        # tolerates on the code side.
        slice_note = (
            "\n# Evidence already analysed\n\n"
            f"A previous retrospective analysed orchestrator/logs/events.jsonl "
            f"through line {prev_through}. New evidence starts after that "
            "line, though you may still look back further if a new event "
            "only makes sense against an old one. To read just the new "
            "slice, tolerating a damaged line instead of stopping at it:\n\n"
            "```sh\njq -R 'fromjson? // empty' orchestrator/logs/events.jsonl "
            f"| tail -n +{prev_through + 1}\n```\n"
        )
    p = "\n".join([(PROMPTS / "shared.md").read_text(),
                    (PROMPTS / "retrospective.md").read_text(),
                    f"\n# This is retrospective {cycle}. Write to "
                    f"orchestrator/logs/retro-{cycle}.md\n",
                    slice_note])
    _, rc, text = invoke("claude", ROOT, p, LOGS / "retro", f"r{cycle}", role="critic")
    # Only a pass that demonstrably produced analysis may advance the
    # watermark. invoke() drops the error reason _final_text() returns
    # alongside its text (see _final_text()/invoke()), so an API-error result
    # can arrive as non-empty text with rc == 0. The report file existing and
    # non-empty is the check that cannot lie about that: it is the one thing
    # only a real analysis pass produces.
    report = LOGS / f"retro-{cycle}.md"
    if rc == 0 and report.exists() and report.stat().st_size > 0 and text.strip():
        record("retrospective", cycle=cycle, through=through)
    else:
        # Still counted -- _next_retro_cycle() must not hand out this number
        # again -- but not claiming coverage: no `through` field.
        record("retrospective", cycle=cycle, analysed=False)


def cmd_retro() -> int:
    retrospective(_next_retro_cycle())
    return 0


# A merge is the only event that produces new evidence, so it is the only thing
# worth retrospecting on. This runs on its own thread: doing it inline would
# stall a worker for the length of an Opus call, and doing it once per batch
# would skip merges whenever several land together.
_merge_signal = threading.Event()
_retro_stop = threading.Event()


def retro_thread() -> None:
    while not _retro_stop.is_set():
        if not _merge_signal.wait(timeout=5):
            continue
        _merge_signal.clear()          # cleared before the run, so merges that
        try:                            # land during it trigger another pass
            retrospective(_next_retro_cycle())
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
    # Rolling pool, not a batch barrier: a worker that finishes early takes the
    # next issue immediately instead of waiting on its slowest sibling.
    # `inflight` is the single source of truth for how much of the pool is
    # occupied -- the claim budget, the drain confirmation and the busy
    # exclusion below are all keyed off it, not off MAX_PARALLEL.
    inflight: dict[Future, gh.Issue] = {}

    def reap(timeout: float = 0) -> None:
        """Drop finished futures from `inflight`, surfacing any exception.

        work() is documented to never raise (its own try/except sees to that),
        so `.result()` here is not the normal path -- it exists so a broken
        contract becomes a log line instead of a future nobody ever asks
        about, which is what dropping `f.result()` entirely would have done.

        `timeout` lets a caller block until something changes instead of
        busy-polling: 0 (the default) is a non-blocking sweep to free capacity
        before budgeting a new claim; a caller with nothing else to do passes
        a real timeout to wait for the next completion.
        """
        if not inflight:
            return
        done, _ = wait(list(inflight), timeout=timeout, return_when=FIRST_COMPLETED)
        for f in done:
            issue = inflight.pop(f)
            try:
                f.result()
            except Exception as exc:  # noqa: BLE001
                log(f"    !! #{issue.number} escaped work(): {exc}")
                record("work_escaped", issue=issue.number, reason=repr(exc),
                       trace=traceback.format_exc()[-2000:])

    with ThreadPoolExecutor(max_workers=MAX_PARALLEL) as pool:
        while True:
            if time.time() - _start > WALLCLOCK_LIMIT:
                # Claim nothing further. The futures already in `inflight` are
                # not cancelled -- preserve_branch/merge_worktree mean an
                # unmerged attempt is the only copy of itself -- so `break`
                # falls through to the `with` block's shutdown(wait=True),
                # which waits for exactly those futures. The claim budget below
                # never lets more than MAX_PARALLEL be submitted at once, so
                # there is nothing queued behind them for shutdown to overshoot
                # into -- the limit is missed by the tail of one issue, not by
                # a whole extra batch.
                log("!! wallclock limit reached, stopping")
                break

            reap()  # opportunistic, non-blocking: free capacity before budgeting

            # No new claims once a self-update is pending -- swapping the
            # module out from under a running worker would orphan its
            # worktree. Drain to zero, then hand off. #90 owns turning this
            # into a named, logged, once-only quiesce; this is the minimum
            # that keeps the invariant true.
            if self_update_pending():
                if inflight:
                    reap(5)
                    continue
                restart_into_new_code()
                break  # unreachable once restart_into_new_code() execve's;
                       # kept so this loop is correct on its own terms too.

            # Recover before claiming, so anything rescued is claimable in this
            # same round. Cheap when there is nothing to do -- blocked_needing_recovery
            # only reaches an agent for a block that other open issues wait on.
            recover_blocked()
            # Mirror the dependency filter onto the labels. Only writes on a
            # change, so a queue in steady state costs one read.
            added, removed = gh.sync_waiting()
            if added or removed:
                log(f"fr:waiting +{added} -{removed}")

            with _claim_lock:
                # Claim lazily, up to however much of the pool is actually
                # free -- MAX_PARALLEL - len(inflight), not MAX_PARALLEL.
                # ThreadPoolExecutor queues submissions past max_workers rather
                # than running them, so claiming beyond what is free strands
                # the remainder in fr:claimed with a worker that never starts,
                # and the drain/self-update logic above would count it as
                # in-flight work that in fact nobody is touching.
                batch: list[gh.Issue] = []
                budget = MAX_PARALLEL - len(inflight)
                if budget > 0:
                    # An issue can land back in fr:ready while its OWN worker
                    # is still running: resolve_question's REWRITE path calls
                    # gh.requeue() from inside work(), and that worker still
                    # has comment(), restore_worktree() and retire_worktree()
                    # ahead of it -- several seconds still holding
                    # WORKTREES/<n> and branch issue/<n>. Reclaiming it here
                    # would call make_worktree(n) a second time and
                    # force-remove that worktree and branch out from under the
                    # first worker, destroying the attempt preserve_branch
                    # exists to keep. Exclude anything already inflight;
                    # reap() re-opens it the moment the owning worker actually
                    # returns.
                    busy = {i.number for i in inflight.values()}
                    for cand in gh.claimable():
                        if len(batch) >= budget:
                            break
                        if cand.number in busy:
                            continue
                        if gh.claim(cand.number):
                            batch.append(cand)

            if batch:
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
                for issue in batch:
                    inflight[pool.submit(work, issue)] = issue
                continue

            if inflight:
                # Nothing claimable this instant, but a worker could finish --
                # or requeue something -- any second. That is progress, not an
                # empty queue, so it must not feed DRAIN_CONFIRMATIONS: counting
                # it is how three empty polls while workers are still running
                # would read as "queue drained" right as they are about to hand
                # something back. Wait for the next completion instead of
                # polling on a fixed timer.
                reap(10)
                continue

            remaining = gh.fetch(label="fr:ready")
            if remaining:
                empty = 0
                # Distinguish the two reasons this can happen. "Dependencies
                # unmet" with every blocker itself blocked is not waiting,
                # it is deadlock, and it used to look identical in the log to
                # a batch that was about to free up.
                stuck = gh.fetch(label="fr:blocked")
                detail = (f", {len(stuck)} blocked" if stuck else "")
                log(f"waiting: {len(remaining)} ready but dependencies unmet{detail}")
                time.sleep(30)
                continue
            # GitHub's issue list is eventually consistent -- a queue seeded
            # seconds ago reads back empty. Quitting on the first empty poll
            # would end the run before it started. Reached only with `inflight`
            # empty (see above), so this really is "nothing to claim and
            # nothing running", not a snapshot of a momentarily-idle pool.
            empty += 1
            if empty >= DRAIN_CONFIRMATIONS:
                log("queue drained")
                break
            log(f"queue reads empty ({empty}/{DRAIN_CONFIRMATIONS}); confirming")
            time.sleep(20)
            continue

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
    for label in ("fr:ready", "fr:waiting", "fr:claimed", "fr:blocked", "fr:questioned"):
        items = gh.fetch(label=label)
        print(f"\n{label} ({len(items)})")
        for i in items:
            print(f"  #{i.number:<4} {i.title}  deps={i.deps or '-'}")
    # What can actually be picked up right now, which is not len(fr:ready) and
    # was never shown anywhere. A status that reports 49 ready when 39 are
    # claimable is the number that made the queue look healthy while the port
    # was one issue wide.
    ready = gh.claimable()
    print(f"\nclaimable now: {len(ready)}")
    for i in ready[:MAX_PARALLEL]:
        print(f"  #{i.number:<4} [{i.gate}/{i.agent}] {i.title}")
    stuck = gh.blocked_needing_recovery()
    if stuck:
        print(f"\nblocked and gating open work: {len(stuck)}")
        for i, waiting in stuck:
            print(f"  #{i.number:<4} gates {len(waiting)} "
                  f"({', '.join(f'#{n}' for n in waiting[:6])})  "
                  f"recoveries={i.recoveries}/{MAX_RECOVERIES}")
    print(f"\nclosed: {len(gh.closed_numbers())}")
    return 0


def main() -> int:
    cmd = sys.argv[1] if len(sys.argv) > 1 else "run"
    return {"run": cmd_run, "seed": cmd_seed, "status": cmd_status,
            "retro": cmd_retro}.get(
        cmd, lambda: (print(__doc__), 2)[1])()


if __name__ == "__main__":
    sys.exit(main())
