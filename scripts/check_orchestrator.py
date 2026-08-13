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
        # Extraction must be line-anchored, not a search over the whole body.
        # #56 -- the issue for cutting spurious dependency edges -- wrote
        # "Audit every `Depends on:` edge in the port graph -- #8, #10, ..."
        # and thereby depended on all of them, so it could not be claimed until
        # the port it existed to unblock was already finished.
        ("the phrase mid-sentence is prose, not metadata",
         "1. **Audit every `Depends on:` edge** -- #8, #10, #11, #12, #13,\n", []),
        ("a quoted dependency line inside prose is still prose",
         "#11 carries the identical `Depends on: #7, #8` and never uses it.\n", []),
        # ...but a real line following prose must still be found, which is the
        # failure the anchoring could plausibly introduce.
        ("prose mentioning the phrase, then a real line",
         "This is what `Depends on: #99` in another issue caused.\n\n"
         "Depends on: #7\n", [7]),
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


def check_gate_targets_the_worktree() -> int:
    """The gate must run the worktree's own gate.sh, not the main checkout's.

    gate.sh starts with `cd "$(dirname "$0")/.."`, so handing it the main
    checkout's path makes it cd there and silently discard the cwd the loop
    set. Every gate then validates main instead of the diff about to merge --
    green, meaningless, and undetectable from the outside. It cost a whole run:
    the first issue to create a Cargo workspace failed three times on
    `Cargo.toml missing` while the file sat committed in its worktree.

    Checked two ways because either alone rots: gate_for must resolve to the
    worktree's copy, and no gate invocation may go back to passing str(GATE).
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    bad = 0
    with tempfile.TemporaryDirectory() as tmp:
        wt = Path(tmp) / "wt"
        (wt / "scripts").mkdir(parents=True)
        (wt / "scripts" / "gate.sh").write_text("#!/bin/bash\n")
        got = loop.gate_for(wt)
        if Path(got).resolve() != (wt / "scripts" / "gate.sh").resolve():
            bad += fail(f"gate_for() must return the worktree's gate.sh, got {got}")

    src = (ROOT / "orchestrator" / "loop.py").read_text()
    for m in re.finditer(r'run\(\["bash",\s*([^,]+),', src):
        if "gate_for" not in m.group(1):
            bad += fail(f"gate invoked with {m.group(1).strip()}; must be gate_for(wt)")
    return bad


def check_no_absorbing_states() -> int:
    """Every label the loop can put on an issue, something can take off again.

    The general form of the defect that nearly cost the project a night.
    `fr:blocked` was set by `block()` and removed by nothing, so the toolchain
    issue twelve others depended on parked at 02:30 and only a human reading
    the label by chance kept the run alive. `fr:questioned` had been the same
    shape earlier, which is why the resolver exists.

    Stated as an invariant rather than two special cases, because the next
    absorbing state will be a label that does not exist yet. An issue leaves
    this machine by being closed; it must never leave by being labelled.
    """
    src = (ROOT / "orchestrator" / "gh.py").read_text()

    def labels_for(flag: str) -> set[str]:
        out: set[str] = set()
        for m in re.finditer(rf'"{flag}",\s*"([^"]+)"', src):
            out |= {p.strip() for p in m.group(1).split(",") if p.strip()}
        return out

    added, removed = labels_for("--add-label"), labels_for("--remove-label")
    stranding = added - removed
    if stranding:
        return fail(f"label(s) the loop can set but never clear: {sorted(stranding)}; "
                    "an issue that reaches one waits for a human who is not coming")
    return 0


def check_blocked_has_a_recovery_path() -> int:
    """fr:blocked specifically must be reachable *out of*, by an agent.

    check_no_absorbing_states proves some code path clears the label; this
    proves the path is a real adjudication and not, say, a blanket release that
    re-queues a failing issue forever. The recovery must have a prompt, a
    bounded budget, and a trigger keyed on what the block is holding up.

    That last part is the one worth pinning. Recovery keyed on an empty queue
    looks equivalent and is not: when #5 parked there were still fourteen
    claimable housekeeping issues, so the loop never starved and would never
    have fired. What makes a block urgent is its dependants.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import gh
    import loop

    bad = 0
    if not (ROOT / "orchestrator" / "prompts" / "unblocker.md").exists():
        bad += fail("no prompts/unblocker.md: fr:blocked has no recovery role")
    for name in ("recover_blocked",):
        if not hasattr(loop, name):
            bad += fail(f"loop.{name}() is gone; fr:blocked is absorbing again")
    for name in ("blocked_gating_work", "unblock"):
        if not hasattr(gh, name):
            bad += fail(f"gh.{name}() is gone; fr:blocked is absorbing again")
    if getattr(loop, "MAX_RECOVERIES", 0) < 1:
        bad += fail("MAX_RECOVERIES < 1 disables recovery entirely")

    # The trigger must be dependant-driven. blocked_gating_work() returning only
    # blocks with open dependants is what encodes that, so assert the filter.
    src = (ROOT / "orchestrator" / "gh.py").read_text()
    if "if x[1]" not in src.split("def blocked_gating_work")[-1].split("def ")[0]:
        bad += fail("blocked_gating_work() no longer filters to blocks with open "
                    "dependants; recovery would fire on starvation, which is too late")

    # ...and cmd_run must actually call it, before claiming.
    run_src = (ROOT / "orchestrator" / "loop.py").read_text().split("def cmd_run")[-1]
    if "recover_blocked()" not in run_src:
        bad += fail("cmd_run() never calls recover_blocked(); the recovery exists "
                    "but nothing triggers it")
    return bad


