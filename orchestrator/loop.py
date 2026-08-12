#!/usr/bin/env python3
"""
FrankenRust build loop.

Unattended orchestrator that runs coding agents (codex / claude) in parallel
git worktrees against a dependency-ordered backlog. A task is only merged into
main if it passes the verification gate. Designed to run for hours with nobody
watching, and to be resumable after a crash.

    python3 orchestrator/loop.py run          # run until backlog is drained
    python3 orchestrator/loop.py status       # print state
    python3 orchestrator/loop.py reset <id>   # kick a blocked task back to ready
"""

from __future__ import annotations

import json
import os
import shlex
import shutil
import subprocess
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ORCH = ROOT / "orchestrator"
BACKLOG = ORCH / "backlog.json"
STATE = ORCH / "queue" / "state.json"
LOGS = ORCH / "logs"
WORKTREES = ORCH / "worktrees"
PROMPTS = ORCH / "prompts"
GATE = ROOT / "scripts" / "gate.sh"

# --- knobs -------------------------------------------------------------------
MAX_PARALLEL = int(os.environ.get("FR_PARALLEL", "3"))
MAX_ATTEMPTS = int(os.environ.get("FR_ATTEMPTS", "3"))
AGENT_TIMEOUT = int(os.environ.get("FR_AGENT_TIMEOUT", str(60 * 60)))  # 1h/attempt
GATE_TIMEOUT = int(os.environ.get("FR_GATE_TIMEOUT", str(30 * 60)))
WALLCLOCK_LIMIT = int(os.environ.get("FR_WALLCLOCK", str(14 * 60 * 60)))
# -----------------------------------------------------------------------------

_merge_lock = threading.Lock()
_state_lock = threading.Lock()
_start = time.time()


def now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def log(msg: str) -> None:
    line = f"[{now()}] {msg}"
    print(line, flush=True)
    LOGS.mkdir(parents=True, exist_ok=True)
    with (LOGS / "loop.log").open("a") as fh:
        fh.write(line + "\n")


def run(cmd: list[str], cwd: Path, timeout: int, log_path: Path | None = None) -> tuple[int, str]:
    """Run a command, tee output to a log file, return (rc, tail_of_output)."""
    if log_path:
        log_path.parent.mkdir(parents=True, exist_ok=True)
    buf: list[str] = []
    try:
        proc = subprocess.Popen(
            cmd, cwd=str(cwd), stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True, errors="replace", env={**os.environ, "FRANKENRUST_AGENT": "1"},
        )
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


# --- state -------------------------------------------------------------------
@dataclass
class Task:
    id: str
    title: str
    spec: str
    deps: list[str] = field(default_factory=list)
    agent: str = "codex"          # codex | claude | duel
    role: str = "implementer"     # maps to orchestrator/prompts/<role>.md
    gate: str = "default"         # gate profile passed to scripts/gate.sh


def load_backlog() -> dict[str, Task]:
    raw = json.loads(BACKLOG.read_text())
    return {t["id"]: Task(**t) for t in raw["tasks"]}


def load_state() -> dict:
    if STATE.exists():
        return json.loads(STATE.read_text())
    return {"tasks": {}, "started": now()}


def save_state(state: dict) -> None:
    STATE.parent.mkdir(parents=True, exist_ok=True)
    tmp = STATE.with_suffix(".tmp")
    tmp.write_text(json.dumps(state, indent=2))
    tmp.replace(STATE)


def set_status(state: dict, tid: str, status: str, **extra) -> None:
    with _state_lock:
        entry = state["tasks"].setdefault(tid, {})
        entry["status"] = status
        entry["updated"] = now()
        entry.update(extra)
        save_state(state)


# --- agent invocation --------------------------------------------------------
def build_prompt(task: Task, attempt: int, failure: str | None) -> str:
    role = (PROMPTS / f"{task.role}.md").read_text()
    shared = (PROMPTS / "shared.md").read_text()
    parts = [
        shared,
        role,
        f"\n# Your task: {task.id} — {task.title}\n",
        task.spec,
        (
            "\n# Verification\n"
            "Before you finish you MUST run `./scripts/gate.sh " + task.gate + "` from the repo "
            "root and get a clean exit. The loop will run it again independently; if it fails "
            "your work is discarded. Do not weaken, skip, or delete tests to make it pass — "
            "that is the one unforgivable failure mode here.\n"
        ),
    ]
    if failure:
        parts.append(
            f"\n# Previous attempt {attempt - 1} FAILED the gate\n"
            "Fix the root cause. Do not paper over it.\n\n```\n"
            + failure[-6000:]
            + "\n```\n"
        )
    return "\n".join(parts)


