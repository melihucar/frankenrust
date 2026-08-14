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
import json
import re
import subprocess
import sys
import tempfile
import threading
import time
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
    for name in ("blocked_needing_recovery", "unblock"):
        if not hasattr(gh, name):
            bad += fail(f"gh.{name}() is gone; fr:blocked is absorbing again")
    if getattr(loop, "MAX_RECOVERIES", 0) < 1:
        bad += fail("MAX_RECOVERIES < 1 disables recovery entirely")

    # Reachability, behaviourally. The previous version of this check asserted
    # that gh.unblock() existed and that blocked_needing_recovery() contained a
    # `if x[1]` filter -- and both were true while fr:blocked was still
    # absorbing for every issue nothing depended on, because that filter was
    # what dropped them. A check that proves a code path exists says nothing
    # about whether anything reaches it. Ask the function instead.
    issues = [
        gh.Issue(number=1, title="blocked, gates two", body="", labels=["fr:blocked"]),
        gh.Issue(number=2, title="blocked leaf, nothing waits on it", body="",
                 labels=["fr:blocked"]),
        gh.Issue(number=3, title="waits on 1", body="Depends on: #1", labels=["fr:ready"]),
        gh.Issue(number=4, title="waits on 1", body="Depends on: #1", labels=["fr:ready"]),
    ]
    real_fetch, real_closed = gh.fetch, gh.closed_numbers
    gh.closed_numbers = lambda: set()
    try:
        gh.fetch = lambda label=None, state="open": (
            [] if state == "closed"
            else [i for i in issues if not label or label in i.labels])
        order = [i.number for i, _ in gh.blocked_needing_recovery()]
    finally:
        gh.fetch, gh.closed_numbers = real_fetch, real_closed

    if 2 not in order:
        bad += fail("a blocked issue with no dependants is never offered to recovery; "
                    "fr:blocked is absorbing for leaves, which is most of the queue")
    if order != [1, 2]:
        bad += fail(f"recovery order regressed: got {order}, want [1, 2] "
                    "(most dependants first, leaves last but never dropped)")

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


def check_silent_reviewer_is_not_a_pass() -> int:
    """A reviewer that said nothing must never count as approval.

    `"VERDICT: BLOCK" in ""` is False, so selecting blockers by membership
    alone made a reviewer that timed out, crashed, or hit a quota wall
    indistinguishable from one that read the diff and approved it. Two dead
    reviewers merged code nothing had looked at, and the issue was closed
    claiming "two adversarial reviews (claude + codex)".

    The empty-string and truncated-transcript cases are the ones that actually
    happened; the "reviewer discusses the format" case is here because the
    obvious fix -- searching for either verdict token anywhere -- reintroduces
    exactly the prompt-echo bug that 8802753 fixed for codex.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    cases = [
        ("both silent must block", {1: "", 2: ""}, True),
        ("one dead, one passing must not block but is incomplete",
         {1: "", 2: "Looks correct.\nVERDICT: PASS"}, False),
        ("one dead, one blocking still blocks",
         {1: "", 2: "Unsound.\nVERDICT: BLOCK"}, True),
        ("both passing does not block",
         {1: "VERDICT: PASS", 2: "VERDICT: PASS"}, False),
        ("a truncated transcript is not a pass",
         {1: "I am reading the diff now and", 2: "partial output"}, True),
        ("prose without any verdict token is not a pass",
         {1: "This all looks fine to me.", 2: "No concerns."}, True),
    ]
    bad = 0
    for name, results, want_block in cases:
        blocking, verdicts = loop.review_outcome(results)
        if bool(blocking) != want_block:
            bad += fail(f"review_outcome regressed on {name}: "
                        f"blocking={bool(blocking)}, want {want_block} ({verdicts})")

    # The silent case must be distinguishable from a real finding, or the fixer
    # gets handed "no reviewer produced a verdict" as though it were a defect
    # in the diff and starts editing code to satisfy it.
    blocking, _ = loop.review_outcome({1: "", 2: ""})
    if blocking != loop.SILENT_REVIEW:
        bad += fail("a silent review must return SILENT_REVIEW verbatim so the "
                    "fixer can tell a harness fault from a finding")
    return bad


def check_pre_implementer_stages_restore() -> int:
    """The critic and resolver must not be able to change what gets merged.

    Both run with cwd=worktree and full tool access and are told to research
    against the code, so both can leave files behind -- and they run BEFORE the
    implementer, so anything they leave is swept up by the implementer's diff
    and is indistinguishable from work the implementer did. No reviewer has any
    reason to question it.

    #24 fixed this for reviewers and read as fixing the class. It did not: the
    bracket was added to review_stage only, and _work called the critic bare
    for another two runs.
    """
    src = (ROOT / "orchestrator" / "loop.py").read_text()
    body = src.split("def _work(")[-1].split("\ndef ")[0]
    bad = 0
    if "snapshot_worktree" not in body or "restore_worktree" not in body:
        bad += fail("_work() invokes the critic without a snapshot/restore bracket; "
                    "critic and resolver scratch reaches main")
    elif body.index("snapshot_worktree") > body.index('prompt_for("critic"'):
        bad += fail("_work() snapshots after invoking the critic, which is too late")
    elif "finally:" not in body.split("snapshot_worktree")[1].split('prompt_for("implementer"')[0]:
        bad += fail("_work()'s critic bracket has no finally:, so a critic that "
                    "raises leaves its scratch in the worktree")
    return bad


def check_unmerged_work_survives_reclaim() -> int:
    """Re-claiming an issue must not destroy the previous attempt's commits.

    An issue branch is the only copy of an attempt: agents work uncommitted and
    merge_worktree commits for them, so anything that failed review exists
    nowhere else. make_worktree's `git branch -D` was unconditional.

    #11's branch held 3,955 lines including the only sound design for a Zend
    bailout crossing a Rust frame -- reached only after the obvious approach was
    built and rejected as UB. Re-claiming #11 would have deleted it, and the
    loop would have rebuilt the unsound version, because that is what the issue
    body described.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    def sh(cwd: Path, *args: str) -> str:
        return subprocess.run(["git", *args], cwd=cwd, check=True,
                              capture_output=True, text=True).stdout.strip()

    real_git, real_log, real_record = loop.git, loop.log, loop.record
    events: list[tuple[str, dict]] = []
    loop.log = lambda msg: None
    loop.record = lambda event, **f: events.append((event, f))
    try:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            sh(repo, "init", "-q", "-b", "main")
            sh(repo, "config", "user.email", "check@example.com")
            sh(repo, "config", "user.name", "check")
            (repo / "a.txt").write_text("base\n")
            sh(repo, "add", "-A"); sh(repo, "commit", "-q", "-m", "base")

            # an attempt that failed review: committed, never merged
            sh(repo, "checkout", "-q", "-b", "issue/99")
            (repo / "hard-won.c").write_text("the only copy\n")
            sh(repo, "add", "-A"); sh(repo, "commit", "-q", "-m", "attempt")
            sha = sh(repo, "rev-parse", "HEAD")
            sh(repo, "checkout", "-q", "main")

            loop.git = lambda args, cwd=repo: real_git(args, cwd=repo)
            loop.preserve_branch("issue/99", "99")
            sh(repo, "branch", "-D", "issue/99")

            bad = 0
            tags = sh(repo, "tag", "--list").split()
            if not tags:
                bad += fail("preserve_branch left no tag; an unmerged attempt is "
                            "destroyed by the next claim of its issue")
            elif sh(repo, "rev-parse", tags[0]) != sha:
                bad += fail(f"preserved tag {tags[0]} does not point at the attempt")

            # a fully-merged branch must NOT be tagged, or every reclaim litters
            sh(repo, "checkout", "-q", "-b", "issue/98")
            sh(repo, "checkout", "-q", "main")
            before = len(sh(repo, "tag", "--list").split())
            loop.preserve_branch("issue/98", "98")
            if len(sh(repo, "tag", "--list").split()) != before:
                bad += fail("preserve_branch tagged a branch main already contains")
            return bad
    finally:
        loop.git, loop.log, loop.record = real_git, real_log, real_record


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