def check_waiting_is_annotation_only() -> int:
    """fr:waiting must not change what is claimable -- only what is visible.

    It exists because `claimable()`'s dependency filter was invisible from
    outside: 49 issues wore fr:ready while 10 of them could not be picked by
    anyone. The fix is worth nothing if it also becomes a scheduling input,
    because then the label and the filter can disagree and the queue is wrong
    in a new way. Same issues in, same order out, labels notwithstanding.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import gh

    # #4 is the case that matters and the one an obvious test misses: it wears
    # fr:waiting but has nothing to wait for. Labelling an issue the dependency
    # filter already excludes proves nothing, because both a correct claimable()
    # and one that reads the label drop it for different reasons and agree on
    # the answer. Only a stale label -- which is what a crashed run, a closed
    # dependency, or any lag between the two writes leaves behind -- can tell
    # an annotation apart from a scheduling input.
    issues = [
        gh.Issue(number=1, title="dep is closed", body="Depends on: #99",
                 labels=["fr:ready"]),
        gh.Issue(number=2, title="dep is open", body="Depends on: #3",
                 labels=["fr:ready"]),
        gh.Issue(number=3, title="no deps", body="", labels=["fr:ready"]),
        gh.Issue(number=4, title="stale fr:waiting, nothing to wait for", body="",
                 labels=["fr:ready", "fr:waiting"]),
    ]
    real_fetch, real_closed = gh.fetch, gh.closed_numbers
    gh.closed_numbers = lambda: {99}
    try:
        gh.fetch = lambda label=None, state="open": (
            [] if state == "closed"
            else [i for i in issues if not label or label in i.labels])
        got = [i.number for i in gh.claimable()]

        # The same queue with the annotation stripped entirely. A claimable()
        # that ignores the label -- the only correct one -- cannot tell these
        # two apart.
        for i in issues:
            i.labels = [l for l in i.labels if l != "fr:waiting"]
        unlabelled = [i.number for i in gh.claimable()]
    finally:
        gh.fetch, gh.closed_numbers = real_fetch, real_closed

    bad = 0
    if got != unlabelled:
        bad += fail(f"fr:waiting changed the schedule: {got} with the label vs "
                    f"{unlabelled} without; it is an annotation, not a state")
    if 4 not in got:
        bad += fail("an issue wearing a stale fr:waiting was dropped from the "
                    "queue; the label is being read as a scheduling input")
    if 2 in got:
        bad += fail("an issue with an open dependency is claimable")
    # #3 first: #2 waits on it, and rank()'s first key is -dependants. Then #1
    # and #4 on issue number, neither having dependants and neither being
    # housekeeping.
    if got != [3, 1, 4]:
        bad += fail(f"claimable() ordering regressed: got {got}, want [3, 1, 4] "
                    "(by dependant count, then housekeeping, then issue number)")
    return bad


def check_filing_contract_is_stated() -> int:
    """Every role that files an issue must be told the body format.

    planner.md has always specified Gate:/Agent:/Depends on:. shared.md -- the
    only filing guidance the other roles read -- specified none of it, so agents
    filed free-form prose into a queue that parses four structured fields out of
    it. Measured at the time: 15 of 49 open issues had no Gate: and 19 no Agent:,
    silently inheriting `default` and `codex`. A docs fix that inherits `default`
    fails a gate it cannot satisfy, three times, and lands in fr:blocked.

    Checked as a fenced template rather than by grepping for the field names.
    The words "Gate:" and "Agent:" appear all over the surrounding prose that
    explains them, so a substring test passes even with the template deleted --
    it cannot fail, which makes it worse than no check at all. What an agent
    copies is the block.
    """
    shared = (ROOT / "orchestrator" / "prompts" / "shared.md").read_text()
    fields = ("Gate:", "Agent:", "Depends on:")
    blocks = re.findall(r"```[^\n]*\n(.*?)```", shared, re.S)
    if any(all(f in b for f in fields) for b in blocks):
        return 0
    absent = [f for f in fields if f not in shared]
    return fail(
        "prompts/shared.md has no fenced block containing all of "
        f"{list(fields)} — agents that file issues have no template to copy"
        + (f"; {absent} appear nowhere in the file at all" if absent else
           " (the field names appear in prose, but prose is not a template)"))


if __name__ == "__main__":
    bad = check_parses()
    if bad:                      # do not try to run code that does not parse
        sys.exit(1)
    sys.exit(1 if check_runs() + check_prompts() + check_dep_parsing()
             + check_gate_targets_the_worktree() + check_no_absorbing_states()
             + check_blocked_has_a_recovery_path() + check_waiting_is_annotation_only()
             + check_filing_contract_is_stated() + check_reviewer_restore() else 0)