def agent_cmd(agent: str, prompt_file: Path) -> list[str]:
    """Agent invocations, sandboxed as tightly as the work allows.

    These agents run for hours with nobody watching, so they get workspace-write
    rather than a full bypass: they can edit the repo and reach the network
    (cargo and docker both need it) but cannot write outside the worktree.
    Set FR_YOLO=1 to drop the sandbox entirely -- only worth it if a task
    genuinely cannot work otherwise, and it is a real escalation.
    """
    yolo = os.environ.get("FR_YOLO") == "1"
    if agent == "codex":
        if yolo:
            return ["codex", "exec", "--dangerously-bypass-approvals-and-sandbox",
                    "--skip-git-repo-check", "-"]
        return [
            "codex", "exec",
            "-s", "workspace-write",
            "-c", "sandbox_workspace_write.network_access=true",
            "-c", "approval_policy=\"never\"",
            "--skip-git-repo-check",
            "-",
        ]
    if agent == "claude":
        # claude -p has no workspace-scoped sandbox equivalent; headless
        # operation requires skipping prompts. The worktree is the blast radius.
        return [
            "claude", "-p",
            "--dangerously-skip-permissions",
            "--output-format", "text",
        ]
    raise ValueError(f"unknown agent {agent!r}")


def invoke_agent(agent: str, wt: Path, prompt: str, logdir: Path, attempt: int | str) -> int:
    """Feed the prompt over stdin so we never hit argv length limits."""
    prompt_file = logdir / f"prompt.{attempt}.md"
    prompt_file.parent.mkdir(parents=True, exist_ok=True)
    prompt_file.write_text(prompt)
    cmd = agent_cmd(agent, prompt_file)
    logpath = logdir / f"{agent}.{attempt}.log"
    log(f"    -> {agent} attempt {attempt} ({wt.name})")
    with prompt_file.open("rb") as stdin_fh, logpath.open("ab") as out_fh:
        proc = subprocess.Popen(
            cmd, cwd=str(wt), stdin=stdin_fh, stdout=out_fh,
            stderr=subprocess.STDOUT, env={**os.environ, "FRANKENRUST_AGENT": "1"},
        )
        try:
            return proc.wait(timeout=AGENT_TIMEOUT)
        except subprocess.TimeoutExpired:
            proc.kill()
            log(f"    !! {agent} timed out after {AGENT_TIMEOUT}s")
            return 124


# --- worktrees ---------------------------------------------------------------
def make_worktree(tid: str) -> Path:
    wt = WORKTREES / tid
    branch = f"task/{tid}"
    if wt.exists():
        git(["worktree", "remove", "--force", str(wt)])
        shutil.rmtree(wt, ignore_errors=True)
    git(["branch", "-D", branch])
    rc, out = git(["worktree", "add", "-b", branch, str(wt), "main"])
    if rc != 0:
        raise RuntimeError(f"worktree add failed: {out}")
    return wt


def merge_worktree(tid: str, logdir: Path, gate: str = "default") -> bool:
    """Serialize merges; rebase onto latest main so parallel tasks compose."""
    branch = f"task/{tid}"
    wt = WORKTREES / tid
    with _merge_lock:
        git(["add", "-A"], cwd=wt)
        git(["commit", "-m", f"{tid}: agent work", "--allow-empty"], cwd=wt)
        rc, out = git(["rebase", "main"], cwd=wt)
        if rc != 0:
            git(["rebase", "--abort"], cwd=wt)
            log(f"    !! {tid} rebase conflict onto main")
            (logdir / "merge.log").write_text(out)
            return False
        # re-gate after rebase: main may have moved under us
        rc, tail = run(["bash", str(GATE), gate], wt, GATE_TIMEOUT, logdir / "gate.rebase.log")
        if rc != 0:
            log(f"    !! {tid} gate failed after rebase onto main")
            return False
        rc, out = git(["merge", "--ff-only", branch])
        if rc != 0:
            log(f"    !! {tid} ff merge failed: {out}")
            return False
        log(f"    ++ {tid} merged into main")
        return True