def check_verdict_parsing() -> int:
    """An agent's verdict is its last message, never its transcript.

    `codex exec` opens its log with a verbatim echo of the prompt it was given,
    and prompts/reviewer.md necessarily contains the line "`VERDICT: BLOCK` --
    you found at least one defect". _final_text() used to return codex's whole
    transcript ("codex writes plain text, so it passes through"), so
    `"VERDICT: BLOCK" in text` was true of every codex review ever run,
    whatever codex concluded. #8 passed the gate on all three attempts and drew
    PASS from all six reviews, and was parked as fr:blocked anyway; #11 and #20
    died the same way. The same echo made every codex critic read as VERDICT:
    REVISE. Nothing could merge while codex was reachable -- the seven issues
    that did land all merged in windows where codex was quota-walled and both
    reviewers were claude.

    The cases below are transcripts built around the real prompt files, so the
    echo is the actual adversarial input rather than a guess at it. The check
    is vacuous unless those prompts really do quote the tokens, so that is
    asserted first.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    prompts = ROOT / "orchestrator" / "prompts"
    reviewer = (prompts / "reviewer.md").read_text()
    critic = (prompts / "critic.md").read_text()
    bad = 0
    for name, text, token in (("reviewer.md", reviewer, "VERDICT: BLOCK"),
                              ("critic.md", critic, "VERDICT: REVISE")):
        if token not in text:
            bad += fail(f"{name} no longer quotes {token!r}; this check is now "
                        "vacuous -- point it at whatever token replaced it")

    def transcript(echo: str, final: str) -> str:
        """What `codex exec` writes to stdout: echo, work, then the verdict twice."""
        return (f"user\n{echo}\ncodex\n{final}\ntokens used\n44,435\n{final}\n")

    def parsed(tmp: Path, echo: str, final: str, wrote_file: bool = True) -> str:
        log = tmp / "codex.review2.1.log"
        log.write_text(transcript(echo, final) if final else f"user\n{echo}\n"
                       "ERROR: You've hit your usage limit.\n")
        last = tmp / "codex.review2.1.final.txt"
        last.unlink(missing_ok=True)     # as invoke() does, so no case inherits
        if wrote_file and final:
            last.write_text(final + "\n")
        text, err = loop._final_text(log, "codex", last)
        return text if text else f"<no text: {err}>"

    with tempfile.TemporaryDirectory() as tmp:
        d = Path(tmp)
        cases = [
            # (name, echoed prompt, final message, -o written, must contain, must not)
            ("a PASS whose prompt echo quotes BLOCK", reviewer,
             "Checked every path. Nothing blocking.\n\nVERDICT: PASS",
             True, "VERDICT: PASS", "VERDICT: BLOCK"),
            ("the same with no -o file, read off the transcript", reviewer,
             "Checked every path. Nothing blocking.\n\nVERDICT: PASS",
             False, "VERDICT: PASS", "VERDICT: BLOCK"),
            # The fix must not simply stop seeing BLOCK anywhere.
            ("a real BLOCK still blocks", reviewer,
             "### leaks a subscriber\nFile: a.rs:1\n\nVERDICT: BLOCK",
             True, "VERDICT: BLOCK", None),
            ("a real BLOCK off the transcript", reviewer,
             "### leaks a subscriber\nFile: a.rs:1\n\nVERDICT: BLOCK",
             False, "VERDICT: BLOCK", None),
            ("a critic PROCEED whose echo quotes REVISE", critic,
             "The spec matches the oracle.\n\nVERDICT: PROCEED",
             True, "VERDICT: PROCEED", "VERDICT: REVISE"),
        ]
        for name, echo, final, wrote, want, unwanted in cases:
            got = parsed(d, echo, final, wrote)
            if want not in got:
                bad += fail(f"verdict parsing lost {want!r} on {name}: {got[:120]!r}")
            if unwanted and unwanted in got:
                bad += fail(f"verdict parsing read {unwanted!r} out of {name} -- "
                            "the prompt echo is being read as the agent's verdict")

        # A run that died before answering is an error, not a finding. Returning
        # the transcript here is what turned a quota wall into a BLOCK.
        got = parsed(d, reviewer, "", wrote_file=False)
        if "VERDICT" in got:
            bad += fail(f"a codex run that produced no final message parsed as a "
                        f"verdict: {got[:120]!r}")

    # And the wiring that makes the file exist in the first place.
    cmd = loop.agent_cmd("codex", None, Path("/tmp/fr-last.txt"))
    if "-o" not in cmd or "/tmp/fr-last.txt" not in cmd:
        bad += fail("agent_cmd no longer asks codex for its last message (-o); "
                    "verdicts fall back to transcript scraping")
    return bad


def check_record_repairs_torn_journal() -> int:
    """A short write to events.jsonl must cost exactly one record, not two.

    #104: `record()` opens the journal in plain append mode. If a previous
    write landed short -- ENOSPC is the realistic trigger, and this loop
    already emits `low_disk` because the disk does fill -- the file is left
    mid-token with no trailing newline. Appending onto that fragment welds
    the *next*, complete record onto its tail, so a line-by-line reader (the
    retrospective) discards both: the one that was already lost, and the one
    that just arrived. The fix isolates the fragment on its own line before
    writing anything else and says so with a `journal_torn` event, so this
    checks that exactly the fragment -- not the record after it -- ends up
    unreadable.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    bad = 0
    with tempfile.TemporaryDirectory() as tmp:
        logs = Path(tmp)
        journal = logs / "events.jsonl"
        # A write that stopped mid-token: no closing brace, no trailing '\n'.
        fragment = '{"ts": "2026-08-14T00:00:00", "event": "retrospective"'
        journal.write_text(fragment)

        real_journal, real_logs = loop.JOURNAL, loop.LOGS
        loop.JOURNAL, loop.LOGS = journal, logs
        try:
            loop.record("merged", issue=4)
        finally:
            loop.JOURNAL, loop.LOGS = real_journal, real_logs

        lines = journal.read_text().splitlines()
        if len(lines) != 3:
            return bad + fail(f"expected the fragment plus a journal_torn record "
                              f"plus the merged record (3 lines), got "
                              f"{len(lines)}: {lines!r}")
        if lines[0] != fragment:
            bad += fail(f"the original fragment was rewritten, not just "
                        f"isolated on its own line: {lines[0]!r}")

        unreadable = 0
        parsed = []
        for line in lines:
            try:
                parsed.append(json.loads(line))
            except json.JSONDecodeError:
                unreadable += 1
        if unreadable != 1:
            bad += fail(f"expected exactly 1 unreadable line (the original "
                        f"fragment); got {unreadable} of {len(lines)}: {lines!r}")

        events = [r.get("event") for r in parsed]
        if "journal_torn" not in events:
            bad += fail(f"record() did not report the tear with a journal_torn "
                        f"event: {lines!r}")
        else:
            torn = parsed[events.index("journal_torn")]
            if torn.get("offset") != len(fragment):
                bad += fail(f"journal_torn offset {torn.get('offset')!r} does not "
                            f"match the fragment's length {len(fragment)}")
        if events[-1] != "merged":
            bad += fail(f"the merged event record() was asked to write did not "
                        f"land as its own, final, complete line: {lines!r}")
    return bad


