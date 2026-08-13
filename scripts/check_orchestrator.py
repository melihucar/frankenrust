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

    review_stage() gives reviewers full tool access in the worktree and tells
    them to build repros -- good review work. Without a restore, whatever they
    leave behind rides worktree_diff's and merge_worktree's `git add -A`
    straight onto main. This pins loop.snapshot_worktree/restore_worktree
    against the ways that has been shown to happen.

    The last three reviewer artifacts below are the ones an index-driven
    restore alone gets wrong, each exiting 0 while leaking:

      * a repro crate with its own .gitignore (`cargo new --vcs git` writes
        one): `add -A` skips the build output it covers, then the restore
        deletes the rule and un-ignores it,
      * the same shape via a rule appended to the root .gitignore,
      * a nested git repo, which stages as a gitlink and which `read-tree`
        refuses to rmdir.

    The implementer's own ignored build cache must still survive -- this is a
    restore, not `git clean -xfd`.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    def sh(wt: Path, *args: str) -> None:
        subprocess.run(["git", *args], cwd=wt, check=True, capture_output=True, text=True)

    def init(wt: Path) -> None:
        sh(wt, "init", "-q", "-b", "main")
        sh(wt, "config", "user.email", "check_orchestrator@example.com")
        sh(wt, "config", "user.name", "check_orchestrator")

    def write(p: Path, text: str) -> None:
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)

    # loop.record appends to the real orchestrator/logs/events.jsonl, which the
    # retrospective reads as evidence about the run. Capture instead, both to
    # keep test runs out of it and to assert on what did or did not get logged.
    events: list[tuple[str, dict]] = []
    real_record, real_log = loop.record, loop.log
    loop.record = lambda event, **f: events.append((event, f))
    loop.log = lambda msg: None
    try:
        with tempfile.TemporaryDirectory() as tmp:
            wt = Path(tmp)
            init(wt)
            write(wt / "a.txt", "original\n")
            write(wt / ".gitignore", "/target/\n")
            sh(wt, "add", "-A")
            sh(wt, "commit", "-q", "-m", "base")

            # the implementer's work under review: an edit, a new tracked file,
            # and the build cache their gate run populated
            write(wt / "a.txt", "original\nimplementer edit\n")
            write(wt / "b.txt", "from the implementer\n")
            write(wt / "target" / "build.out", "build artifact\n")
            before = loop.worktree_diff(wt)

            snapshot = loop.snapshot_worktree(wt)
            if not snapshot:
                return fail("snapshot_worktree could not write-tree the worktree")

            # a reviewer investigating the diff
            write(wt / "scratch.txt", "throwaway repro\n")
            write(wt / "b.txt", "reviewer overwrote this\n")
            write(wt / "target" / "reviewer.out", "reviewer's own build output\n")
            write(wt / ".review-repro" / "a" / ".gitignore", "/target\n")
            write(wt / ".review-repro" / "a" / "Cargo.toml", "[package]\n")
            write(wt / ".review-repro" / "a" / "src" / "main.rs", "fn main() {}\n")
            write(wt / ".review-repro" / "a" / "target" / "repro.o", "\0\0\0\n")
            write(wt / ".gitignore", "/target/\n/reviewer-scratch/\n")
            write(wt / "reviewer-scratch" / "main.rs", "fn main() {}\n")
            nested = wt / "nested-clone"
            nested.mkdir()
            init(nested)
            write(nested / "f.txt", "upstream, cloned to diff against\n")
            sh(nested, "add", "-A")
            sh(nested, "commit", "-q", "-m", "scratch")

            loop.restore_worktree(wt, snapshot)
            after = loop.worktree_diff(wt)

            bad = 0
            # The predicate that matters: what the next stage -- the fixer's
            # review, or merge_worktree -- sees is what was reviewed.
            if after != before:
                bad += fail("restore_worktree did not reproduce the pre-review diff")
            if (wt / "scratch.txt").exists():
                bad += fail("restore_worktree left a reviewer's untracked file in place")
            if (wt / "b.txt").read_text() != "from the implementer\n":
                bad += fail("restore_worktree did not revert a reviewer's edit to a tracked file")
            if (wt / ".review-repro").exists():
                bad += fail("restore_worktree left a repro crate whose own .gitignore "
                            "hid its build output from `git add -A`")
            if (wt / "reviewer-scratch").exists():
                bad += fail("restore_worktree left a directory a reviewer-added ignore "
                            "rule hid, and reverting .gitignore un-ignores it")
            if nested.exists():
                bad += fail("restore_worktree left a nested git repo, which merges as a "
                            "gitlink pointing at a commit no clone can resolve")
            if not (wt / "target" / "build.out").exists():
                bad += fail("restore_worktree deleted an ignored build artifact -- it must not")
            if [e for e, _ in events]:
                bad += fail(f"restore_worktree reported trouble on a clean restore: {events}")

            # ...and when it cannot restore, it must say so rather than hand
            # the residue on in silence. Every command in the leaking cases
            # above exits 0, so the postcondition -- not rc -- is what has to
            # hold; an unreachable tree is just the cheapest way to force it.
            events.clear()
            loop.restore_worktree(wt, "0" * 40)
            if "restore_incomplete" not in [e for e, _ in events]:
                bad += fail("restore_worktree reported success on a restore that did not "
                            f"happen; recorded {[e for e, _ in events]}")
            return bad
    finally:
        loop.record, loop.log = real_record, real_log


if __name__ == "__main__":
    bad = check_parses()
    if bad:                      # do not try to run code that does not parse
        sys.exit(1)
    sys.exit(1 if check_runs() + check_prompts() + check_dep_parsing()
             + check_reviewer_restore() else 0)