# --- the loop ----------------------------------------------------------------
def review_stage(task: Task, wt: Path, logdir: Path, attempt: int) -> str | None:
    """Adversarial review, per the Bun rewrite's implementer->2 reviewers->fixer loop.

    A green gate proves the code compiles and passes the tests we thought to
    write. It does not prove the unsafe blocks are sound, that the PHP thread
    affinity rules are respected, or that we match upstream's behaviour in the
    cases nobody tested. Two reviewers run in SEPARATE processes, each seeing
    only the diff, each told to assume the code is wrong -- independent contexts
    are the point, since one reviewer that also wrote the code just agrees with
    itself. Returns concatenated blocking findings, or None if both pass.
    """
    _, diff = git(["diff", "main...HEAD"], cwd=wt)
    if not diff.strip():
        return None
    (logdir / f"diff.{attempt}.patch").write_text(diff)

    review_prompt = (
        (PROMPTS / "shared.md").read_text()
        + (PROMPTS / "reviewer.md").read_text()
        + f"\n# Change under review: {task.id} — {task.title}\n\n"
        + f"## Task the implementer was given\n{task.spec}\n\n"
        + f"## The diff\n```diff\n{diff[:120000]}\n```\n"
    )

    findings: list[str] = []
    results: dict[int, str] = {}

    def one(idx: int, agent: str) -> None:
        rc = invoke_agent(agent, wt, review_prompt, logdir, f"review{idx}.{attempt}")
        out = (logdir / f"{agent}.review{idx}.{attempt}.log")
        text = out.read_text(errors="replace") if out.exists() else ""
        results[idx] = text

    # Deliberately cross-model: codex and claude fail differently, so two of the
    # same model reviewing one diff is closer to one reviewer than to two.
    threads = [
        threading.Thread(target=one, args=(1, "claude")),
        threading.Thread(target=one, args=(2, "codex")),
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    for idx, text in sorted(results.items()):
        if "VERDICT: BLOCK" in text:
            findings.append(f"## Reviewer {idx} findings\n{text[-8000:]}")
    return "\n\n".join(findings) if findings else None


def work(task: Task, state: dict) -> None:
    logdir = LOGS / task.id
    logdir.mkdir(parents=True, exist_ok=True)
    set_status(state, task.id, "running")
    log(f"== {task.id}: {task.title}")

    try:
        wt = make_worktree(task.id)
    except RuntimeError as exc:
        set_status(state, task.id, "blocked", error=str(exc))
        return

    failure: str | None = None
    agents = ["codex", "claude"] if task.agent == "duel" else [task.agent]

    for attempt in range(1, MAX_ATTEMPTS + 1):
        agent = agents[(attempt - 1) % len(agents)]
        invoke_agent(agent, wt, build_prompt(task, attempt, failure), logdir, attempt)
        rc, tail = run(["bash", str(GATE), task.gate], wt, GATE_TIMEOUT,
                       logdir / f"gate.{attempt}.log")
        if rc != 0:
            log(f"    xx gate failed ({agent}, attempt {attempt}, rc={rc})")
            failure = tail
            continue

        log(f"    ok gate passed ({agent}, attempt {attempt}) — adversarial review")
        blocking = review_stage(task, wt, logdir, attempt)
        if blocking:
            log(f"    xx review BLOCKED {task.id} (attempt {attempt})")
            set_status(state, task.id, "running", last_review=blocking[:2000])
            # A dedicated fixer applies the findings without re-implementing --
            # handing them back to the original author tends to produce a rewrite.
            fix_prompt = (
                (PROMPTS / "shared.md").read_text()
                + (PROMPTS / "fixer.md").read_text()
                + f"\n# Task {task.id} — {task.title}\n{task.spec}\n\n{blocking}\n"
            )
            invoke_agent(agent, wt, fix_prompt, logdir, f"fix.{attempt}")
            rc, tail = run(["bash", str(GATE), task.gate], wt, GATE_TIMEOUT,
                           logdir / f"gate.fix.{attempt}.log")
            if rc != 0:
                failure = f"Fixer broke the gate:\n{tail}"
                continue
            if review_stage(task, wt, logdir, f"post{attempt}"):
                failure = "Reviewers still blocking after the fix pass:\n" + blocking
                continue

        if merge_worktree(task.id, logdir, task.gate):
            set_status(state, task.id, "done", agent=agent, attempts=attempt)
            git(["worktree", "remove", "--force", str(wt)])
            return
        failure = "Work passed gate and review but could not merge into main "\
                  "(rebase conflict or gate regression after rebase). Re-sync and redo."

    set_status(state, task.id, "blocked", attempts=MAX_ATTEMPTS, last_failure=failure or "")
    log(f"!! {task.id} BLOCKED after {MAX_ATTEMPTS} attempts")


def ready(tasks: dict[str, Task], state: dict) -> list[Task]:
    out = []
    for t in tasks.values():
        st = state["tasks"].get(t.id, {}).get("status")
        if st in {"done", "running", "blocked"}:
            continue
        if all(state["tasks"].get(d, {}).get("status") == "done" for d in t.deps):
            out.append(t)
    return out


def cmd_run() -> int:
    tasks = load_backlog()
    state = load_state()
    log(f"loop start: {len(tasks)} tasks, parallel={MAX_PARALLEL}")

    with ThreadPoolExecutor(max_workers=MAX_PARALLEL) as pool:
        while True:
            if time.time() - _start > WALLCLOCK_LIMIT:
                log("!! wallclock limit reached, stopping")
                break
            batch = ready(tasks, state)
            if not batch:
                inflight = [t for t, s in state["tasks"].items() if s.get("status") == "running"]
                if not inflight:
                    break
                time.sleep(15)
                continue
            futures = [pool.submit(work, t, state) for t in batch]
            for f in futures:
                f.result()

    done = sum(1 for s in state["tasks"].values() if s.get("status") == "done")
    blocked = [t for t, s in state["tasks"].items() if s.get("status") == "blocked"]
    log(f"loop end: {done}/{len(tasks)} done, blocked={blocked}")
    return 0 if not blocked else 1


def cmd_status() -> int:
    tasks = load_backlog()
    state = load_state()
    for tid, t in tasks.items():
        s = state["tasks"].get(tid, {})
        print(f"{s.get('status','pending'):>8}  {tid:<28} {t.title}")
    return 0


def main() -> int:
    cmd = sys.argv[1] if len(sys.argv) > 1 else "run"
    if cmd == "run":
        return cmd_run()
    if cmd == "status":
        return cmd_status()
    if cmd == "reset":
        state = load_state()
        state["tasks"].pop(sys.argv[2], None)
        save_state(state)
        return 0
    print(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main())