def check_retro_cycle_survives_restart() -> int:
    """The retrospective's cycle number must be re-derivable, not remembered.

    `retro_thread()` used to keep the cycle in a plain Python local, `n`, which
    lives only in that thread's memory. A self-restart -- the loop's designed
    way of adopting a merged change to itself -- stops that thread and then
    `os.execve`s the process away, so the successor's `retro_thread` started
    `n` back at 0 and relabelled its first retrospective "1", overwriting a
    complete, different retrospective that already owned that name.

    JOURNAL is a file, so it survives the restart; the fix is to derive the
    cycle from it every time instead. This is checked with a real subprocess
    reading a journal it never wrote a line of itself -- the only way to prove
    the number does not depend on anything the process remembers.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    bad = 0
    with tempfile.TemporaryDirectory() as tmp:
        journal = Path(tmp) / "events.jsonl"
        journal.write_text("\n".join(json.dumps(r) for r in [
            {"ts": "t", "event": "merged", "issue": 1},
            {"ts": "t", "event": "retrospective", "cycle": 1},
            {"ts": "t", "event": "gate_fail", "issue": 2},
            {"ts": "t", "event": "retrospective", "cycle": 2},
        ]) + "\n")
        # An empty, isolated LOGS: _next_retro_cycle() also looks at disk (see
        # check_retro_orphaned_claim_does_not_skew_forever), so it must not pick
        # up whatever this developer's own worktree happens to have lying
        # around in the real orchestrator/logs/.
        logs = Path(tmp) / "logs"
        logs.mkdir()

        real_journal, real_logs = loop.JOURNAL, loop.LOGS
        loop.JOURNAL, loop.LOGS = journal, logs
        try:
            got = loop._next_retro_cycle()
        finally:
            loop.JOURNAL, loop.LOGS = real_journal, real_logs
        if got != 3:
            bad += fail(f"_next_retro_cycle() read a journal with two "
                        f"retrospective events and returned {got}, want 3")

        script = (
            f"import sys; sys.path.insert(0, {str(ROOT / 'orchestrator')!r})\n"
            "from pathlib import Path\n"
            "import loop\n"
            f"loop.JOURNAL = Path({str(journal)!r})\n"
            f"loop.LOGS = Path({str(logs)!r})\n"
            "print(loop._next_retro_cycle())\n"
        )
        p = subprocess.run([sys.executable, "-c", script], capture_output=True,
                           text=True, timeout=30, cwd=ROOT)
        out = p.stdout.strip()
        if out != "3":
            bad += fail(f"a fresh process reading the same journal got "
                        f"{out!r} (stderr: {p.stderr.strip()[-300:]}), want '3' "
                        "-- the cycle number came from somewhere other than the journal")
    return bad


def check_retro_callers_derive_the_cycle() -> int:
    """...and the callers must actually ask for it.

    The other half of check_retro_cycle_survives_restart, and it is the half
    that matches where the bug actually lived: there was no derivation function
    to get wrong, there was a `n = 0` local in `retro_thread()` that `execve`
    threw away. A correct, fully tested `_next_retro_cycle()` can sit in
    loop.py while the caller ignores it and counts for itself, and the run goes
    back to overwriting retro-1.md with a green gate. Same reasoning as
    check_gate_targets_the_worktree: prove the helper works, then prove nobody
    goes around it.

    This is worth an AST walk rather than a grep because retro_thread is code
    the retrospective prompt explicitly invites an agent to edit.
    """
    tree = ast.parse((ROOT / "orchestrator" / "loop.py").read_text())

    def cycle_arg(call: ast.Call) -> ast.expr | None:
        if call.args:
            return call.args[0]
        return next((k.value for k in call.keywords if k.arg == "cycle"), None)

    def calls_to_retrospective(node: ast.AST) -> list[ast.Call]:
        return [n for n in ast.walk(node)
                if isinstance(n, ast.Call) and isinstance(n.func, ast.Name)
                and n.func.id == "retrospective"]

    def is_derived(arg: ast.expr | None) -> bool:
        return (isinstance(arg, ast.Call) and isinstance(arg.func, ast.Name)
                and arg.func.id == "_next_retro_cycle")

    bad = 0
    # No numbered pass anywhere may invent its own cycle. A string literal is
    # the deliberate exception -- retrospective("final") names itself.
    for call in calls_to_retrospective(tree):
        arg = cycle_arg(call)
        if isinstance(arg, ast.Constant) and isinstance(arg.value, str):
            continue
        if not is_derived(arg):
            shown = ast.unparse(arg) if arg is not None else "<no cycle argument>"
            bad += fail(f"loop.py:{call.lineno}: retrospective({shown}) -- the cycle must "
                        "be _next_retro_cycle(); a number the process carries in memory "
                        "resets to 1 on every self-restart and overwrites retro-1.md")

    thread = next((n for n in tree.body if isinstance(n, ast.FunctionDef)
                   and n.name == "retro_thread"), None)
    if thread is None:
        return bad + fail("loop.py has no retro_thread(); this check is pointed at "
                          "nothing and the restart regression is unguarded again")
    if not calls_to_retrospective(thread):
        bad += fail("retro_thread() no longer calls retrospective(); the automatic "
                    "pass -- the one a self-restart interrupts -- is unchecked")
    # The historical body's other half, kept as its own tripwire: `n += 1`.
    # Redundant with the argument check today, but it is the exact shape that
    # regressed once, and it costs three lines to make that shape unmergeable.
    for node in ast.walk(thread):
        if isinstance(node, ast.AugAssign):
            bad += fail(f"loop.py:{node.lineno}: retro_thread() keeps a running counter "
                        f"({ast.unparse(node)}); that state dies with the process on "
                        "restart, which is the bug #39 exists to prevent")
    return bad


def check_retro_no_clobber() -> int:
    """A cycle whose artifacts already exist on disk must never be overwritten.

    `_next_retro_cycle()` counts JOURNAL events, but a journal that disagrees
    with what is actually on disk -- a hand-moved file, a manual `loop.py
    retro`, two processes racing -- is exactly the situation #39 exists to
    make survivable. Finding `retro-3.md` already written when about to write
    `retro-3.md` must roll forward to the next free number, leave the existing
    file byte-for-byte untouched, and say so in the journal.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    events: list[tuple[str, dict]] = []
    real_record, real_log = loop.record, loop.log
    loop.record = lambda event, **f: events.append((event, f))
    loop.log = lambda msg: None
    try:
        with tempfile.TemporaryDirectory() as tmp:
            logs = Path(tmp)
            existing = logs / "retro-3.md"
            existing.write_text("the real retrospective 3\n")
            before = existing.read_bytes()

            real_logs = loop.LOGS
            loop.LOGS = logs
            try:
                used = loop._claim_retro_cycle(3)
            finally:
                loop.LOGS = real_logs

            bad = 0
            if used == 3:
                bad += fail("_claim_retro_cycle(3) returned 3 with retro-3.md "
                            "already on disk; the existing report would be overwritten")
            if existing.read_bytes() != before:
                bad += fail("_claim_retro_cycle mutated an existing retro-N.md "
                            "instead of leaving it untouched")
            claimed = logs / f"retro-{used}.md"
            if not claimed.exists():
                bad += fail(f"_claim_retro_cycle({used}) did not reserve {claimed}")
            if ("retro_clobber_avoided", {"wanted": 3, "used": used}) not in events:
                bad += fail(f"_claim_retro_cycle did not record retro_clobber_avoided "
                            f"(wanted=3, used={used}); recorded {events}")
            return bad
    finally:
        loop.record, loop.log = real_record, real_log


def check_retro_cycle_claim_is_atomic() -> int:
    """Two simultaneous claims of the same cycle must not both win it.

    A previous version of this fix checked "does retro-N.md exist?" and then
    created it as two separate steps -- `retro_thread()`'s automatic pass and a
    manual `loop.py retro` can both observe cycle N free and both write it,
    since nothing stops a second check from running in the gap before the
    first write lands. `_claim_retro_cycle` has to decide the tie with
    `O_CREAT | O_EXCL`, which the filesystem makes atomic; this drives two
    threads at the identical cycle number to prove the race is actually closed,
    not just unlikely to lose in a quick test run.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    real_log, real_record = loop.log, loop.record
    loop.log = lambda msg: None
    loop.record = lambda event, **f: None
    try:
        with tempfile.TemporaryDirectory() as tmp:
            real_logs = loop.LOGS
            loop.LOGS = Path(tmp)
            try:
                results: list[int] = []
                lock = threading.Lock()

                def claim() -> None:
                    got = loop._claim_retro_cycle(7)
                    with lock:
                        results.append(got)

                threads = [threading.Thread(target=claim) for _ in range(2)]
                for t in threads:
                    t.start()
                for t in threads:
                    t.join()
            finally:
                loop.LOGS = real_logs

            if len(results) != 2 or results[0] == results[1]:
                return fail(f"two concurrent _claim_retro_cycle(7) calls returned "
                            f"{results}; both claimed the same report path")
            return 0
    finally:
        loop.log, loop.record = real_log, real_record


def check_retro_orphaned_claim_does_not_skew_forever() -> int:
    """A pass killed after claiming a cycle must not poison every pass after it.

    `_claim_retro_cycle` pre-creates a cycle's report file before the agent
    that fills it in has run. `restart_into_new_code()` can kill exactly that
    pass: it calls `os.execve` without joining the retro thread, and `execve`
    destroys every thread but the caller's without running so much as a
    `finally`, so nothing ever unclaims the slot or records a `retrospective`
    event for it. A derivation that only counts JOURNAL never finds out --
    every later pass asks for the same already-claimed number, gets bumped
    forward by `_claim_retro_cycle`, and logs a `retro_clobber_avoided` that
    is not describing a real disagreement, forever.

    Simulates exactly that: two successful retrospectives in JOURNAL, a third
    that claimed `retro-3.md` and never got further. The next call must jump
    straight to 4, and claiming that 4 must not itself bump again -- the
    orphan gets absorbed exactly once.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    events: list[tuple[str, dict]] = []
    real_record, real_log = loop.record, loop.log
    loop.record = lambda event, **f: events.append((event, f))
    loop.log = lambda msg: None
    try:
        with tempfile.TemporaryDirectory() as tmp:
            journal = Path(tmp) / "events.jsonl"
            journal.write_text("\n".join(json.dumps(r) for r in [
                {"ts": "t", "event": "retrospective", "cycle": 1},
                {"ts": "t", "event": "retrospective", "cycle": 2},
            ]) + "\n")
            logs = Path(tmp) / "logs"
            logs.mkdir()
            (logs / "retro-3.md").touch()   # the orphaned claim; empty, no journal entry

            real_journal, real_logs = loop.JOURNAL, loop.LOGS
            loop.JOURNAL, loop.LOGS = journal, logs
            try:
                got = loop._next_retro_cycle()
                bad = 0
                if got != 4:
                    bad += fail(f"_next_retro_cycle() with 2 journal events and an "
                                f"orphaned retro-3.md on disk returned {got}, want 4 "
                                "-- the claim from the killed pass was never absorbed")
                used = loop._claim_retro_cycle(got)
                if used != got:
                    bad += fail(f"_claim_retro_cycle({got}) used {used}; the orphan "
                                "should already be accounted for by _next_retro_cycle, "
                                "so claiming its answer must succeed on the first try")
                if events:
                    bad += fail(f"claiming the derived cycle logged {events}; a "
                                "correctly-derived cycle should never collide")
            finally:
                loop.JOURNAL, loop.LOGS = real_journal, real_logs
            return bad
    finally:
        loop.record, loop.log = real_record, real_log


