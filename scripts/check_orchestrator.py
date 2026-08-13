#!/usr/bin/env python3
"""Gate check: the orchestrator can still run itself.

The loop can merge changes to its own source and restart into them, so this is
what makes that survivable -- a syntax error, an import-time crash or a role
with no prompt file would otherwise end an unattended run with nobody there to
notice. It runs in every gate profile.
"""

from __future__ import annotations

import ast
import contextlib
import io
import re
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SOURCES = ["orchestrator/loop.py", "orchestrator/gh.py"]


def fail(msg: str) -> int:
    print(f"    {msg}")
    return 1


def check_parses() -> int:
    bad = 0
    for rel in SOURCES:
        p = ROOT / rel
        if not p.exists():
            bad += fail(f"{rel} is missing")
            continue
        try:
            ast.parse(p.read_text())
        except SyntaxError as exc:
            bad += fail(f"{rel} does not parse: line {exc.lineno}: {exc.msg}")
    return bad


def check_runs() -> int:
    """`status` exercises import, the constants, and the loop->gh contract."""
    try:
        p = subprocess.run([sys.executable, str(ROOT / "orchestrator" / "loop.py"), "status"],
                           capture_output=True, text=True, timeout=120, cwd=ROOT)
    except subprocess.TimeoutExpired:
        return fail("loop.py status hung: the orchestrator would not restart")
    if p.returncode != 0:
        detail = (p.stderr or p.stdout).strip().splitlines()
        return fail("loop.py status failed: " + (detail[-1] if detail else "no output"))
    return 0


def check_prompts() -> int:
    """Every role the loop can ask for must have a prompt on disk."""
    src = (ROOT / "orchestrator" / "loop.py").read_text()
    roles = set(re.findall(r'prompt_for\(\s*"(\w+)"', src))
    roles |= set(re.findall(r'role\s*=\s*"(\w+)"', src))
    missing = sorted(r for r in roles
                     if not (ROOT / "orchestrator" / "prompts" / f"{r}.md").exists())
    if missing:
        return fail(f"no prompt file for role(s): {missing}")
    return 0


def check_dep_parsing() -> int:
    """The queue's ordering guarantee, pinned by the cases that broke it.

    An issue whose dependencies parse to [] is claimable immediately, so it
    reaches an agent before the code it builds on exists and burns every
    attempt on a failure the implementer cannot fix. The first two cases are
    verbatim prose from issues the planner filed, which really did parse as
    unblocked. The bullet case is here because the obvious fix -- anchoring the
    pattern to the line start -- silently breaks it in the same direction.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import gh

    cases = [
        ("prose before the metadata line",
         "will all change behaviour on paths #10 depends on.\n\nDepends on: #7\n", [7]),
        ("prose after an issue reference",
         "because #12's `go_ub_write` depends on this distinction.\n\n"
         "Depends on: #10, #12\n", [10, 12]),
        ("bullet form", "- Depends on: #5\n", [5]),
        ("no dependency line", "Nothing to wait for.\n", []),
        ("a version number is not a dependency", "Depends on: PHP 8.5\n", []),
    ]
    bad = 0
    for name, body, want in cases:
        with contextlib.redirect_stderr(io.StringIO()):   # the last case warns by design
            got = gh.Issue(number=1, title="t", body=body).deps
        if got != want:
            bad += fail(f"dependency parsing regressed on {name}: got {got}, want {want}")
    return bad


def check_reviewer_restore() -> int:
    """A reviewer's scratch work must not survive into what gets merged.

    review_stage() gives reviewers full tool access in the worktree with no
    instruction to clean up after themselves -- building a repro there is good
    review work. Without a restore, whatever they leave behind rides
    worktree_diff's and merge_worktree's `git add -A` straight onto main. This
    pins loop.snapshot_worktree/restore_worktree against exactly that: an
    untracked file added, a tracked file edited, and an ignored build artifact
    (which must survive -- this is a restore, not `git clean -xfd`).
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    def sh(wt: Path, *args: str) -> None:
        subprocess.run(["git", *args], cwd=wt, check=True, capture_output=True, text=True)

    with tempfile.TemporaryDirectory() as tmp:
        wt = Path(tmp)
        sh(wt, "init", "-q", "-b", "main")
        sh(wt, "config", "user.email", "check_orchestrator@example.com")
        sh(wt, "config", "user.name", "check_orchestrator")
        (wt / "a.txt").write_text("original\n")
        (wt / ".gitignore").write_text("/target/\n")
        sh(wt, "add", "-A")
        sh(wt, "commit", "-q", "-m", "base")

        # the implementer's work under review: an edit and a new tracked file
        (wt / "a.txt").write_text("original\nimplementer edit\n")
        (wt / "b.txt").write_text("from the implementer\n")
        before = loop.worktree_diff(wt)

        snapshot = loop.snapshot_worktree(wt)
        if not snapshot:
            return fail("snapshot_worktree could not write-tree the worktree")

        # a reviewer: scratch file, edit to a tracked file, a populated (and
        # already-ignored) target/ left behind by a build
        (wt / "scratch.txt").write_text("throwaway repro\n")
        (wt / "b.txt").write_text("reviewer overwrote this\n")
        (wt / "target").mkdir()
        (wt / "target" / "build.out").write_text("build artifact\n")

        loop.restore_worktree(wt, snapshot)
        after = loop.worktree_diff(wt)

        bad = 0
        if after != before:
            bad += fail("restore_worktree did not reproduce the pre-review diff")
        if (wt / "scratch.txt").exists():
            bad += fail("restore_worktree left a reviewer's untracked file in place")
        if (wt / "b.txt").read_text() != "from the implementer\n":
            bad += fail("restore_worktree did not revert a reviewer's edit to a tracked file")
        if not (wt / "target" / "build.out").exists():
            bad += fail("restore_worktree deleted an ignored build artifact -- it must not")
        return bad


if __name__ == "__main__":
    bad = check_parses()
    if bad:                      # do not try to run code that does not parse
        sys.exit(1)
    sys.exit(1 if check_runs() + check_prompts() + check_dep_parsing()
             + check_reviewer_restore() else 0)