def check_retro_through_is_snapshotted_before_invoke() -> int:
    """#105: a successful pass records `through` as of *before* the agent ran.

    `through` has to be the physical line count of events.jsonl as it stood
    the instant invoke() was called, not whatever the file grows to while the
    agent is working -- a merge landing mid-pass may or may not have been
    read by it, and claiming it was is the wrong way to be wrong (evidence
    lost for good beats one redundant re-read). This drives retrospective()
    with a fake invoke() that appends a new journal line *during* the call --
    simulating exactly that race -- and a report file, so the pass reads as
    genuinely analysed. The recorded `through` must reflect the journal as it
    stood before invoke() ran, not after.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    events: list[tuple[str, dict]] = []
    real_record, real_log, real_invoke = loop.record, loop.log, loop.invoke
    real_journal, real_logs = loop.JOURNAL, loop.LOGS
    loop.record = lambda event, **f: events.append((event, f))
    loop.log = lambda msg: None
    bad = 0
    try:
        with tempfile.TemporaryDirectory() as tmp:
            logs = Path(tmp)
            journal = logs / "events.jsonl"
            journal.write_text("\n".join(json.dumps(r) for r in [
                {"ts": "t", "event": "merged", "issue": 1},
                {"ts": "t", "event": "gate_fail", "issue": 2},
            ]) + "\n")
            loop.JOURNAL, loop.LOGS = journal, logs

            def fake_invoke(agent, wt, prompt, logdir, tag, role="implementer",
                            escalate=False):
                # A merge landing while the agent is "running" -- must not be
                # folded into `through`, which was already taken.
                with journal.open("a") as fh:
                    fh.write(json.dumps({"ts": "t", "event": "merged", "issue": 99}) + "\n")
                (logs / "retro-1.md").write_text("findings\n")
                return "claude", 0, "some analysis"

            loop.invoke = fake_invoke
            loop.retrospective(1)

            recs = [f for e, f in events if e == "retrospective"]
            if len(recs) != 1:
                bad += fail(f"expected exactly one retrospective record, got {recs}")
            elif recs[0].get("through") != 2:
                bad += fail(f"through={recs[0].get('through')!r}, want 2 (the "
                            "journal's line count before invoke() ran, excluding "
                            f"the merge recorded during it): {recs[0]}")
    finally:
        loop.record, loop.log, loop.invoke = real_record, real_log, real_invoke
        loop.JOURNAL, loop.LOGS = real_journal, real_logs
    return bad


def check_retro_unanalysed_pass_does_not_advance_through() -> int:
    """#105: a pass that did not demonstrably analyse anything must not move the watermark.

    Three things must ALL hold before a pass may claim coverage: rc == 0, a
    non-empty final message, and a non-empty logs/retro-{cycle}.md on disk.
    invoke() drops the error reason _final_text() returns alongside its text,
    so an API-error result can arrive as non-empty text with rc == 0 -- the
    report file is the check that cannot lie about that. This drives two
    failing shapes (rc != 0 with nothing written; rc == 0 with text but no
    report file) and asserts each records `analysed=False` with no `through`,
    and that _prev_retro_through() -- what the *next* pass's prompt is built
    from -- is unmoved by either, so the next pass is offered the same
    starting line as the failed one, not a line further on.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    events: list[tuple[str, dict]] = []
    real_record, real_log, real_invoke = loop.record, loop.log, loop.invoke
    real_journal, real_logs = loop.JOURNAL, loop.LOGS
    loop.log = lambda msg: None
    bad = 0
    try:
        with tempfile.TemporaryDirectory() as tmp:
            logs = Path(tmp)
            journal = logs / "events.jsonl"
            journal.write_text("\n".join(json.dumps(r) for r in [
                {"ts": "t", "event": "merged", "issue": 1},
                {"ts": "t", "event": "retrospective", "cycle": 1, "through": 1},
            ]) + "\n")
            loop.JOURNAL, loop.LOGS = journal, logs

            # The stub appends to the journal as well as capturing, because
            # _prev_retro_through() below reads the file, not the capture list.
            # A stub that only captured would leave that assertion reading back
            # the fixture line it wrote itself, and it could then never fail --
            # a wrongly-recorded `through` would go straight past it.
            def fake_record(event, **f):
                events.append((event, f))
                with journal.open("a") as fh:
                    fh.write(json.dumps({"ts": "t", "event": event, **f}) + "\n")

            loop.record = fake_record

            # cycle 2: the agent errors out. rc != 0, nothing written.
            loop.invoke = lambda *a, **k: ("claude", 1, "")
            loop.retrospective(2)

            # cycle 3: a non-empty final message, but no report file -- the
            # API-error-arrives-as-success shape invoke() can produce.
            loop.invoke = lambda *a, **k: ("claude", 0, "looks fine, nothing to report")
            loop.retrospective(3)

            recs = [(e, f) for e, f in events if e == "retrospective"]
            labels = ["rc != 0", "no report file"]
            if len(recs) != 2:
                bad += fail(f"expected 2 retrospective records, got {recs}")
            else:
                for (_, f), label in zip(recs, labels):
                    if f.get("analysed") is not False:
                        bad += fail(f"{label}: expected analysed=False, got {f}")
                    if "through" in f:
                        bad += fail(f"{label}: recorded through={f['through']!r}; "
                                    "a pass that did not analyse anything must not "
                                    "claim coverage")

            got = loop._prev_retro_through()
            if got != 1:
                bad += fail(f"_prev_retro_through() after two unanalysed passes "
                            f"returned {got}, want 1 -- an unanalysed pass moved "
                            "the watermark even though it read nothing")
    finally:
        loop.record, loop.log, loop.invoke = real_record, real_log, real_invoke
        loop.JOURNAL, loop.LOGS = real_journal, real_logs
    return bad


def _decode_json_stream(out: str) -> list[dict] | None:
    """Decode concatenated (pretty-printed) JSON objects, or None if it is not that.

    Stronger than a substring probe: a slice that begins mid-record leaves a
    fragment like `  "issue": 34,` at the front, which is not decodable, so
    this catches an off-by-a-different-unit that `'"issue": 7' in out` would
    happily accept.
    """
    dec, objs, i = json.JSONDecoder(), [], 0
    while i < len(out):
        if out[i].isspace():
            i += 1
            continue
        try:
            obj, i = dec.raw_decode(out, i)
        except json.JSONDecodeError:
            return None
        objs.append(obj)
    return objs


def check_retro_damaged_line_and_tolerant_prompt_command() -> int:
    """#105: a damaged line must not freeze `through`, and the agent's own
    read command must survive it too.

    `through` is a physical line count specifically so a torn or malformed
    line costs exactly one line rather than capping the watermark at the last
    readable one forever (an earlier attempt at this issue did exactly that).
    But the code advancing past the damage is only half of it: the *agent* is
    handed a `tail`/`jq` recipe to read the new slice, and plain `jq` -- what
    prompts/retrospective.md's own recipes use -- halts at the first
    malformed line. If the embedded command did the same, the watermark would
    advance over evidence the agent's own command never reached. This builds
    a journal with valid records before the watermark, a valid record after
    it, a torn line, and two more valid records after that, runs a successful
    retrospective, and checks both: `through` lands past the damage, and the
    exact command text embedded in the prompt, executed for real against that
    journal, reaches the records on both sides of the tear (unlike the
    non-tolerant recipe, run alongside for contrast).

    The watermark deliberately spans several records, and the assertions are
    on the decoded object stream rather than on substrings, because the first
    version of this check pinned prev_through=1 -- `tail -n +2`, the single
    offset at which composing the pipeline backwards (`jq ... | tail`) is
    invisible, since it drops one `{` and nothing else. That shipped a command
    whose `tail` counted jq's pretty-printed output lines, ~5 per record, so
    the agent got the journal from about line N/5 onward -- beginning on a
    torn fragment -- while the prompt said lines 1..N were already analysed.
    Hence: records before the watermark must be ABSENT, every byte of output
    must decode as whole JSON objects, and the offset must be the physical
    line number.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    events: list[tuple[str, dict]] = []
    real_record, real_log, real_invoke = loop.record, loop.log, loop.invoke
    real_journal, real_logs = loop.JOURNAL, loop.LOGS
    loop.record = lambda event, **f: events.append((event, f))
    loop.log = lambda msg: None
    bad = 0
    captured: dict = {}
    try:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            logs = root / "orchestrator" / "logs"
            logs.mkdir(parents=True)
            journal = logs / "events.jsonl"

            # Lines 1-4 are already analysed (line 4 is the record that says
            # so, and sets prev_through=4). Line 5 is new, line 6 is torn, and
            # lines 7-8 are new again -- so the slice the agent is handed has
            # valid records on both sides of the damage, and four records
            # ahead of the watermark that it must NOT be handed.
            analysed = [{"ts": "t", "event": "merged", "issue": 1},
                        {"ts": "t", "event": "gate_fail", "issue": 2},
                        {"ts": "t", "event": "merged", "issue": 3},
                        {"ts": "t", "event": "retrospective", "cycle": 1, "through": 4}]
            damaged = '{"ts": "t", "event": "gate_fail", "issue": 6'   # torn: no closing brace
            fresh_before = [{"ts": "t", "event": "claimed", "issue": 5}]
            fresh_after = [{"ts": "t", "event": "recovered", "issue": 7},
                           {"ts": "t", "event": "merged", "issue": 8}]
            lines = ([json.dumps(r) for r in analysed]
                     + [json.dumps(r) for r in fresh_before]
                     + [damaged]
                     + [json.dumps(r) for r in fresh_after])
            journal.write_text("\n".join(lines) + "\n")
            total_lines = len(lines)
            prev_through = 4
            want_issues = [5, 7, 8]

            loop.JOURNAL, loop.LOGS = journal, logs

            def fake_invoke(agent, wt, prompt, logdir, tag, role="implementer",
                            escalate=False):
                captured["prompt"] = prompt
                (logs / "retro-2.md").write_text("findings\n")
                return "claude", 0, "some analysis"

            loop.invoke = fake_invoke
            loop.retrospective(2)

            recs = [f for e, f in events if e == "retrospective"]
            if len(recs) != 1 or recs[0].get("through") != total_lines:
                bad += fail(f"want through={total_lines} (the physical line count, "
                            f"damaged line included), got {recs}")

            text = captured.get("prompt", "")
            # retrospective.md has its own ```sh fences (the plain, non-tolerant
            # recipes in "Your evidence"), so find the one this change adds --
            # not just the first fence in the prompt.
            blocks = re.findall(r"```sh\n(.*?)```", text, re.S)
            tolerant = [b for b in blocks if "fromjson" in b]
            if not tolerant:
                return bad + fail("the prompt does not embed the tolerant jq/tail "
                                  f"recipe at all: {text[-1000:]!r}")
            cmd = tolerant[0].strip()
            off = re.search(r"tail -n \+(\d+)", cmd)
            if not off or int(off.group(1)) != prev_through + 1:
                bad += fail(f"embedded command has the wrong tail offset: {cmd!r}, "
                            f"want +{prev_through + 1} (previous through="
                            f"{prev_through}, new evidence starts on the next "
                            "physical line)")

            proc = subprocess.run(["bash", "-c", cmd], cwd=root,
                                  capture_output=True, text=True, timeout=30)
            got = _decode_json_stream(proc.stdout)
            if got is None:
                bad += fail("the embedded command emitted output that is not a "
                            "clean stream of JSON objects -- it is slicing on some "
                            "unit other than journal lines and started mid-record; "
                            f"stdout:\n{proc.stdout}")
            elif [r.get("issue") for r in got] != want_issues:
                bad += fail(
                    f"the embedded command emitted issues "
                    f"{[r.get('issue') for r in got]}, want {want_issues}: the "
                    "records after line "
                    f"{prev_through}, damaged line skipped and both sides of it "
                    "present, and nothing from the already-analysed lines "
                    f"1-{prev_through}; stdout:\n{proc.stdout}")

            # Contrast: the same slice read with plain `jq`, as the recipes in
            # prompts/retrospective.md do, must stop at the tear -- the exact
            # regression `fromjson? // empty` exists to avoid. Same ordering,
            # so the only variable is tolerance.
            naive = subprocess.run(
                ["bash", "-c", f"tail -n +{prev_through + 1} "
                               "orchestrator/logs/events.jsonl | jq ."],
                cwd=root, capture_output=True, text=True, timeout=30)
            if '"issue": 7' in naive.stdout or '"issue": 8' in naive.stdout:
                bad += fail("the naive (non-tolerant) jq recipe reached past the "
                            "damaged line in this test setup, so the contrast this "
                            "check relies on does not hold -- rebuild the fixture")
    finally:
        loop.record, loop.log, loop.invoke = real_record, real_log, real_invoke
        loop.JOURNAL, loop.LOGS = real_journal, real_logs
    return bad


def check_retro_first_pass_still_gets_tolerant_recipe() -> int:
    """#105: with no prior watermark, the prompt must still embed the tolerant recipe.

    A reviewer blocked an earlier fix over exactly this: the tolerant
    `tail`/`jq 'fromjson? // empty'` recipe was only emitted `if prev_through:`,
    so the very first retrospective against any journal -- and every journal
    that predates the `through` field, which is all of them the moment this
    merges, since orchestrator/logs/ is never cleaned -- got no read command at
    all and fell back to prompts/retrospective.md's plain, non-tolerant `jq`
    recipes. Those halt at the first malformed line, while `through` is still
    recorded as the full physical count -- silently blessing everything after
    a tear as analysed when the agent's own command never reached it. This
    drives retrospective() with an empty journal history (no prior
    `retrospective` record at all, so prev_through == 0) against a journal
    that itself contains a damaged line, and checks that the embedded command
    still exists, starts at line 1, and -- run for real -- reaches records on
    both sides of the tear.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    events: list[tuple[str, dict]] = []
    real_record, real_log, real_invoke = loop.record, loop.log, loop.invoke
    real_journal, real_logs = loop.JOURNAL, loop.LOGS
    loop.record = lambda event, **f: events.append((event, f))
    loop.log = lambda msg: None
    bad = 0
    captured: dict = {}
    try:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            logs = root / "orchestrator" / "logs"
            logs.mkdir(parents=True)
            journal = logs / "events.jsonl"

            fresh_before = [{"ts": "t", "event": "merged", "issue": 1},
                             {"ts": "t", "event": "gate_fail", "issue": 2}]
            damaged = '{"ts": "t", "event": "gate_fail", "issue": 3'   # torn: no closing brace
            fresh_after = [{"ts": "t", "event": "recovered", "issue": 4},
                           {"ts": "t", "event": "merged", "issue": 5}]
            lines = ([json.dumps(r) for r in fresh_before]
                     + [damaged]
                     + [json.dumps(r) for r in fresh_after])
            journal.write_text("\n".join(lines) + "\n")
            total_lines = len(lines)
            want_issues = [1, 2, 4, 5]

            loop.JOURNAL, loop.LOGS = journal, logs

            def fake_invoke(agent, wt, prompt, logdir, tag, role="implementer",
                            escalate=False):
                captured["prompt"] = prompt
                (logs / "retro-1.md").write_text("findings\n")
                return "claude", 0, "some analysis"

            loop.invoke = fake_invoke
            loop.retrospective(1)

            recs = [f for e, f in events if e == "retrospective"]
            if len(recs) != 1 or recs[0].get("through") != total_lines:
                bad += fail(f"want through={total_lines} (the physical line count, "
                            f"damaged line included), got {recs}")

            text = captured.get("prompt", "")
            blocks = re.findall(r"```sh\n(.*?)```", text, re.S)
            tolerant = [b for b in blocks if "fromjson" in b]
            if not tolerant:
                return bad + fail("with no prior watermark, the prompt embeds no "
                                  "tolerant jq/tail recipe at all -- the first "
                                  f"retrospective against any journal falls back to "
                                  f"prompts/retrospective.md's plain jq: {text[-1000:]!r}")
            cmd = tolerant[0].strip()
            off = re.search(r"tail -n \+(\d+)", cmd)
            if not off or int(off.group(1)) != 1:
                bad += fail(f"embedded command has the wrong tail offset: {cmd!r}, "
                            "want +1 (no prior watermark, so the whole journal is "
                            "new evidence)")

            proc = subprocess.run(["bash", "-c", cmd], cwd=root,
                                  capture_output=True, text=True, timeout=30)
            got = _decode_json_stream(proc.stdout)
            if got is None:
                bad += fail("the embedded command emitted output that is not a "
                            "clean stream of JSON objects; "
                            f"stdout:\n{proc.stdout}")
            elif [r.get("issue") for r in got] != want_issues:
                bad += fail(
                    f"the embedded command emitted issues "
                    f"{[r.get('issue') for r in got]}, want {want_issues}: the "
                    "damaged line should be skipped and both sides of it present; "
                    f"stdout:\n{proc.stdout}")
    finally:
        loop.record, loop.log, loop.invoke = real_record, real_log, real_invoke
        loop.JOURNAL, loop.LOGS = real_journal, real_logs
    return bad


def check_retro_final_does_not_inherit_a_stale_report() -> int:
    """#105: retrospective("final") must not claim coverage on someone else's report.

    "logs/retro-{cycle}.md exists and is non-empty" is only a truthful witness
    that this pass analysed something because the file was empty when the pass
    started. For a numbered cycle _claim_retro_cycle() guarantees that with
    O_CREAT|O_EXCL, but that is gated on `isinstance(cycle, int)` and the
    "final" cycle skips it. orchestrator/logs/ is gitignored and nothing ever
    cleans it (events.jsonl surviving across runs is load-bearing for
    _next_retro_cycle()), and scripts/supervise.sh restarts `loop.py run` --
    which ends in retrospective("final") -- so several final passes per
    supervised session is the normal path, and run 2 finds run 1's
    retro-final.md still on disk.

    Left unguarded, a run-2 final pass whose agent hits an API error -- which
    invoke() can surface as rc == 0 with non-empty text, since it drops the
    error reason _final_text() returns beside it -- has all three conditions
    satisfied, two of them by an artifact it did not produce, and records
    `through` for the entire journal. _prev_retro_through() takes the max, so
    every later pass in every later run is told that evidence was analysed.
    That is the direction #105 names as unacceptable: claiming too much loses
    evidence permanently.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    events: list[tuple[str, dict]] = []
    real_record, real_log, real_invoke = loop.record, loop.log, loop.invoke
    real_journal, real_logs = loop.JOURNAL, loop.LOGS
    loop.record = lambda event, **f: events.append((event, f))
    loop.log = lambda msg: None
    bad = 0
    try:
        with tempfile.TemporaryDirectory() as tmp:
            logs = Path(tmp)
            journal = logs / "events.jsonl"
            journal.write_text("".join(
                json.dumps({"ts": "t", "event": "merged", "issue": i}) + "\n"
                for i in range(1, 21)))
            loop.JOURNAL, loop.LOGS = journal, logs
            report = logs / "retro-final.md"
            report.write_text("findings from the previous supervised run\n")
            stale = report.read_text()

            # An API error arriving as a successful-looking result, writing
            # nothing. Only the leftover file could vouch for it.
            loop.invoke = lambda *a, **k: ("claude", 0, "API Error: Connection error.")
            loop.retrospective("final")

            recs = [f for e, f in events if e == "retrospective"]
            if len(recs) != 1:
                bad += fail(f"expected exactly one retrospective record, got {recs}")
            else:
                if "through" in recs[0]:
                    bad += fail(
                        f"retrospective('final') recorded through="
                        f"{recs[0]['through']!r} on a pass that produced no "
                        "analysis -- the only non-empty report on disk was left "
                        "by an earlier run, so this claims 20 lines nothing read")
                if recs[0].get("analysed") is not False:
                    bad += fail(f"want analysed=False on the failed final pass, "
                                f"got {recs[0]}")
            if report.read_text() != stale:
                bad += fail("the earlier run's retro-final.md was destroyed; the "
                            "freshness check must not cost the previous run its "
                            "findings")

            # ...and the same path must still record `through` when the final
            # pass really does write a report, or the guard has just broken the
            # feature instead of the bug.
            events.clear()

            def fake_invoke(*a, **k):
                report.write_text("real findings from this run\n")
                return "claude", 0, "some analysis"

            loop.invoke = fake_invoke
            loop.retrospective("final")
            recs = [f for e, f in events if e == "retrospective"]
            if len(recs) != 1 or recs[0].get("through") != 20:
                bad += fail(f"a final pass that did write its report must record "
                            f"through=20 (the journal's line count), got {recs}")
    finally:
        loop.record, loop.log, loop.invoke = real_record, real_log, real_invoke
        loop.JOURNAL, loop.LOGS = real_journal, real_logs
    return bad


# The whole surface cmd_run resolves as a module global at call time, and so
# the whole surface the harness below has to swap out -- and put back. Shared
# with check_rolling_pool_abandons_safely() so the two cannot drift: a name
# missing from one restore leaks a stub into the next check.
_LOOP_GLOBALS = ("gh", "work", "self_update_pending", "restart_into_new_code",
                 "record", "log", "retro_thread", "retrospective", "MAX_PARALLEL",
                 "WALLCLOCK_LIMIT", "DRAIN_CONFIRMATIONS", "_start")


def check_rolling_pool(join_timeout: float = 15.0) -> int:
    """A worker that finishes early must be able to take the next issue.

    retro-2.md Finding 1: cmd_run used to claim a batch of MAX_PARALLEL issues
    and then block on `f.result()` for every one of them before claiming
    again, so a worker that finished in minutes sat idle until the slowest
    member of its batch -- sometimes hours -- finished too. Measured over one
    run: ~50% of worker-time idle and 321 issue-minutes of queue latency,
    entirely self-inflicted -- the critic/resolver/reviewer stages were
    behaving correctly and finishing fast; the batch barrier is what turned
    their speed into idle time.

    A regex proving `f.result()` is gone cannot tell a correct rewrite from
    one that over-claims, drains early or deadlocks, so this drives the real
    cmd_run() against a fake queue and a stubbed work(), and asserts on what
    it actually does. Every name cmd_run touches is a module global read at
    call time -- loop.gh, loop.work, loop.self_update_pending,
    loop.restart_into_new_code, loop.record, loop.log, loop.retro_thread,
    loop.retrospective, loop.MAX_PARALLEL, loop.WALLCLOCK_LIMIT,
    loop.DRAIN_CONFIRMATIONS and loop._start -- which is what makes this
    possible without a worktree, an agent or a network call. It runs on a
    daemon thread with a join timeout: a rolling pool that deadlocks must fail
    this check, not hang the gate. `join_timeout` is a parameter only so
    check_rolling_pool_abandons_safely() can reach the expiry branch cheaply.

    Only the two properties #40 is scoped to: a fourth issue claimed before
    the slow one finishes (the barrier this issue removes), and claims
    outstanding never exceeding MAX_PARALLEL (the failure mode on the other
    side of the same fix -- the claim budget forgetting to subtract
    len(inflight)). Quiescent self-update, early-drain-while-busy, the
    wallclock drain and the busy-worker exclusion are #125's; #40 only had to
    implement those invariants in cmd_run, not prove them here.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import gh
    import loop

    class FakeGh:
        """Every method cmd_run (and the recover_blocked/sync_waiting it
        calls) touches, with no network. `ready` is the only state a scenario
        drives: claim() pops from it and records when; a work stub can push a
        new issue back mid-run via requeue() to simulate a resolver's REWRITE.
        """
        Issue = gh.Issue

        def __init__(self, issues):
            self.ready = {i.number: i for i in issues}
            self.claims: list[tuple[float, int]] = []

        def ensure_labels(self) -> None:
            pass

        def fetch(self, label: str | None = None, state: str = "open"):
            if state == "closed":
                return []
            if label in (None, "fr:ready"):
                return list(self.ready.values())
            return []   # fr:blocked / fr:claimed / fr:questioned: none, ever

        def claimable(self):
            return sorted(self.ready.values(), key=lambda i: i.number)

        def claim(self, n: int) -> bool:
            if n not in self.ready:
                return False
            del self.ready[n]
            self.claims.append((time.time(), n))
            return True

        def requeue(self, issue) -> None:
            self.ready[issue.number] = issue

        def release(self, n: int, label: str = "fr:ready") -> None:
            pass

        def sync_waiting(self) -> tuple[int, int]:
            return (0, 0)

        def blocked_needing_recovery(self):
            return []

        def closed_numbers(self):
            return set()

        def comment(self, n: int, body: str) -> None:
            pass

        def close(self, n: int, summary: str) -> None:
            pass

        def block(self, n: int, why: str) -> None:
            pass

    def issue(n: int) -> gh.Issue:
        return gh.Issue(number=n, title=f"issue {n}",
                        body="Gate: bootstrap\nAgent: claude\n", labels=["fr:ready"])

    saved = {name: getattr(loop, name) for name in _LOOP_GLOBALS}

    def drive(fake, work, *, max_parallel: int, wallclock: float,
              join_timeout: float, self_update_pending=None,
              restart_into_new_code=None, drain_confirmations: int = 3) -> dict:
        loop.gh = fake
        loop.work = work
        loop.self_update_pending = self_update_pending or (lambda: False)
        loop.restart_into_new_code = restart_into_new_code or (lambda: None)
        loop.record = lambda event, **f: None
        loop.log = lambda msg: None
        loop.retro_thread = lambda: None
        loop.retrospective = lambda cycle: None
        loop.MAX_PARALLEL = max_parallel
        loop.WALLCLOCK_LIMIT = wallclock
        loop.DRAIN_CONFIRMATIONS = drain_confirmations
        loop._start = time.time()
        result: dict = {}

        def runner() -> None:
            result["rc"] = loop.cmd_run()

        t = threading.Thread(target=runner, daemon=True)
        t.start()
        t.join(timeout=join_timeout)
        return {"alive": t.is_alive(), **result}

    bad = 0
    abandoned = False
    try:
        # One issue ten times slower than the rest, sized so the fixed loop's
        # own drain-confirmation sleeps (20-30s, untouched by #40 and not this
        # check's concern) never get a chance to run -- the wallclock check at
        # the top of the next iteration catches the run first.
        SLOW, FAST = 3.0, 0.3
        dur = {1: SLOW, **{n: FAST for n in range(2, 8)}}
        finishes: list[tuple[float, int]] = []

        def work_(iss) -> None:
            time.sleep(dur[iss.number])
            finishes.append((time.time(), iss.number))

        fake = FakeGh([issue(n) for n in dur])
        out = drive(fake, work_, max_parallel=3, wallclock=1.5,
                    join_timeout=join_timeout)

        if out["alive"]:
            # The join expired, so this thread is no longer ours to stop -- and
            # cmd_run re-reads gh, work, record, MAX_PARALLEL and the wallclock
            # as module globals on *every* pass. Putting the real ones back in
            # the finally: below would hand a thread nobody controls the live
            # orchestrator: a real gh.sync_waiting() label write, a real
            # gh.claim() of somebody's fr:ready issue, and a real work() ->
            # make_worktree(n), whose `git branch -D issue/<n>` destroys the
            # unmerged attempt preserve_branch exists to keep -- then a real
            # agent. Interpreter shutdown would then block on it: the executor
            # is still held open by this thread, and _python_exit joins every
            # thread in _threads_queues, which daemon status does not exempt.
            #
            # So leave the fakes installed. They are pure in-memory, and their
            # WALLCLOCK_LIMIT has already expired, so the abandoned thread
            # hits cmd_run's wallclock break on its next pass and unwinds on
            # its own; if it is instead wedged on a lock, the only thing exit
            # can wait on is a work_ stub that sleeps at most SLOW seconds. Nothing
            # later in the gate reads loop's globals
            # (check_rolling_pool_abandons_safely runs first and restores its
            # own), so nothing downstream is owed the real ones.
            abandoned = True
            bad += fail("rolling pool deadlocked (1 slow + 6 fast issues, "
                        "max_parallel=3) -- did not finish within the join "
                        "timeout; loop's globals left stubbed so the abandoned "
                        "thread cannot reach real GitHub or a real agent")
            return bad   # nothing below is safe to assert with the thread still live

        claims = sorted(fake.claims)
        if len(claims) < 4:
            bad += fail(f"rolling pool only claimed {len(claims)}/7 issues; "
                        f"claims={claims} finishes={sorted(finishes)}")
            return bad

        # Rolling: a 4th issue must be claimed strictly before the slow one
        # (#1) finishes. Under the batch barrier this is impossible by
        # construction -- claimable() cannot run again until every future in
        # the batch has had f.result() called on it -- so this reproduces
        # retro-2.md Finding 1 directly against the unfixed code.
        slow_finish = next((t for t, n in finishes if n == 1), None)
        fourth_claim = claims[3][0]
        if slow_finish is None:
            bad += fail("the slow issue (#1) never finished")
        elif fourth_claim >= slow_finish:
            bad += fail("a 4th issue was not claimed until the slow issue finished -- "
                        f"this is the batch barrier: claims={claims} "
                        f"finishes={sorted(finishes)}")

        # No over-claiming: at no instant may claims outstanding (claimed but
        # not yet finished) exceed max_parallel. The failure mode on the other
        # side of the same fix -- the claim budget forgetting to subtract
        # len(inflight) -- over-subscribes the pool and strands the excess in
        # fr:claimed with no worker, since ThreadPoolExecutor just queues
        # submissions past max_workers instead of running them.
        events = sorted([(t, 1) for t, _ in fake.claims] +
                        [(t, -1) for t, _ in finishes])
        outstanding = peak = 0
        for _, delta in events:
            outstanding += delta
            peak = max(peak, outstanding)
        if peak > 3:
            bad += fail(f"claims outstanding peaked at {peak} > max_parallel=3: "
                        f"claims={claims} finishes={sorted(finishes)}")

        def bail_if_abandoned(out: dict, desc: str) -> bool:
            """Shared with the scenario above: same daemon-thread-plus-join-timeout
            contract, same reason for leaving loop's globals stubbed rather than
            restored -- see the comment on `if out["alive"]:` above.
            check_rolling_pool_abandons_safely() only has to see the FIRST
            scenario honour this contract (a stubbed cmd_run that never returns
            makes every drive() call here hang, so it never reaches the rest of
            this function) -- every scenario after it follows the same rule so
            that a hang anywhere in this file fails loud instead of leaking a
            real gh/work into a thread nobody controls.
            """
            nonlocal bad, abandoned
            if not out["alive"]:
                return False
            abandoned = True
            bad += fail(f"{desc} -- did not finish within its join timeout")
            return True

        # ---- scenario: quiescent self-update (#125) ----
        # self_update_pending() flips True the instant the first issue is
        # claimed. A correct cmd_run must then claim nothing further -- two
        # more issues are sitting claimable, so "nothing left to claim" cannot
        # be why -- drain the one in-flight issue to zero, call
        # restart_into_new_code() exactly once with the pool empty, and stop.
        # max_parallel=1 so exactly one claim happens before the flip is even
        # visible to cmd_run, which is what makes "claimed after the flip"
        # unambiguous.
        #
        # Catches: the quiesce check missing or never firing (claims keep
        # coming after the flip), restarting before the in-flight worker has
        # actually finished (restart_into_new_code called with the pool
        # non-empty), and a missing `break` after the restart call (it fires
        # more than once, or spins forever and this scenario's own join
        # timeout catches it as a hang).
        dur_a = {1: 0.3, 2: 0.5, 3: 0.2}
        finishes_a: list[tuple[float, int]] = []
        fake_a = FakeGh([issue(n) for n in dur_a])
        restart_calls: list[int] = []

        def work_a(iss: gh.Issue) -> None:
            time.sleep(dur_a[iss.number])
            finishes_a.append((time.time(), iss.number))

        def self_update_pending_a() -> bool:
            return len(fake_a.claims) >= 1

        def restart_into_new_code_a() -> None:
            restart_calls.append(len(fake_a.claims) - len(finishes_a))

        out_a = drive(fake_a, work_a, max_parallel=1, wallclock=5.0,
                      join_timeout=5.0, self_update_pending=self_update_pending_a,
                      restart_into_new_code=restart_into_new_code_a,
                      drain_confirmations=1)
        if bail_if_abandoned(out_a, "quiescent self-update scenario deadlocked"):
            return bad

        if len(fake_a.claims) != 1:
            bad += fail(f"self_update_pending() flipped after the first claim "
                        f"but {len(fake_a.claims)} issue(s) were claimed, not 1: "
                        f"{fake_a.claims} -- quiesce must stop new claims")
        if sorted(fake_a.ready) != [2, 3]:
            bad += fail(f"quiescent self-update left {sorted(fake_a.ready)} "
                        "claimable, want [2, 3] left untouched -- this scenario "
                        "proves nothing unless the queue still had work to "
                        "(wrongly) claim")
        if restart_calls != [0]:
            bad += fail(f"restart_into_new_code() should fire exactly once with "
                        f"the pool drained to zero; recorded in-flight counts "
                        f"at call time were {restart_calls}, want [0]")

        # ---- scenario: no early drain while a worker is busy (#125) ----
        # Issue 1 is claimed, the queue immediately goes empty (nothing else
        # was ever seeded), and issue 1's own worker -- still "running" as far
        # as cmd_run is concerned -- requeues a brand new issue before it
        # returns. This is resolve_question()'s RESOLUTION: REWRITE path
        # (its `gh.requeue()` call, made from inside _work() before the
        # worker returns). drain_confirmations=1 so a broken implementation
        # that miscounts the busy window as an empty poll exits on its very
        # first wrong empty read instead of being saved by extra confirmations
        # it does not deserve, and so a correct one still exits promptly.
        #
        # Catches: checking "is the queue empty" before checking "is a worker
        # still running" -- i.e. counting the momentarily-empty ready list
        # against DRAIN_CONFIRMATIONS while inflight is non-empty, which logs
        # "queue drained" and exits before the running worker ever hands back
        # issue 2.
        fake_b = FakeGh([issue(1)])
        finishes_b: list[tuple[float, int]] = []

        def work_b(iss: gh.Issue) -> None:
            if iss.number == 1:
                time.sleep(0.35)
                fake_b.requeue(issue(2))
            else:
                time.sleep(0.15)
            finishes_b.append((time.time(), iss.number))

        out_b = drive(fake_b, work_b, max_parallel=1, wallclock=5.0,
                      join_timeout=5.0, drain_confirmations=1)
        if bail_if_abandoned(out_b, "no-early-drain scenario deadlocked"):
            return bad

        claimed_b = sorted(n for _, n in fake_b.claims)
        if claimed_b != [1, 2]:
            bad += fail(f"no-early-drain scenario claimed {claimed_b}, want "
                        "[1, 2] -- issue 2 (requeued by issue 1's own worker "
                        "while it was still running) was never picked up; the "
                        "loop must have read the momentarily-empty queue as drained")
        finished_b = sorted(n for _, n in finishes_b)
        if finished_b != [1, 2]:
            bad += fail(f"no-early-drain scenario: {finished_b} ran to "
                        f"completion, want [1, 2]")

        # ---- scenario: wallclock stop drains in-flight work (#125) ----
        # Two issues claimed immediately fill the pool (max_parallel=2); two
        # more sit claimable and untouched. The wallclock limit expires while
        # issue 2 is still running. A correct cmd_run stops claiming -- issues
        # 3 and 4 stay in fake_c.ready forever -- but must NOT cancel issue 2:
        # preserve_branch/merge_worktree mean an unmerged attempt is the only
        # copy of itself (commit 8dd6f46), so cancelling it destroys work
        # outright rather than merely delaying it.
        #
        # Catches: the wallclock check being skipped or inverted (3 and/or 4
        # would get claimed too), and a wallclock stop that abandons in-flight
        # futures instead of draining them (issue 2 would never reach
        # finishes_c, and drive() would return in well under issue 2's own
        # duration).
        dur_c = {1: 0.45, 2: 1.1, 3: 0.15, 4: 0.15}
        finishes_c: list[tuple[float, int]] = []
        fake_c = FakeGh([issue(n) for n in dur_c])

        def work_c(iss: gh.Issue) -> None:
            time.sleep(dur_c[iss.number])
            finishes_c.append((time.time(), iss.number))

        t0_c = time.time()
        out_c = drive(fake_c, work_c, max_parallel=2, wallclock=0.25,
                      join_timeout=6.0, drain_confirmations=1)
        elapsed_c = time.time() - t0_c
        if bail_if_abandoned(out_c, "wallclock-drain scenario deadlocked"):
            return bad

        claimed_c = sorted(n for _, n in fake_c.claims)
        if claimed_c != [1, 2]:
            bad += fail(f"wallclock-drain scenario claimed {claimed_c}, want "
                        "[1, 2] -- claiming must stop at the limit even though "
                        "issues 3 and 4 were still sitting ready")
        if sorted(fake_c.ready) != [3, 4]:
            bad += fail(f"wallclock-drain scenario left {sorted(fake_c.ready)} "
                        "claimable, want [3, 4] left untouched")
        finished_c = sorted(n for _, n in finishes_c)
        if finished_c != [1, 2]:
            bad += fail(f"wallclock-drain scenario: {finished_c} ran to "
                        "completion, want [1, 2] -- issue 2 was still in flight "
                        "when the limit hit and must not be cancelled")
        if elapsed_c < dur_c[2] - 0.2:
            bad += fail(f"wallclock-drain scenario returned after {elapsed_c:.2f}s, "
                        f"faster than issue 2's own {dur_c[2]}s duration -- the "
                        "in-flight future was abandoned, not drained")

        # ---- scenario: busy self-requeue exclusion (#125) ----
        # Issue 1's own worker requeues issue 1 itself partway through -- the
        # same RESOLUTION: REWRITE shape as the no-early-drain scenario above,
        # except this time it is the SAME issue number reappearing in
        # fake_d.ready while its own future is still inflight, not a new one.
        # loop.py's `busy` set (built from inflight.values() just above the
        # claim loop) exists for exactly this: reclaiming it here would call
        # make_worktree(1) a second time and force-remove the worktree and
        # branch out from under the still-running first attempt. Issues 2 and
        # 3 free up pool slots at different times so there is always spare
        # budget to (wrongly) reclaim issue 1 with while it is still busy, and
        # issue 4 keeps the pool full a while longer so the busy window is not
        # just "nothing else was claimable anyway" (the single-issue-queue
        # trap: a queue with nothing else in it cannot tell exclusion from
        # coincidence).
        #
        # Catches: a missing or reverted `busy` filter (issue 1 gets reclaimed
        # while its own future is still running, strictly before its first
        # completion timestamp), and a `busy` filter that never releases
        # (issue 1 would be claimed only once total instead of twice).
        requeued_d = {"done": False}
        finishes_d: list[tuple[float, int]] = []
        fake_d = FakeGh([issue(1), issue(2), issue(3), issue(4)])

        def work_d(iss: gh.Issue) -> None:
            if iss.number == 1 and not requeued_d["done"]:
                requeued_d["done"] = True
                time.sleep(0.4)
                fake_d.requeue(issue(1))
                time.sleep(0.8)
            elif iss.number == 2:
                time.sleep(0.2)
            elif iss.number == 3:
                time.sleep(0.6)
            elif iss.number == 4:
                time.sleep(0.7)
            else:  # issue 1's second run, after its own worker has retired
                time.sleep(0.15)
            finishes_d.append((time.time(), iss.number))

        out_d = drive(fake_d, work_d, max_parallel=3, wallclock=5.0,
                      join_timeout=6.0, drain_confirmations=1)
        if bail_if_abandoned(out_d, "busy-exclusion scenario deadlocked"):
            return bad

        claimed_nums_d = sorted(n for _, n in fake_d.claims)
        if claimed_nums_d != [1, 1, 2, 3, 4]:
            bad += fail(f"busy-exclusion scenario claimed {claimed_nums_d}, "
                        "want [1, 1, 2, 3, 4] -- issue 1 must be claimed once "
                        "up front and once again after it fully retires")
        else:
            claim1_times = sorted(t for t, n in fake_d.claims if n == 1)
            finish1_times = sorted(t for t, n in finishes_d if n == 1)
            if len(finish1_times) != 2:
                bad += fail(f"busy-exclusion scenario: issue 1 finished "
                            f"{len(finish1_times)} time(s), want 2")
            elif claim1_times[1] < finish1_times[0]:
                bad += fail("busy-exclusion scenario: issue 1 was re-claimed "
                            f"at {claim1_times[1]:.3f} before its own first run "
                            f"finished at {finish1_times[0]:.3f} -- it was "
                            "reclaimed while its own worker was still busy")
    finally:
        if not abandoned:
            for name, val in saved.items():
                setattr(loop, name, val)

    return bad


def check_rolling_pool_abandons_safely() -> int:
    """check_rolling_pool()'s deadlock branch must not rearm the orchestrator.

    That check drives the real cmd_run on a daemon thread with a join timeout,
    on the contract that a pool which deadlocks fails the gate instead of
    hanging it. But the thread outliving the join is exactly the case where the
    harness has lost control of it, and cmd_run resolves gh, work, record,
    MAX_PARALLEL and the wallclock as module globals on every pass. Restoring
    the real ones on that path gives a runaway thread real label writes, a real
    claim, and a real work() whose make_worktree() force-removes branch
    issue/<n> -- and since the abandoned thread still holds the executor, and
    interpreter shutdown joins pool workers whether or not their owner is a
    daemon, the gate then blocks on that agent rather than exiting. Worse than
    a hang: it mutates the queue it exists to simulate.

    This check runs first, so the globals it captures are the real ones, and it
    restores them itself. #125 adds four more scenarios to this same harness,
    each with its own join timeout, so this branch only gets more load-bearing.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    real = {name: getattr(loop, name) for name in ("cmd_run", *_LOOP_GLOBALS)}
    release = threading.Event()

    def never_returns() -> int:
        release.wait(30)   # a cmd_run that outlives the join; bounded anyway
        return 0

    loop.cmd_run = never_returns
    bad = 0
    try:
        # Its own failure message is the expected output here, not gate noise.
        with contextlib.redirect_stdout(io.StringIO()):
            rc = check_rolling_pool(join_timeout=0.5)
        if rc == 0:
            bad += fail("check_rolling_pool() passed against a cmd_run that never "
                        "returns -- its deadlock branch no longer detects a hang")
        if loop.gh is real["gh"] or loop.work is real["work"]:
            bad += fail("check_rolling_pool() restored the real gh/work while the "
                        "thread it drives was still inside cmd_run: an abandoned "
                        "thread would claim a live fr:ready issue, delete its "
                        "issue/<n> branch and run an unsupervised agent")
    finally:
        release.set()
        for name, val in real.items():
            setattr(loop, name, val)

    return bad


if __name__ == "__main__":
    bad = check_parses()
    if bad:                      # do not try to run code that does not parse
        sys.exit(1)
    sys.exit(1 if check_runs() + check_prompts() + check_dep_parsing()
             + check_gate_targets_the_worktree() + check_no_absorbing_states()
             + check_blocked_has_a_recovery_path() + check_waiting_is_annotation_only()
             + check_silent_reviewer_is_not_a_pass() + check_pre_implementer_stages_restore()
             + check_unmerged_work_survives_reclaim()
             + check_filing_contract_is_stated() + check_reviewer_restore()
             + check_verdict_parsing() + check_record_repairs_torn_journal()
             + check_retro_cycle_survives_restart()
             + check_retro_callers_derive_the_cycle()
             + check_retro_no_clobber() + check_retro_cycle_claim_is_atomic()
             + check_retro_orphaned_claim_does_not_skew_forever()
             + check_retro_through_is_snapshotted_before_invoke()
             + check_retro_unanalysed_pass_does_not_advance_through()
             + check_retro_damaged_line_and_tolerant_prompt_command()
             + check_retro_first_pass_still_gets_tolerant_recipe()
             + check_retro_final_does_not_inherit_a_stale_report()
             + check_rolling_pool_abandons_safely() + check_rolling_pool() else 0)
