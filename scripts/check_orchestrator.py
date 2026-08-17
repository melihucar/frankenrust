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
import os
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


def check_review_stage_retries_a_silent_reviewer() -> int:
    """One silent reviewer must not be read as consent -- it must be retried.

    check_silent_reviewer_is_not_a_pass pins review_outcome()'s classification:
    one silent, one passing reviewer is correctly *not* a block. #139 is that
    nothing acted on that classification -- review_stage() logged
    review_incomplete and returned the pass anyway, so a reviewer killed by the
    60-minute AGENT_TIMEOUT (loop.py:440) was read as two reviews having
    happened. #40 got a 1,006-line rewrite of the orchestrator's own scheduling
    loop cleared for merge on one review; only an unrelated rebase conflict
    stopped it landing.

    This exercises review_stage() itself (not just review_outcome) against a
    real git worktree, with loop.invoke stubbed so the "reviewer" is scripted
    by tag: a silent reviewer 1 must be retried exactly once, into a
    `review1.<tag>.retry` tag, before the merge path is reached -- and a
    reviewer that passed on the first round must never be re-invoked.

    Two further shapes, each a defect the reviewers of this change caught and
    no test did:

      * The closing comment must name the agent that *ran*, not the one the
        REVIEWER_AGENTS roster asked for. resolve() turns either slot's
        "opencode" into claude the instant the quota latch is armed, so naming
        from the roster credits the diff to a vendor that never opened it -- on
        a latched run, #40's own {1: silent, 2: pass} read out as "one
        adversarial review (opencode)" when claude died and claude passed. The
        stub therefore resolves its return value exactly as invoke() does.
      * A reviewer whose invoke() *raises* -- a renamed agent binary is a bare
        Popen FileNotFoundError, and invoke() is the one Popen site with no
        guard -- must be retried and disclosed like any other reviewer that
        did not report. It used to be absent from `results` rather than silent
        in it, so it was neither.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import gh
    import loop

    def sh(wt: Path, *args: str) -> None:
        subprocess.run(["git", *args], cwd=wt, check=True, capture_output=True, text=True)

    def make_repo(wt: Path) -> None:
        wt.mkdir(parents=True, exist_ok=True)
        sh(wt, "init", "-q", "-b", "main")
        sh(wt, "config", "user.email", "check_orchestrator@example.com")
        sh(wt, "config", "user.name", "check_orchestrator")
        (wt / "a.txt").write_text("base\n")
        sh(wt, "add", "-A")
        sh(wt, "commit", "-q", "-m", "base")
        (wt / "a.txt").write_text("base\nunder review\n")   # the diff to review

    issue = gh.Issue(number=139, title="a reviewer killed by the timeout", body="body")

    real_invoke, real_record, real_log = loop.invoke, loop.record, loop.log
    real_codex_ok = loop.codex_ok
    real_opencode_ok = loop.opencode_ok
    # Captured rather than dropped: a reviewer that dies has to leave evidence
    # in the journal the retrospective reads, not just a traceback on stderr.
    events: list[tuple[str, dict]] = []
    loop.record = lambda event, **f: events.append((event, f))
    loop.log = lambda msg: None

    def run_case(script: dict, latched: bool = False) -> tuple[tuple, list[str]]:
        """Script a round by tag. A BaseException value is raised, not returned.

        `latched` pins codex_ok()/opencode_ok() rather than inheriting the
        FR_*_DISABLED envs from whatever the ambient run happens to be, so
        both sides of the fallback are covered deterministically.
        """
        calls: list[str] = []
        events.clear()

        def stub(agent, wt, prompt, logdir, tag, role="implementer", escalate=False,
             model=None):
            calls.append(tag)
            scripted = script.get(tag, "")
            if isinstance(scripted, BaseException):
                raise scripted
            # invoke()'s first return value is the agent that ran *after*
            # resolve()'s cheap-agent->claude fallback, not the one requested.
            return loop.resolve(agent, "reviewer")[0], 0, scripted

        loop.invoke = stub
        loop.codex_ok = lambda: not latched
        loop.opencode_ok = lambda: not latched
        with tempfile.TemporaryDirectory() as tmp:
            wt = Path(tmp) / "repo"
            make_repo(wt)
            logdir = Path(tmp) / "logs"
            logdir.mkdir()
            result = loop.review_stage(issue, wt, logdir, "1")
        return result, calls

    bad = 0
    try:
        # Reviewer 1 goes silent on the first pass, then blocks on the retry.
        # Reviewer 2 passes immediately and must not be re-invoked.
        (blocking, verdicts, reviewers), calls = run_case({
            "review1.1": "",
            "review2.1": "Looks correct.\nVERDICT: PASS",
            "review1.1.retry": "Found a real defect.\n\nVERDICT: BLOCK",
        })
        if calls.count("review1.1.retry") != 1:
            bad += fail(f"a silent reviewer must be retried exactly once "
                        f"before the merge path is reached; calls were {calls}")
        if "review2.1.retry" in calls:
            bad += fail(f"reviewer 2 passed on the first round and was retried "
                        f"anyway: {calls}")
        if not blocking or "VERDICT: BLOCK" not in blocking:
            bad += fail(f"a BLOCK produced on the retry did not block the "
                        f"diff: {blocking!r}")
        if verdicts.get(1) != "block":
            bad += fail(f"the retried reviewer's BLOCK verdict is missing "
                        f"from the final verdicts: {verdicts}")

        # Reviewer 1 stays silent even after the retry. The round must not
        # block -- review_outcome's classification of "one silent, one
        # passing" is not itself the bug -- but it must come back marked as
        # one review, not two, for _work() to hand to gh.close(), and it must
        # credit the review to the agent that actually produced it.
        still_silent = {
            "review1.1": "",
            "review2.1": "Looks correct.\nVERDICT: PASS",
            "review1.1.retry": "",
        }
        (blocking, verdicts, reviewers), calls = run_case(still_silent)
        if blocking:
            bad += fail(f"a reviewer still silent after its one retry must "
                        f"not block a diff the other reviewer passed: {blocking!r}")
        if calls.count("review1.1.retry") != 1:
            bad += fail(f"a reviewer still silent after retry was retried "
                        f"again instead of the round being scored final: {calls}")
        if verdicts != {1: "silent", 2: "pass"}:
            bad += fail(f"final verdicts do not show reviewer 1 as still "
                        f"silent: {verdicts}")
        summary = loop.review_summary(verdicts, reviewers)
        if "two adversarial" in summary or "one adversarial review" not in summary:
            bad += fail(f"review_summary() must say one review happened, not "
                        f"two, when a reviewer stayed silent through the "
                        f"retry: {summary!r}")
        if "opencode" not in summary:
            bad += fail(f"opencode really did produce the surviving review here "
                        f"and must be named: {summary!r}")

        # The same round with the opencode quota latch armed. Both slots
        # asked for opencode, so both slots fall back to claude, and the
        # surviving review is claude's -- the sentence must say so: telling
        # the morning reader a second vendor read a diff it never saw is
        # #40's false record one field over.
        (blocking, verdicts, reviewers), calls = run_case(still_silent, latched=True)
        if reviewers != {1: "claude", 2: "claude"}:
            bad += fail(f"review_stage must report the agents that ran, and "
                        f"under the latch both slots are claude -- a reviewer "
                        f"that timed out still returns from invoke(): {reviewers}")
        summary = loop.review_summary(verdicts, reviewers)
        if "opencode" in summary or "claude" not in summary:
            bad += fail(f"with opencode walled off, the surviving reviewer was "
                        f"claude; the closing comment must not credit opencode: "
                        f"{summary!r}")

        # A reviewer that dies hard rather than quietly: invoke() raises, the
        # thread unwinds, and nothing ever assigns results[1]. It must still
        # count as a reviewer that owes a verdict -- retried, and its BLOCK on
        # the retry honoured.
        (blocking, verdicts, reviewers), calls = run_case({
            "review1.1": FileNotFoundError(2, "No such file or directory: 'claude'"),
            "review2.1": "Looks correct.\nVERDICT: PASS",
            "review1.1.retry": "Found a real defect.\n\nVERDICT: BLOCK",
        })
        if calls.count("review1.1.retry") != 1:
            bad += fail(f"a reviewer whose invoke() raised was not retried; it "
                        f"is absent from the verdicts, not silent in them: {calls}")
        if not blocking or "VERDICT: BLOCK" not in blocking:
            bad += fail(f"the retry of a hard-dead reviewer produced a BLOCK "
                        f"that did not block the diff: {blocking!r}")
        if not [f for e, f in events
                if e == "agent_error" and f.get("tag") == "review1.1"]:
            bad += fail(f"a reviewer that raised left nothing in the journal "
                        f"but a traceback on stderr: {events}")

        # ...and if it dies both times, the merge is one review, not two.
        (blocking, verdicts, reviewers), calls = run_case({
            "review1.1": FileNotFoundError(2, "No such file or directory: 'claude'"),
            "review2.1": "Looks correct.\nVERDICT: PASS",
            "review1.1.retry": OSError("fork failed"),
        })
        if blocking:
            bad += fail(f"a hard-dead reviewer must not block a diff the other "
                        f"reviewer passed: {blocking!r}")
        if verdicts != {1: "silent", 2: "pass"}:
            bad += fail(f"a reviewer whose invoke() raised must appear in the "
                        f"verdicts as silent, not vanish from them: {verdicts}")
        summary = loop.review_summary(verdicts, reviewers)
        if "two adversarial" in summary:
            bad += fail(f"a round where one reviewer never ran at all closed "
                        f"claiming two adversarial reviews: {summary!r}")

        # review_summary reads the round, never a run-global latch: codex may
        # have reviewed this diff perfectly well and been walled off by another
        # worker twenty minutes later, and it must still be credited.
        loop.codex_ok = lambda: False
        both = {1: "pass", 2: "pass"}
        summary = loop.review_summary(both, {1: "claude", 2: "codex"})
        if "claude + codex" not in summary:
            bad += fail(f"codex reviewed this round and was latched off after "
                        f"it; the closing comment must still credit it: {summary!r}")
        summary = loop.review_summary(both, {1: "claude", 2: "claude"})
        if "both claude" not in summary:
            bad += fail(f"two claude reviewers must not be reported as "
                        f"cross-vendor review: {summary!r}")
    finally:
        loop.invoke, loop.record, loop.log = real_invoke, real_record, real_log
        loop.codex_ok = real_codex_ok
        loop.opencode_ok = real_opencode_ok
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
    silently inheriting `default` and `opencode`. A docs fix that inherits `default`
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

    # opencode streams NDJSON events, so the same discipline has a different
    # shape: the verdict is the last `text` event, and an `error` event must
    # invalidate whatever text came before it -- opencode exits 0 even on a
    # failed run, so the log is the only place a quota wall shows up.
    def opencode_parsed(tmp: Path, events: list[dict], final: str) -> str:
        log = tmp / "opencode.review2.1.log"
        log.write_text("\n".join(json.dumps(e) for e in events) + "\n")
        text, err = loop._final_text(log, "opencode", None)
        return text if text else f"<no text: {err}>"

    with tempfile.TemporaryDirectory() as tmp:
        d = Path(tmp)
        echo = [{"type": "text", "part": {"type": "text",
                "text": "I will check for VERDICT: BLOCK conditions"}},
                {"type": "tool", "part": {"type": "tool", "tool": "bash"}}]
        cases = [
            ("a PASS whose earlier text quotes BLOCK", echo + [{"type": "text",
             "part": {"type": "text", "text": "Checked every path. Nothing "
             "blocking.\n\nVERDICT: PASS"}}], "VERDICT: PASS", "VERDICT: BLOCK"),
            ("a real BLOCK still blocks", echo + [{"type": "text",
             "part": {"type": "text", "text": "### leaks a subscriber\nFile: "
             "a.rs:1\n\nVERDICT: BLOCK"}}], "VERDICT: BLOCK", None),
            ("a critic PROCEED whose earlier text quotes REVISE", echo +
             [{"type": "text", "part": {"type": "text", "text": "The spec "
             "matches the oracle.\n\nVERDICT: PROCEED"}}], "VERDICT: PROCEED",
             "VERDICT: REVISE"),
        ]
        for name, events, want, unwanted in cases:
            got = opencode_parsed(d, events, "")
            if want not in got:
                bad += fail(f"opencode verdict parsing lost {want!r} on "
                            f"{name}: {got[:120]!r}")
            if unwanted and unwanted in got:
                bad += fail(f"opencode verdict parsing read {unwanted!r} out "
                            f"of {name} -- earlier text events are being read "
                            "as the verdict")

        # An error event after a clean-looking text event must void the text:
        # a quota wall is a harness fault, not a finding.
        got = opencode_parsed(d, echo + [{"type": "error", "error": {
            "name": "UnknownError",
            "data": {"message": "Rate limit exceeded, please try again"}}}], "")
        if "VERDICT" in got:
            bad += fail(f"an opencode run that ended in an error event parsed "
                        f"as a verdict: {got[:120]!r}")

        # A dead run -- no text, no error -- is not a verdict either.
        got = opencode_parsed(d, [{"type": "step_start",
                                   "part": {"type": "step-start"}}], "")
        if "VERDICT" in got:
            bad += fail(f"an opencode run with no final message parsed as a "
                        f"verdict: {got[:120]!r}")

    # And the wiring that makes the verdicts reachable in the first place.
    cmd = loop.agent_cmd("codex", None, Path("/tmp/fr-last.txt"))
    if "-o" not in cmd or "/tmp/fr-last.txt" not in cmd:
        bad += fail("agent_cmd no longer asks codex for its last message (-o); "
                    "verdicts fall back to transcript scraping")
    cmd = loop.agent_cmd("opencode", "opencode/deepseek-v4-flash-free",
                         Path("/tmp/fr-last.txt"))
    for flag in ("--format", "json", "--auto", "-"):
        if flag not in cmd:
            bad += fail(f"agent_cmd no longer asks opencode for its event "
                        f"stream ({flag}); verdicts are unreadable")
    if "opencode/deepseek-v4-flash-free" not in cmd:
        bad += fail("agent_cmd does not pass opencode's model through; every "
                    "run falls back to the configured default")
    return bad


def check_agent_routing() -> int:
    """resolve() must route requested agents to their agents/models.

    The table this exercises is config.py, overlaid with whatever .env / FR_*
    knobs the ambient run carries -- so every case here requests agents
    explicitly (routing must hold under any override), and the committed
    defaults themselves are pinned separately in a subprocess with the
    environment stripped, by check_config_defaults().

    This is the table every stage in the loop trusts: a wrong route here
    silently moves implementation onto Opus (costs money for no gain) or a
    reviewer onto a model the merge gate never meant to rely on, and neither
    failure leaves a trace in the journal.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop
    bad = 0

    # Routing table: (requested, role, escalate, latch_agent, want_agent,
    # want_model, model_override).
    cases = [
        ("claude", "implementer", False, None, "claude", loop.MODELS["implementer"],
         None),
        ("opencode", "implementer", False, None, "opencode",
         loop.OPENCODE_MODELS["implementer"], None),
        ("opencode", "reviewer", False, None, "opencode",
         loop.OPENCODE_MODELS["reviewer"], None),
        ("codex", "implementer", False, None, "codex", None, None),
        ("claude", "implementer", True, None, "claude", loop.ESCALATED_MODEL, None),
        ("opencode", "implementer", True, None, "opencode",
         loop.OPENCODE_ESCALATED_MODEL, None),
        ("codex", "implementer", True, None, "claude", loop.ESCALATED_MODEL, None),
        ("opencode", "implementer", False, "opencode", "claude",
         loop.MODELS["implementer"], None),
        ("codex", "implementer", False, "codex", "claude",
         loop.MODELS["implementer"], None),
        # "duel" is a scheduling directive: a stage asking for the issue's
        # agent without unwrapping the rotation routes to the role default.
        # The want follows the configured critic, whatever the ambient run
        # sets it to.
        ("duel", "critic", False, None, loop.ROLE_AGENT["critic"],
         (loop.OPENCODE_MODELS["critic"] if loop.ROLE_AGENT["critic"] == "opencode"
          else loop.MODELS["critic"] if loop.ROLE_AGENT["critic"] == "claude"
          else None), None),
        # The duel rotation's model override threads through to opencode only.
        ("opencode", "implementer", False, None, "opencode", "opencode/hy3-free",
         "opencode/hy3-free"),
    ]
    saved_ok = {a: getattr(loop, f"{a}_ok") for a in ("codex", "opencode")}
    try:
        for requested, role, escalate, latch, want_agent, want_model, override in cases:
            if latch:
                getattr(loop, f"disable_{latch}")("routing check")
            else:
                loop._disabled_agents = set()
            agent, model = loop.resolve(requested, role, escalate, override)
            if agent != want_agent or model != want_model:
                bad += fail(f"resolve({requested!r}, {role!r}, escalate={escalate}"
                            f", model={override!r}"
                            f"{', latched ' + latch if latch else ''}) -> "
                            f"({agent!r}, {model!r}); want ({want_agent!r}, "
                            f"{want_model!r})")
        loop._disabled_agents = set()
        try:
            loop.resolve("bogus", "implementer")
            bad += fail("resolve() accepted an unknown agent instead of "
                        "failing loudly")
        except ValueError:
            pass
    finally:
        loop._disabled_agents = set()
        for a, ok in saved_ok.items():
            setattr(loop, f"{a}_ok", ok)
    return bad


def _clean_env() -> dict:
    """The ambient environment with every FR_* knob stripped, so config.py's
    committed defaults are tested, not the run's overrides."""
    env = {k: v for k, v in os.environ.items() if not k.startswith("FR_")}
    env["FR_ENV_FILE"] = os.devnull  # and ignore orchestrator/.env, if any
    return env


def check_config_defaults() -> int:
    """config.py's committed defaults must be the all-opencode testing roster.

    Everything runs on opencode's free models while the run tests end to end
    (claude is quota-starved); claude/codex stay wired -- FR_AGENT_<ROLE>,
    FR_REVIEWER1/2 and FR_MODEL_<ROLE> restore the implementation-cheap /
    judgement-strong split the day the quota resets. The check runs in a
    subprocess with the environment stripped: an ambient .env or exported
    FR_* knob must not be able to fake a pass.
    """
    bad = 0
    probe = (
        "import config\n"
        "t = config.ROLE_AGENT\n"
        "assert all(a == 'opencode' for a in t.values()), t\n"
        "assert config.REVIEWER_AGENTS == {1: 'opencode', 2: 'opencode'}, "
        "config.REVIEWER_AGENTS\n"
        "assert config.DUEL_AGENTS == ['opencode'], config.DUEL_AGENTS\n"
        "assert config.DUEL_MODELS == ['opencode/deepseek-v4-flash-free', "
        "'opencode/hy3-free'], config.DUEL_MODELS\n"
        "assert config.ESCALATED_MODEL == 'claude-opus-5', "
        "config.ESCALATED_MODEL\n"
        "assert config.OPENCODE_ESCALATED_MODEL == "
        "config.OPENCODE_MODELS['implementer'], "
        "config.OPENCODE_ESCALATED_MODEL\n"
    )
    p = subprocess.run([sys.executable, "-c", probe], cwd=ROOT / "orchestrator",
                       capture_output=True, text=True, env=_clean_env())
    if p.returncode != 0:
        bad += fail("config.py defaults drifted from the all-opencode testing "
                    f"roster:\n{p.stderr.strip()}")
    return bad


def check_config_overrides() -> int:
    """Precedence must be env > .env file > config.py defaults, Laravel-style.

    The point of config.py is that the weekly split change is a one-line edit
    (or a .env value) without touching code: FR_AGENT_PLAN=claude in .env must
    move planning onto claude, a real environment variable must beat the
    file, and an unknown agent in either layer must fail at boot, not when
    the first issue reaches the role.
    """
    bad = 0
    with tempfile.TemporaryDirectory() as tmp:
        envfile = Path(tmp) / "env"
        envfile.write_text(
            "FR_AGENT_PLAN=codex\n"
            "# a comment line, and a junk line with no '='\n"
            "FR_MODEL_PLAN=claude-sonnet-5\n"
            "not-a-knob\n"
        )
        probe = (
            "import config\n"
            "assert config.ROLE_AGENT['planner'] == 'codex', "
            "config.ROLE_AGENT['planner']\n"
            "assert config.MODELS['planner'] == 'claude-sonnet-5', "
            "config.MODELS['planner']\n"
            "assert config.REVIEWER_AGENTS == {1: 'opencode', 2: 'opencode'}, "
            "config.REVIEWER_AGENTS\n"
        )
        env = _clean_env()
        env["FR_ENV_FILE"] = str(envfile)
        p = subprocess.run([sys.executable, "-c", probe], cwd=ROOT / "orchestrator",
                           capture_output=True, text=True, env=env)
        if p.returncode != 0:
            bad += fail(f".env values must override config.py defaults:\n"
                        f"{p.stderr.strip()}")

        # A real environment variable beats the file.
        probe = ("import config\n"
                 "assert config.ROLE_AGENT['planner'] == 'claude', "
                 "config.ROLE_AGENT['planner']\n")
        env["FR_AGENT_PLAN"] = "claude"
        p = subprocess.run([sys.executable, "-c", probe], cwd=ROOT / "orchestrator",
                           capture_output=True, text=True, env=env)
        if p.returncode != 0:
            bad += fail("a real environment variable must beat the .env file:\n"
                        f"{p.stderr.strip()}")

        # An unknown agent in the tables must fail at import.
        env2 = dict(env)
        env2["FR_AGENT_PLAN"] = "gpt"
        p = subprocess.run([sys.executable, "-c", "import config"],
                           cwd=ROOT / "orchestrator", capture_output=True,
                           text=True, env=env2)
        if p.returncode == 0:
            bad += fail("config.py accepted an unknown agent in the tables "
                        "instead of failing at boot")
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
                            escalate=False, model=None):
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
                            escalate=False, model=None):
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
                            escalate=False, model=None):
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


def check_priority_leads_the_queue() -> int:
    """A priority label must outrank dependants, not merely break ties.

    The queue order was a closure inside claimable(), so the only way to observe
    it was to call GitHub, and no check ever did. That is why five separate
    retrospectives (retro-1, -2, -6, -22, -23) each rediscovered the same
    starvation by hand and none of them could pin it: #25 sat ready and
    unclaimed for 33.5h with nothing able to assert it should not have.

    The load-bearing case is a p0 that unblocks nothing against a p2 that
    unblocks ten. If priority were the last term rather than the first, that
    comes out backwards -- so this single assertion is what proves the label
    actually schedules work rather than annotating it.

    The other half matters as much: untriaged must sort exactly as it did before
    priorities existed. 92 open issues carry no priority label, and a default
    that sank them would have handed the whole port to whatever got labelled
    first.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    try:
        import gh
    except Exception as exc:                     # noqa: BLE001 - reported, not raised
        return fail(f"cannot import orchestrator/gh.py to check queue order: {exc!r}")

    if not hasattr(gh, "rank_key"):
        return fail("gh.py has no module-level rank_key() — queue order is "
                    "unobservable again, and only GitHub can answer what runs next")

    def issue(n: int, labels: list[str]):
        return gh.Issue(number=n, title="", body="", labels=labels)

    def order(issues, dependants):
        return [i.number for i in sorted(issues, key=lambda i: gh.rank_key(i, dependants))]

    bad = 0

    got = order([issue(2, ["fr:ready"]), issue(1, ["fr:ready", "fr:p0"])], {2: 10})
    if got != [1, 2]:
        bad += fail(f"a p0 unblocking nothing lost to a p2 unblocking ten: {got} — "
                    "priority is decorating the queue, not ordering it")

    got = order([issue(1, ["fr:p3"]), issue(2, ["fr:p0"])], {1: 99})
    if got != [2, 1]:
        bad += fail(f"a p3 with 99 dependants outranked a p0 leaf: {got}")

    if gh.priority(issue(9, ["fr:ready"])) != gh.priority(issue(9, ["fr:ready", "fr:p2"])):
        bad += fail("an untriaged issue does not sort as fr:p2 — the 92 issues "
                    "that predate priorities have silently changed position")

    got = order([issue(5, ["fr:ready"]), issue(3, ["fr:ready", "fr:meta"]),
                 issue(4, ["fr:ready"])], {5: 1})
    if got != [5, 4, 3]:
        bad += fail(f"untriaged issues no longer order by dependants, then "
                    f"housekeeping, then number: {got}")

    if gh.priority(issue(1, ["fr:p3", "fr:p0"])) != 0:
        bad += fail("contradictory priority labels do not resolve to the "
                    "strongest — a half-finished downgrade reads as a downgrade")

    if gh.priority(issue(1, ["fr:p9"])) != gh.P_DEFAULT:
        bad += fail("a label that merely looks like a priority is being parsed "
                    "as one")

    declared = set(gh.PRIORITY_LABELS) - set(gh.LABELS)
    if declared:
        bad += fail(f"{sorted(declared)} rank the queue but are absent from "
                    "LABELS, so ensure_labels() never creates them and every "
                    "attempt to set one fails against a label that does not exist")
    return bad


def check_reviewer_diff_not_silently_truncated() -> int:
    """An over-cap diff must never reach a reviewer as a silent, unmarked cut.

    `prompt_for("reviewer", issue, f"...{diff[:120000]}...")` closed the fence
    right after the bare slice, so an over-cap diff arrived as a syntactically
    well-formed markdown code block that happened to stop mid-token -- no
    marker, no byte count, no list of what fell off. And because
    `worktree_diff` stages with `git add -A` and git emits changed paths
    alphabetically, the tail that fell off was deterministic: `frankenrust-sys`
    -- shim.c, the bindgen headers, build.rs -- exactly what reviewer.md ranks
    as priority one to check. #11 merged with six of nine changed files never
    shown to either reviewer of the round that approved it. Retro 21, finding 1.

    Pins loop.diff_prompt_section against the three things #135 asks for:
    every path named in a manifest regardless of the cut, an unmissable prose
    notice carrying both byte counts, and the absolute path of the complete
    patch on disk so a reviewer with tool access can go read what the inline
    copy dropped. The synthetic diff below reproduces the actual failure shape
    -- alphabetically-first Rust files large enough alone to blow the cap,
    pushing the C shim and headers entirely past it.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    def hunk(path: str, body_lines: int) -> str:
        header = (f"diff --git a/{path} b/{path}\n"
                  f"index 0000000..1111111 100644\n"
                  f"--- a/{path}\n+++ b/{path}\n"
                  f"@@ -1,1 +1,{body_lines} @@\n")
        body = "".join(f"+line {n} of {path}\n" for n in range(body_lines))
        return header + body

    # Alphabetical, as git emits them -- frankenrust-core sorts before -sys,
    # and core's two biggest files alone exceed the cap, so the C shim and
    # headers land entirely past the cut just like they did on #11.
    files = [
        ("crates/frankenrust-core/src/cgi.rs", 2500),
        ("crates/frankenrust-core/src/context.rs", 2500),
        ("crates/frankenrust-core/src/lib.rs", 400),
        ("crates/frankenrust-sys/build.rs", 200),
        ("crates/frankenrust-sys/include/frankenrust_shim.h", 200),
        ("crates/frankenrust-sys/shim.c", 200),
        ("crates/frankenrust-sys/wrapper.h", 100),
    ]
    diff = "".join(hunk(path, n) for path, n in files)
    total = len(diff.encode())
    if total <= loop.DIFF_INLINE_CAP:
        return fail(f"test setup bug: synthetic diff ({total} bytes) does not "
                    f"exceed the cap ({loop.DIFF_INLINE_CAP} bytes)")

    bad = 0
    with tempfile.TemporaryDirectory() as tmp:
        patch_path = Path(tmp) / "diff.post3.patch"
        patch_path.write_text(diff)
        prompt = loop.diff_prompt_section(diff, patch_path)

        for path, _ in files:
            if path not in prompt:
                bad += fail(f"{path} never appears in the reviewer prompt -- it "
                            "changed and no reviewer is ever told its name")

        if f"{total:,}" not in prompt:
            bad += fail("prompt does not state the full diff's byte count "
                        f"({total:,})")
        if f"{loop.DIFF_INLINE_CAP:,}" not in prompt:
            bad += fail("prompt does not state how many bytes are actually "
                        f"shown inline ({loop.DIFF_INLINE_CAP:,})")
        if "truncat" not in prompt.lower() and "incomplete" not in prompt.lower():
            bad += fail("prompt carries no explicit prose notice that the "
                        "inline copy is not the whole diff")
        if str(patch_path.resolve()) not in prompt:
            bad += fail("prompt does not give the reviewer the absolute path "
                        "of the complete, untruncated patch on disk")

    # The manifest's counts are advertised to the reviewer as fact, so a line
    # whose own content starts with `---`/`+++` must not be mistaken for the
    # file header those prefixes also mark. Deleting a markdown rule or YAML
    # separator renders as `----`, which a prefix test reads as "not a change"
    # and drops -- a file could show (+0/-0) while being rewritten.
    meta = ("diff --git a/docs/x.md b/docs/x.md\n"
            "--- a/docs/x.md\n+++ b/docs/x.md\n@@ -1,3 +1,3 @@\n"
            " title\n----\n+++ replacement\n body\n")
    if loop.diff_file_stats(meta) != [("docs/x.md", 1, 1)]:
        bad += fail("diff_file_stats miscounts a hunk whose own lines begin "
                    f"with ---/+++: {loop.diff_file_stats(meta)} != "
                    "[('docs/x.md', 1, 1)]")

    # Pin the regression by name, not just by behaviour: the bare slice must
    # not come back wearing a different variable name.
    src = (ROOT / "orchestrator" / "loop.py").read_text()
    if re.search(r"\bdiff\[:\s*120[_,]?000\s*\]", src):
        bad += fail("loop.py still slices the diff with a bare diff[:120000] "
                    "-- the exact silent truncation this check exists to catch")
    return bad


def check_reviewer_patch_file_is_complete_and_applies() -> int:
    """The path the prompt sends a reviewer to must be a whole, usable patch.

    The notice is only worth what the file behind it is worth. Drives
    review_stage for real over an over-cap diff -- invoke() stubbed to leave the
    artifacts the real one leaves -- and pins three properties of the disclosed
    path that a rendered-string test cannot see:

    - it exists, and `git apply` accepts it. `git()` strips its output, so the
      diff arrives without the trailing newline a unified diff must end with,
      and a patch written raw makes `git apply` report "corrupt patch at line
      N". A reviewer who tries to apply what we handed them concludes the patch
      is damaged rather than that the tooling is. (#161 is the strip itself.)
    - it holds what the inline copy dropped -- `shim.c` here, the file that
      falls off every over-cap diff in this repo.
    - the manifest the reviewer is shown agrees with `git apply --numstat` on
      the same patch, path for path and count for count. The manifest is parsed
      out of the diff text by diff_file_stats rather than measured by git, so
      this is the check that it is telling the truth: a wrong `(+0/-0)` invites
      a reviewer to skip a file that really changed, which is the exact failure
      #135 exists to prevent.

    What this deliberately does *not* assert is that the patch sits in a
    directory of its own. It does not: logdir also holds every transcript for
    the issue, and pointing a reviewer there is a real hazard to the
    independence of the two reviews. The mitigation here is the notice's closing
    sentence, asserted below; the structural fix -- a second copy under a
    patches-only directory -- is specified by open issue #160 and belongs to it.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import gh
    import loop

    def sh(cwd: Path, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True)

    prompts: list[str] = []

    def fake_invoke(agent: str, wt: Path, prompt: str, logdir: Path, tag: str,
                    role: str = "implementer", escalate: bool = False,
                    model: str | None = None):
        # The real invoke's side effects on logdir, verbatim -- these are the
        # files that sit next to the patch the reviewer is pointed at.
        logdir.mkdir(parents=True, exist_ok=True)
        (logdir / f"prompt.{tag}.md").write_text(prompt)
        (logdir / f"{agent}.{tag}.log").write_text("... reviewer transcript ...\n")
        (logdir / f"{agent}.{tag}.final.txt").write_text("VERDICT: PASS\n")
        prompts.append(prompt)
        return agent, 0, "VERDICT: PASS"

    saved = (loop.invoke, loop.record, loop.log)
    loop.invoke = fake_invoke
    loop.record = lambda event, **f: None
    loop.log = lambda msg: None
    bad = 0
    try:
        with tempfile.TemporaryDirectory() as tmp:
            wt = Path(tmp) / "wt"
            (wt / "crates" / "frankenrust-sys").mkdir(parents=True)
            sh(wt, "init", "-q", "-b", "main")
            sh(wt, "config", "user.email", "check_orchestrator@example.com")
            sh(wt, "config", "user.name", "check_orchestrator")
            (wt / "seed.txt").write_text("seed\n")
            sh(wt, "add", "-A")
            sh(wt, "commit", "-q", "-m", "base")

            # Over the cap on its own, so the prompt takes the branch that
            # discloses the path -- and alphabetically first, so the C shim is
            # what falls off, which is the shape #135 is about.
            core = wt / "crates" / "frankenrust-core"
            core.mkdir(parents=True)
            (core / "cgi.rs").write_text(
                "".join(f"// line {n}\n" for n in range(12_000)))
            (wt / "crates" / "frankenrust-sys" / "shim.c").write_text(
                "int go_register_server_variables(void) { return 0; }\n")

            logdir = Path(tmp) / "logs" / "135"
            logdir.mkdir(parents=True)
            if len(loop.worktree_diff(wt).encode()) <= loop.DIFF_INLINE_CAP:
                return fail("test setup bug: the worktree diff is under the cap, "
                            "so the prompt never discloses a patch path at all")

            issue = gh.Issue(number=135, title="reviewer prompt", body="body")
            loop.review_stage(issue, wt, logdir, "post3")

            if len(prompts) != 2:
                return fail(f"review_stage invoked {len(prompts)} reviewers, expected 2")
            prompt = prompts[0]
            if prompts[1] != prompt:
                bad += fail("the two reviewers were handed different prompts")
            disclosed = re.findall(r"(/\S+\.patch)\b", prompt)
            if not disclosed:
                return fail("the reviewer prompt discloses no absolute path to the "
                            "complete patch")
            patch = Path(disclosed[0])
            if not patch.is_file():
                return fail(f"the path the prompt sends the reviewer to does not "
                            f"exist: {patch}")
            if "shim.c" not in patch.read_text():
                bad += fail("the patch on disk is not the complete diff -- the file "
                            "the inline copy dropped is missing from it too")

            # git's own reading of the file we wrote: proves it parses as a
            # patch at all, and yields the counts to hold the manifest to.
            applied = sh(wt, "apply", "--numstat", str(patch))
            if applied.returncode != 0:
                bad += fail("git refuses the patch the reviewer is told to read: "
                            f"{(applied.stdout + applied.stderr).strip()}")
            for row in applied.stdout.splitlines():
                added, removed, path = (row.split("\t") + ["", "", ""])[:3]
                if added == "-":            # binary; git reports no counts
                    continue
                entry = f"- {path} (+{added}/-{removed})"
                if entry not in prompt:
                    bad += fail(f"the manifest disagrees with git about {path}: "
                                f"expected {entry!r}, not in the prompt")

            # The patch is disclosed from inside the transcripts directory, so
            # the notice must fence the reviewer to that one file. If #160 later
            # moves it somewhere that holds only patches, this stops applying
            # and stops being asserted -- the hazard is the neighbours, not the
            # sentence.
            neighbours = [q.name for q in patch.parent.iterdir()
                          if q != patch and not q.name.endswith(".patch")]
            if neighbours and "nothing else" not in prompt:
                bad += fail(f"the disclosed patch sits beside {sorted(neighbours)} "
                            "and the prompt does not tell the reviewer to read that "
                            "one file only -- a reviewer that lists the directory "
                            "reads the other reviewer's verdict, and the "
                            "two-reviewer gate becomes one")
            return bad
    finally:
        loop.invoke, loop.record, loop.log = saved


def check_post_fix_empty_diff_does_not_merge() -> int:
    """A fixer that reverts its diff to nothing must not merge an empty commit.

    #166. review_stage() opens with `if not diff.strip(): return None, {}, {}`
    -- correct in isolation, there is nothing to review. The *initial* review
    at loop.py:1217 is guarded against this (an implementer that changed
    nothing spends the attempt), but the post-fix review right after it was
    not: a fixer answering a blocking review by reverting the implementer's
    work to nothing produces an empty diff, `review_stage(f"post{attempt}")`
    reports it as `(None, {}, {})`, `still_blocking` is falsy, and control
    fell straight through to `merge_worktree()` -- which commits with
    `--allow-empty` -- and then `gh.close()`, recording the issue as merged
    with zero reviewer verdicts on the merged content.

    Drives `_work()` itself, for real, against a throwaway git repo: `invoke`
    is stubbed by role (critic PROCEEDs; the implementer writes a file; both
    reviewers BLOCK; the fixer deletes the file it just wrote, exactly the
    "revert rather than fix" response the issue is about); `run` (the gate)
    always reports success; `merge_worktree` is stubbed to a counter so a call
    to it is unambiguous regardless of what it would have done to a fake
    `main`. `MAX_ATTEMPTS` is pinned to 1: the fixer's revert is
    deterministic and reproduces on every attempt, so one is enough to prove
    the guard fires and cheaper than watching it fire three times.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import gh
    import loop

    def sh(wt: Path, *args: str) -> None:
        subprocess.run(["git", *args], cwd=wt, check=True, capture_output=True, text=True)

    def make_repo(wt: Path) -> None:
        wt.mkdir(parents=True, exist_ok=True)
        sh(wt, "init", "-q", "-b", "main")
        sh(wt, "config", "user.email", "check_orchestrator@example.com")
        sh(wt, "config", "user.name", "check_orchestrator")
        (wt / "a.txt").write_text("base\n")
        sh(wt, "add", "-A")
        sh(wt, "commit", "-q", "-m", "base")
        sh(wt, "checkout", "-q", "-b", "issue/166")

    issue = gh.Issue(number=166, title="a fixer that reverts to nothing",
                     body="Gate: bootstrap\nAgent: claude\n")

    class FakeGh:
        def __init__(self) -> None:
            self.closed: list[tuple[int, str]] = []
            self.blocked: list[tuple[int, str]] = []

        def comment(self, n: int, body: str) -> None:
            pass

        def close(self, n: int, summary: str) -> None:
            self.closed.append((n, summary))

        def block(self, n: int, why: str) -> None:
            self.blocked.append((n, why))

    fake_gh = FakeGh()
    merges: list[str] = []
    events: list[tuple[str, dict]] = []

    saved = {name: getattr(loop, name) for name in
             ("invoke", "run", "record", "log", "gh", "merge_worktree", "MAX_ATTEMPTS")}

    def fake_invoke(agent, wt, prompt, logdir, tag, role="implementer",
                    escalate=False, model=None):
        use = loop.resolve(agent, role, escalate)[0]
        if role == "critic":
            return use, 0, "Nothing wrong with this issue.\n\nVERDICT: PROCEED"
        if role == "implementer":
            (wt / "feature.txt").write_text("the implementer's work\n")
            return use, 0, "implemented"
        if role == "fixer":
            # The plausible response to a blocking review it cannot satisfy:
            # undo the work rather than fix it.
            (wt / "feature.txt").unlink(missing_ok=True)
            return use, 0, "reverted"
        if role == "reviewer":
            return use, 0, "This changes nothing safely.\n\nVERDICT: BLOCK"
        raise AssertionError(f"unexpected role {role!r} (tag {tag!r})")

    loop.invoke = fake_invoke
    loop.run = lambda cmd, wt, timeout, log_path=None: (0, "")
    loop.record = lambda event, **f: events.append((event, f))
    loop.log = lambda msg: None
    loop.gh = fake_gh
    loop.merge_worktree = lambda tid, logdir, gate: (merges.append(tid) or True)
    loop.MAX_ATTEMPTS = 1

    bad = 0
    try:
        with tempfile.TemporaryDirectory() as tmp:
            wt = Path(tmp) / "repo"
            make_repo(wt)
            logdir = Path(tmp) / "logs"
            logdir.mkdir()
            loop._work(issue, "166", wt, logdir)

        if merges:
            bad += fail(f"_work() reached merge_worktree() on a post-fix round "
                        f"whose diff the fixer had reverted to nothing: {merges}")
        if fake_gh.closed:
            bad += fail(f"_work() closed #166 as merged after a post-fix round "
                        f"with an empty diff and zero reviewer verdicts on the "
                        f"merged content: {fake_gh.closed}")
        if not fake_gh.blocked:
            bad += fail("_work() neither merged nor blocked #166 -- the attempt "
                        "must be spent, not silently dropped")
        empty_post_fix = [f for e, f in events
                          if e == "empty_diff" and f.get("phase") == "post-fix"]
        if not empty_post_fix:
            bad += fail(f"no empty_diff event recorded with phase='post-fix' -- "
                        f"the post-fix path has no guard against a fixer that "
                        f"reverts its diff to nothing: {events}")
    finally:
        for name, val in saved.items():
            setattr(loop, name, val)
    return bad


def check_root_dirty_set_parses_porcelain_z() -> int:
    """`loop.root_dirty_set()` must survive `git status --porcelain -z`'s actual
    record format, not the format a naive port of the non-`-z` parser expects.

    #168: an agent standing in a worktree can write into the main checkout,
    and prompt_for() reads its prompts from there on every subsequent
    invocation -- so a stray write to a tracked prompt file is fed to every
    later stage, ungated. root_dirty_set() is the parser the detector is
    built on, and it is shared with #185 (the merge path) and #186 (untracked
    strays), so a parsing bug here is not local to this issue.

    The `-z` record format is NOT `--porcelain` with newlines swapped for
    NULs: with `-z` git emits no quoting at all, and a rename/copy is TWO
    NUL-terminated fields, new path first, with no ` -> `. A `split('\\0')`
    loop that assumes one path per record reads the old path back as a bogus,
    status-less record and -- worse -- desyncs every record after it.

    Exercised against real git, not a canned string: rename detection
    behaviour (which column carries `R`, whether the old path even appears)
    is git's, not this test's, to define, and the two rename shapes below
    were independently verified against the installed git before being
    written into this test:

    - a STAGED rename (`git mv`), which reports `R ` -- R in column one, and
    - an UNSTAGED rename ("renamed in work tree"), which git only reports
      when the new path has been `git add -N`'d (intent-to-add) -- without
      that, git treats the old path as merely deleted and the new path as
      merely untracked, no rename at all. Reported as ` R` -- R in column
      TWO, which is the shape a parser keyed on column one alone gets wrong:
      keying only on column one desyncs the rest of the stream and hands a
      garbage path (missing its leading directory component, or reading
      "/def.md" as `ROOT / "/def.md"` -- outside the repository entirely) to
      whatever consumes this dict next.

    Also covers a path containing a space, which -z's raw (unquoted) output
    is the reason to use it at all.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    def sh(cwd: Path, *args: str) -> str:
        return subprocess.run(["git", *args], cwd=cwd, check=True,
                              capture_output=True, text=True).stdout

    bad = 0
    with tempfile.TemporaryDirectory() as tmp:
        repo = Path(tmp)
        sh(repo, "init", "-q", "-b", "main")
        sh(repo, "config", "user.email", "check_orchestrator@example.com")
        sh(repo, "config", "user.name", "check_orchestrator")

        # Large and varied enough that git's similarity heuristic treats the
        # move as a rename rather than an unrelated add+delete.
        content = "".join(f"line {n}\n" for n in range(30))
        (repo / "abc").mkdir()
        (repo / "abc" / "def.md").write_text(content)
        (repo / "c.md").write_text("c file\nsecond line\nthird line\n")
        sh(repo, "add", "-A")
        sh(repo, "commit", "-q", "-m", "base")

        # unstaged rename ("renamed in work tree"): move on disk, then
        # intent-to-add the new path so git's index-vs-worktree diff has
        # something to compare the deleted old path against.
        (repo / "abc" / "def.md").rename(repo / "abc" / "xyz.md")
        sh(repo, "add", "-N", "abc/xyz.md")

        # staged rename, onto a path with a space in it.
        sh(repo, "mv", "c.md", "d file.md")

        # untracked, with a space.
        (repo / "new file.md").write_text("scratch\n")

        dirty = loop.root_dirty_set(repo)

        if dirty.get("abc/xyz.md") != " R":
            bad += fail(f"unstaged rename (R in column two) parsed as "
                        f"{dirty.get('abc/xyz.md')!r}, not ' R': {dirty}")
        if "abc/def.md" in dirty:
            bad += fail(f"the OLD path of the unstaged rename leaked into "
                        f"the dict as its own status-less entry: {dirty}")
        if dirty.get("d file.md") != "R ":
            bad += fail(f"staged rename (R in column one) parsed as "
                        f"{dirty.get('d file.md')!r}, not 'R ': {dirty}")
        if "c.md" in dirty:
            bad += fail(f"the OLD path of the staged rename leaked into "
                        f"the dict as its own status-less entry: {dirty}")
        if dirty.get("new file.md") != "??":
            bad += fail(f"untracked path containing a space parsed as "
                        f"{dirty.get('new file.md')!r}, not '??': {dirty}")
        garbage = [k for k in dirty if k.startswith("/") or not k]
        if garbage:
            bad += fail(f"a desynced record produced a garbage path key -- "
                        f"e.g. '/def.md' resolves outside the repo entirely "
                        f"via ROOT / '/def.md': {garbage} in {dirty}")

        # Consuming the rename record's second field is what keeps the stream
        # in sync; RETURNING it is what lets a stray rename in ROOT be undone.
        # The source path is the tracked file that just left the working tree,
        # and git reports it nowhere else -- drop it and a stray `git mv` of a
        # prompt file reads as a plain untracked addition, so nothing restores
        # it. Kept on its own channel so root_dirty_set() still yields exactly
        # one entry per status record, which is the contract #185 and #186
        # consume.
        _, renamed_from = loop._root_status(repo)
        for src, kind in (("abc/def.md", "unstaged rename (column two)"),
                          ("c.md", "staged rename (column one)")):
            if src not in renamed_from:
                bad += fail(f"the source path of the {kind} was consumed to stay "
                            f"in sync but never returned, so nothing can restore "
                            f"it: {renamed_from}")
        if set(renamed_from) - {"abc/def.md", "c.md"}:
            bad += fail(f"a non-rename record contributed a bogus source path: "
                        f"{renamed_from}")
    return bad


def _guard_init(cwd: Path) -> None:
    subprocess.run(["git", "init", "-q", "-b", "main"], cwd=cwd, check=True)
    subprocess.run(["git", "config", "user.email", "check_orchestrator@example.com"],
                   cwd=cwd, check=True)
    subprocess.run(["git", "config", "user.name", "check_orchestrator"], cwd=cwd, check=True)


def _guard_sh(cwd: Path, *args: str) -> str:
    return subprocess.run(["git", *args], cwd=cwd, check=True,
                          capture_output=True, text=True).stdout


def check_guard_root_writes_reverts_tracked_stray() -> int:
    """A stray write to a TRACKED path in ROOT during a worktree agent stage
    must be detected, journalled with the path, and reverted to HEAD.

    #168, occurrence 1 and 2: agent_cmd() runs claude with
    --dangerously-skip-permissions and no cwd confinement, so an agent
    standing in a worktree can still write into the main checkout, and
    prompt_for() reads ROOT's tracked prompt files on every later invocation.
    guard_root_writes() is the bracket that catches this.

    Covers both ways the stray content can end up on disk: a plain write, and
    a write followed by the agent running `git add` on it in ROOT. The second
    case is why the revert must be `git checkout HEAD -- <path>` and not the
    two-arg `git checkout -- <path>`: the two-arg form restores from the
    index, which is exactly where a `git add`-ed stray lives, so it would
    restore the stray instead of discarding it.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    real_record, real_log = loop.record, loop.log
    events: list[tuple[str, dict]] = []
    loop.record = lambda event, **f: events.append((event, f))
    loop.log = lambda msg: None
    bad = 0
    try:
        # Plain write, never staged.
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            _guard_init(repo)
            (repo / "reviewer.md").write_text("original\n")
            _guard_sh(repo, "add", "-A")
            _guard_sh(repo, "commit", "-q", "-m", "base")

            with loop.guard_root_writes("135", "fix.1", root=repo):
                (repo / "reviewer.md").write_text("original\nsix stray lines\n")

            content = (repo / "reviewer.md").read_text()
            if content != "original\n":
                bad += fail(f"a stray write to a tracked ROOT path was not "
                            f"reverted to HEAD's content: {content!r}")
            root_writes = [f for e, f in events if e == "root_write"]
            if not root_writes:
                bad += fail(f"a stray write to a tracked ROOT path was not "
                            f"journalled at all: {events}")
            elif root_writes[0].get("paths") != ["reviewer.md"]:
                bad += fail(f"root_write did not name the stray path: {root_writes[0]}")
            elif root_writes[0].get("reverted") != ["reviewer.md"]:
                bad += fail(f"root_write did not record the path as reverted: "
                            f"{root_writes[0]}")
            if any(e == "root_write_revert_failed" for e, _ in events):
                bad += fail(f"a revert that worked was journalled as having failed: {events}")

        # Stray write, then the agent runs `git add` on it in ROOT.
        events.clear()
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            _guard_init(repo)
            (repo / "reviewer.md").write_text("original\n")
            _guard_sh(repo, "add", "-A")
            _guard_sh(repo, "commit", "-q", "-m", "base")

            with loop.guard_root_writes("135", "fix.1", root=repo):
                (repo / "reviewer.md").write_text("original\nsix stray lines\n")
                _guard_sh(repo, "add", "-A")   # the stray is now in the index too

            content = (repo / "reviewer.md").read_text()
            if content != "original\n":
                bad += fail(f"a `git add`-ed stray was not reverted -- the "
                            f"two-arg `checkout --` form (which restores from "
                            f"the index) was used instead of `checkout HEAD --`: "
                            f"{content!r}")
    finally:
        loop.record, loop.log = real_record, real_log
    return bad


def check_guard_root_writes_catches_stray_on_already_dirty_path() -> int:
    """A stray write to a path that was ALREADY dirty before the stage began
    must still be detected -- this is occurrence 2's own shape, and it is the
    one case a status-code-keyed delta cannot see.

    #185's preflight deliberately leaves ROOT dirty for the whole run, so a
    tracked path that is ` M` before a guarded stage and still ` M` after
    compares equal under a delta keyed on root_dirty_set()'s XY code alone --
    which would let unbounded stray content land on exactly the paths that
    were already dirty at cmd_run, disarmed for the entire run. This is not
    hypothetical: reviewer.md carrying #135's own uncommitted lines, present
    on no branch, is exactly this shape. guard_root_writes() must key on a
    content fingerprint instead.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    real_record, real_log = loop.record, loop.log
    events: list[tuple[str, dict]] = []
    loop.record = lambda event, **f: events.append((event, f))
    loop.log = lambda msg: None
    bad = 0
    try:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            _guard_init(repo)
            (repo / "reviewer.md").write_text("v1\n")
            _guard_sh(repo, "add", "-A")
            _guard_sh(repo, "commit", "-q", "-m", "base")

            # Dirty BEFORE the stage starts -- pre-existing, uncommitted
            # residue, same status code ("M") the stray write below will
            # leave it at.
            (repo / "reviewer.md").write_text("v1\nlocal-dirt\n")

            with loop.guard_root_writes("135", "fix.2", root=repo):
                (repo / "reviewer.md").write_text("v1\nlocal-dirt\nsix stray lines\n")

            content = (repo / "reviewer.md").read_text()
            if content != "v1\n":
                bad += fail(f"a stray write appended to an already-dirty tracked "
                            f"path was not detected/reverted: {content!r}")
            if not any(e == "root_write" for e, _ in events):
                bad += fail("a status-code-only delta hid a stray write to a path "
                            "that was dirty before the stage began -- this is "
                            "occurrence 2's own shape and it is the case this "
                            "check exists to pin")
    finally:
        loop.record, loop.log = real_record, real_log
    return bad


def check_guard_root_writes_leaves_untracked_stray_on_disk() -> int:
    """An UNTRACKED stray in ROOT must be journalled with its path and left
    alone -- no unlink, no shutil.rmtree, no git reset.

    #168 is scoped to tracked paths only. Six reviewer rounds on the
    predecessor attempts found eleven of twelve blocking findings against
    code that deleted untracked content no agent wrote -- ignore-rule changes
    landing from a concurrent merge, untracked-directory collapsing, a
    pre-existing untracked file merely modified, a parser desync producing an
    unlink target outside the repo. Disposition of untracked strays belongs
    to #186. This pins the boundary: detect and journal, never delete.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    real_record, real_log = loop.record, loop.log
    events: list[tuple[str, dict]] = []
    loop.record = lambda event, **f: events.append((event, f))
    loop.log = lambda msg: None
    bad = 0
    try:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            _guard_init(repo)
            (repo / "a.md").write_text("base\n")
            _guard_sh(repo, "add", "-A")
            _guard_sh(repo, "commit", "-q", "-m", "base")

            with loop.guard_root_writes("135", "fix.1", root=repo):
                (repo / "scratch.md").write_text("an agent's untracked stray\n")

            if not (repo / "scratch.md").exists():
                bad += fail("an untracked stray was removed from disk -- that "
                            "disposition belongs to #186, not this guard")
            elif (repo / "scratch.md").read_text() != "an agent's untracked stray\n":
                bad += fail("an untracked stray's content was altered")
            root_writes = [f for e, f in events if e == "root_write"]
            if not root_writes:
                bad += fail(f"an untracked stray was not journalled at all: {events}")
            elif "scratch.md" not in root_writes[0].get("not_reverted", []):
                bad += fail(f"an untracked stray was journalled but not under "
                            f"not_reverted: {root_writes[0]}")
            if any(e == "root_write_revert_failed" for e, _ in events):
                bad += fail(f"an untracked stray -- which is never reverted -- "
                            f"was journalled as a FAILED revert: {events}")
    finally:
        loop.record, loop.log = real_record, real_log
    return bad


def check_guard_root_writes_ignores_concurrent_merge() -> int:
    """A merge landing in ROOT between the pre- and post-stage capture must
    not be misread as a stray write, and the merged content must survive.

    This is the case that separates a dirty-set-delta implementation from a
    tree-snapshot one, and it is not optional: `_merge_lock` (loop.py) is
    acquired inside merge_worktree and nowhere else, so it serialises merges
    against each other but NOT against agent stages running in the other
    MAX_PARALLEL worker threads. With AGENT_TIMEOUT=3600, a merge landing in
    ROOT mid-stage is the common case, not an edge case.

    A `git write-tree`/`read-tree --reset -u` snapshot approach fails this:
    write-tree reflects the current index, and a fast-forward merge advances
    HEAD (and the index) to a new tree even though the working directory
    ends up clean -- so a before/after tree-hash comparison sees a change
    that isn't a stray write, the post-stage diff contains every file the
    other worker's merge landed, and `read-tree --reset -u` back to the
    pre-merge tree would rewrite ROOT's working tree to pre-merge content
    while HEAD stays at the merged commit, corrupting the checkout for every
    other worker. A dirty-set delta has none of this: a fast-forward merge
    leaves `git status` clean before and after, so it contributes nothing to
    the delta.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    real_record, real_log = loop.record, loop.log
    events: list[tuple[str, dict]] = []
    loop.record = lambda event, **f: events.append((event, f))
    loop.log = lambda msg: None
    bad = 0
    try:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            _guard_init(repo)
            (repo / "a.md").write_text("base\n")
            _guard_sh(repo, "add", "-A")
            _guard_sh(repo, "commit", "-q", "-m", "base")
            _guard_sh(repo, "checkout", "-q", "-b", "feature")
            (repo / "other.md").write_text("v2, from another worker's merge\n")
            _guard_sh(repo, "add", "-A")
            _guard_sh(repo, "commit", "-q", "-m", "concurrent work")
            _guard_sh(repo, "checkout", "-q", "main")

            with loop.guard_root_writes("135", "fix.1", root=repo):
                # Simulates another worker's merge_worktree landing in ROOT
                # while this stage's agent is still running.
                _guard_sh(repo, "merge", "--ff-only", "feature")

            if any(e == "root_write" for e, _ in events):
                bad += fail(f"a concurrent fast-forward merge in ROOT was "
                            f"misdetected as a stray write: {events}")
            merged = (repo / "other.md")
            if not merged.exists() or merged.read_text() != "v2, from another worker's merge\n":
                bad += fail("the concurrent merge's content did not survive in "
                            "ROOT's working tree after the guard ran")
    finally:
        loop.record, loop.log = real_record, real_log
    return bad


def check_guard_root_writes_restores_a_renamed_away_tracked_file() -> int:
    """A stray write that git reports as a RENAME must still restore the
    tracked file it moved away from.

    A rename record in `git status --porcelain -z` carries two paths, and the
    second one -- the path the file used to live at -- appears nowhere else in
    the output. A parser that consumes that field to stay in sync but throws
    it away reports only the NEW path, which is untracked, so the guard files
    the whole event under `not_reverted`, reverts nothing, and records no
    root_write_revert_failed either (there was nothing in `reverted` to
    verify). The event reads in the journal as "an untracked stray, correctly
    left alone per #186" while ROOT/orchestrator/prompts/reviewer.md is simply
    gone -- after which prompt_for() raises FileNotFoundError for every
    subsequent issue in the run, surfacing as work_crash attributed to
    whichever issue was unlucky enough to be next.

    All three shapes an agent produces this in are covered, because git
    reports them in different columns and only one of them is a shape any
    other check constructs:

      * `git mv <old> <new>`                  -> `R ` (column one)
      * `mv` on disk then `git add -A`        -> `R ` (column one)
      * `mv` on disk then `git add -N <new>`  -> ` R` (column two)

    A plain `mv` with no git command at all is NOT rename-detected -- git
    reports ` D <old>` and `?? <new>`, two ordinary records -- so it is the
    one shape that passes even with the source path discarded. It is included
    anyway to pin that the fix did not regress it.

    The new path must survive on disk in every case: it is untracked, and
    untracked disposition is #186's, not this guard's.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    real_record, real_log = loop.record, loop.log
    events: list[tuple[str, dict]] = []
    loop.record = lambda event, **f: events.append((event, f))
    loop.log = lambda msg: None

    old, new = "orchestrator/prompts/reviewer.md", "orchestrator/prompts/stolen.md"
    # Long enough that git's similarity heuristic calls the move a rename
    # rather than an unrelated add plus delete.
    head = "".join(f"reviewer instruction {n}\n" for n in range(40))

    def staged_mv(repo: Path) -> None:
        _guard_sh(repo, "mv", old, new)

    def disk_mv_then_add_all(repo: Path) -> None:
        (repo / old).rename(repo / new)
        _guard_sh(repo, "add", "-A")

    def disk_mv_then_intent_to_add(repo: Path) -> None:
        (repo / old).rename(repo / new)
        _guard_sh(repo, "add", "-N", new)

    def disk_mv_only(repo: Path) -> None:
        (repo / old).rename(repo / new)

    bad = 0
    try:
        for label, mutate in (("git mv", staged_mv),
                              ("mv + git add -A", disk_mv_then_add_all),
                              ("mv + git add -N", disk_mv_then_intent_to_add),
                              ("mv, no git command", disk_mv_only)):
            events.clear()
            with tempfile.TemporaryDirectory() as tmp:
                repo = Path(tmp)
                _guard_init(repo)
                (repo / "orchestrator" / "prompts").mkdir(parents=True)
                (repo / old).write_text(head)
                _guard_sh(repo, "add", "-A")
                _guard_sh(repo, "commit", "-q", "-m", "base")

                with loop.guard_root_writes("135", "fix.2", root=repo):
                    mutate(repo)

                if not (repo / old).exists():
                    bad += fail(f"[{label}] a rename-detected stray write left the "
                                f"tracked {old} deleted in ROOT -- the rename "
                                f"record's source path was parsed but discarded, "
                                f"so nothing ever restored it, and prompt_for() "
                                f"now raises for the rest of the run: {events}")
                elif (repo / old).read_text() != head:
                    bad += fail(f"[{label}] {old} was not restored to HEAD's "
                                f"content: {(repo / old).read_text()!r}")
                root_writes = [f for e, f in events if e == "root_write"]
                if not root_writes:
                    bad += fail(f"[{label}] a rename-detected stray write was not "
                                f"journalled at all: {events}")
                elif old not in root_writes[0].get("reverted", []):
                    bad += fail(f"[{label}] the renamed-away tracked path was not "
                                f"journalled as reverted: {root_writes[0]}")
                if not (repo / new).exists():
                    bad += fail(f"[{label}] the untracked new path was removed from "
                                f"disk -- that disposition belongs to #186")
                if any(e == "root_write_revert_failed" for e, _ in events):
                    bad += fail(f"[{label}] a revert that worked was journalled as "
                                f"having failed: {events}")
    finally:
        loop.record, loop.log = real_record, real_log
    return bad


def check_guard_root_writes_detects_a_failed_revert_of_a_rename_source() -> int:
    """A `git checkout HEAD --` that fails on a RENAME-SHAPED stray must still
    be journalled as `root_write_revert_failed`.

    root_dirty_set() deliberately excludes a rename's *source* path from its
    dict -- it is the second field of the new path's record, not a record of
    its own (see root_dirty_set()'s own docstring). The post-checkout
    verification used to key on root_dirty_set() alone, so for exactly the
    path _root_fingerprints() goes out of its way to recover, a failed
    checkout was invisible to it: the source path was never "still dirty"
    under root_dirty_set(), `failed` came back empty, and the revert was
    journalled as successful while the tracked file stayed missing from ROOT.
    prompt_for() then raises FileNotFoundError for every subsequent issue in
    the run, with the journal insisting the revert worked.

    The checkout is forced to fail (simulating ENOSPC or a contended
    `.git/index.lock`) by monkeypatching `loop.git`, not by exhausting real
    disk or lock state, so this is deterministic. A plain (non-rename)
    tracked stray is forced to fail too, as a control: that shape was already
    caught by root_dirty_set() alone, so it must stay caught.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    real_record, real_log, real_git = loop.record, loop.log, loop.git
    events: list[tuple[str, dict]] = []
    loop.record = lambda event, **f: events.append((event, f))
    loop.log = lambda msg: None

    old, new = "orchestrator/prompts/reviewer.md", "orchestrator/prompts/stolen.md"
    plain = "orchestrator/prompts/unblocker.md"
    # Long enough that git's similarity heuristic calls the move a rename
    # rather than an unrelated add plus delete.
    head = "".join(f"reviewer instruction {n}\n" for n in range(40))

    def failing_checkout(args: list[str], cwd: Path = loop.ROOT) -> tuple[int, str]:
        if args[:1] == ["checkout"] and args[-1] in (old, plain):
            return 1, "simulated checkout failure (e.g. ENOSPC or a contended lock)"
        return real_git(args, cwd)

    bad = 0
    try:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            _guard_init(repo)
            (repo / "orchestrator" / "prompts").mkdir(parents=True)
            (repo / old).write_text(head)
            (repo / plain).write_text("unblocker\n")
            _guard_sh(repo, "add", "-A")
            _guard_sh(repo, "commit", "-q", "-m", "base")

            loop.git = failing_checkout
            with loop.guard_root_writes("135", "fix.2", root=repo):
                _guard_sh(repo, "mv", old, new)
                (repo / plain).write_text("POISONED\n")

            revert_failed = [f for e, f in events if e == "root_write_revert_failed"]
            if not revert_failed:
                bad += fail(f"a forced checkout failure on a rename-shaped stray "
                            f"(and a plain one) was not journalled as a failed "
                            f"revert at all: {events}")
            else:
                failed_paths = set(revert_failed[0].get("paths", []))
                if old not in failed_paths:
                    bad += fail(f"the rename SOURCE path's failed revert was not "
                                f"reported -- root_dirty_set() excludes rename "
                                f"sources by contract, so the verification "
                                f"silently passed while {old} stayed missing from "
                                f"ROOT: {revert_failed[0]}")
                if plain not in failed_paths:
                    bad += fail(f"the control (non-rename) failed revert regressed: "
                                f"{revert_failed[0]}")
            if (repo / old).exists():
                bad += fail(f"{old} unexpectedly exists on disk even though its "
                            f"checkout was forced to fail -- test setup is wrong")
    finally:
        loop.git, loop.record, loop.log = real_git, real_record, real_log
    return bad


def check_root_dirty_set_does_not_read_a_git_failure_as_clean() -> int:
    """`git status` failing must raise, not return `{}`.

    An empty dict is what a genuinely clean tree returns, so swallowing the
    exit code makes a failed capture indistinguishable from "nothing is
    dirty". In guard_root_writes() the *before* capture is the baseline that
    decides what counts as a stray, so an empty baseline means every path
    already dirty in ROOT is a stray write -- and the guard answers a stray
    write with `git checkout HEAD --`, which discards uncommitted content that
    exists in no commit, no index and no reflog. Eleven of the twelve blocking
    findings across six earlier review rounds on this change were in that
    class ("destroys content no agent wrote"); this is the same class reached
    through the tracked branch.

    root_dirty_set() is also the shared primitive #185 and #186 build on, so
    the unchecked exit code would propagate into both.

    Second half: when a capture does fail, the guard must degrade to doing
    nothing rather than to reverting everything -- and must not take the
    agent stage down with it, since a transient git error is not a reason to
    lose an hour of agent work.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    bad = 0
    with tempfile.TemporaryDirectory() as tmp:
        try:
            dirty = loop.root_dirty_set(Path(tmp))     # not a git repository
        except RuntimeError:
            pass
        else:
            bad += fail(f"root_dirty_set() reported a failed `git status` as a "
                        f"clean tree ({dirty!r}) -- in guard_root_writes()'s "
                        f"before-capture that arms a revert against every "
                        f"already-dirty path in ROOT")

    real_record, real_log = loop.record, loop.log
    real_fingerprints = loop._root_fingerprints
    events: list[tuple[str, dict]] = []
    loop.record = lambda event, **f: events.append((event, f))
    loop.log = lambda msg: None
    try:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            _guard_init(repo)
            (repo / "a.md").write_text("committed\n")
            _guard_sh(repo, "add", "-A")
            _guard_sh(repo, "commit", "-q", "-m", "base")
            # ROOT as #185's preflight leaves it: dirty, with local work that
            # lives in no commit and no index.
            (repo / "a.md").write_text("legit uncommitted local work\n")
            (repo / "scratch.md").write_text("legit untracked local work\n")

            calls: list[int] = []

            def failing_once(root: Path = loop.ROOT) -> dict[str, str]:
                calls.append(1)
                if len(calls) == 1:
                    raise RuntimeError("simulated `git status` failure")
                return real_fingerprints(root)

            loop._root_fingerprints = failing_once
            # The agent touches nothing in ROOT.
            with loop.guard_root_writes("135", "fix.1", root=repo):
                pass

            if (repo / "a.md").read_text() != "legit uncommitted local work\n":
                bad += fail("a failed before-capture let the guard revert a tracked "
                            "path no agent had touched, destroying uncommitted "
                            "content that exists in no commit and no reflog")
            if not (repo / "scratch.md").exists():
                bad += fail("a failed before-capture cost an untracked file that "
                            "no agent wrote")
            if any(e == "root_write" for e, _ in events):
                bad += fail(f"a failed capture was journalled as a stray write "
                            f"against an innocent issue: {events}")
    finally:
        loop._root_fingerprints = real_fingerprints
        loop.record, loop.log = real_record, real_log
    return bad


def check_guard_root_writes_revert_is_serialised_against_merges() -> int:
    """The revert must hold `_merge_lock` while it writes ROOT's index.

    `git checkout HEAD -- <path>` takes ROOT/.git/index.lock, and git does not
    retry a contended lock -- it exits 1 with "Unable to create
    '.git/index.lock': File exists". merge_worktree holds `_merge_lock` across
    its `git merge --ff-only` in ROOT, and that lock is the only thing
    serialising writers there: it is not held by agent stages, which run in
    the other MAX_PARALLEL worker threads. So an unsynchronised revert can
    kill a merge that is landing at that instant. merge_worktree returns False
    for that with no record() of its own, and _work then reports it as
    "rebase conflict or gate regression against latest main" -- a diagnosis
    that is false, and that burns an attempt (or blocks the issue outright at
    MAX_ATTEMPTS) on work that had already passed the gate and both reviewers.

    Retrying the checkout would not fix it: the victim of the race is the
    merge, not the revert, so only mutual exclusion helps.

    Asserted by holding `_merge_lock` from this thread and requiring the
    revert to block -- the deterministic form of the race, rather than a
    timing-dependent one that passes whenever the scheduler is kind. The
    read-only captures deliberately stay OUTSIDE the lock: they use
    `--no-optional-locks`, which is already safe against a concurrent merge,
    and serialising them would put every stage boundary behind a lock that
    merge_worktree can hold for a full GATE_TIMEOUT.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    real_record, real_log = loop.record, loop.log
    events: list[tuple[str, dict]] = []
    loop.record = lambda event, **f: events.append((event, f))
    loop.log = lambda msg: None
    bad = 0
    held = False
    try:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            _guard_init(repo)
            (repo / "reviewer.md").write_text("original\n")
            _guard_sh(repo, "add", "-A")
            _guard_sh(repo, "commit", "-q", "-m", "base")

            loop._merge_lock.acquire()
            held = True
            done = threading.Event()

            def stage() -> None:
                with loop.guard_root_writes("135", "fix.1", root=repo):
                    (repo / "reviewer.md").write_text("original\nsix stray lines\n")
                done.set()

            worker = threading.Thread(target=stage, daemon=True)
            worker.start()
            if done.wait(2.0):
                bad += fail("guard_root_writes() reverted a stray write to ROOT "
                            "without holding _merge_lock -- `git checkout HEAD --` "
                            "takes ROOT/.git/index.lock, so it can kill a "
                            "concurrent `git merge --ff-only` in merge_worktree, "
                            "which _work then misreports as a rebase conflict")
            loop._merge_lock.release()
            held = False

            if not done.wait(60):
                bad += fail("guard_root_writes() never completed after _merge_lock "
                            "was released -- the revert is deadlocked")
            worker.join(5)
            content = (repo / "reviewer.md").read_text()
            if content != "original\n":
                bad += fail(f"the serialised revert did not restore HEAD's "
                            f"content: {content!r}")
            if not any(e == "root_write" for e, _ in events):
                bad += fail(f"the serialised revert was not journalled: {events}")
    finally:
        if held:
            loop._merge_lock.release()
        loop.record, loop.log = real_record, real_log
    return bad


def check_implementer_transcript_path_resolves_from_a_worktree() -> int:
    """A path a prompt names must resolve from the cwd that prompt runs in.

    Implementers run with cwd=<worktree> -- `invoke()` does
    `Popen(..., cwd=str(wt))` -- and logs/ is gitignored and lives at
    ORCH/logs, a *sibling* of ORCH/worktrees. So no logs/ directory exists
    anywhere inside a worktree. The preserved-attempt salvage step shipped
    pointing at `logs/<N>/`, which is ENOENT from there: an agent that follows
    it reads an empty directory, concludes the failed attempt left no trace,
    and then judges ~1,500 lines of preserved code having never seen the
    findings that sank it -- defeating the "evidence, not a restore" rule that
    step exists to enforce.

    unblocker.md carries the same unresolvable spelling and survives only
    because the loop injects an absolute logdir next to it ("absolute path;
    you are in a worktree"). The implementer's `extra` carries just the
    previous gate failure, and is empty on exactly the re-claim-after-rescope
    path the salvage step is for -- so here the prompt text is the whole
    mechanism, and it is checked here because a Markdown file has no other
    executable surface for a gate to test.

    The required prefix is derived from the loop's own constants rather than
    hardcoded, so relocating logs/ or nesting worktrees one level deeper fails
    this check instead of leaving a stale `../../` blessed. It is a relation
    between two paths under the same ROOT, so it yields the same answer
    whether the gate runs in the main checkout or inside a worktree.
    """
    sys.path.insert(0, str(ROOT / "orchestrator"))
    import loop

    rel = os.path.relpath(loop.LOGS, loop.WORKTREES / "0").replace(os.sep, "/")
    text = (ROOT / "orchestrator" / "prompts" / "implementer.md").read_text()
    bad = 0

    if f"{rel}/" not in text:
        bad += fail(
            f"prompts/implementer.md never names `{rel}/` -- the only spelling "
            "of the transcript directory that resolves from an implementer's "
            "cwd, which is a worktree and not the main checkout")

    # Naming the canonical location too is useful, but only if the prompt also
    # says it is somewhere the agent is not standing.
    unresolvable = sorted({m for m in re.findall(r"`([^`]*logs/[^`]*)`", text)
                           if not m.startswith(("..", "/"))})
    if unresolvable and "main checkout" not in text:
        bad += fail(
            f"prompts/implementer.md names {unresolvable} but never says they "
            "live in the main checkout -- from a worktree those are ENOENT, "
            "and an agent reads that as 'the attempt left no trace'")
    return bad


if __name__ == "__main__":
    bad = check_parses()
    if bad:                      # do not try to run code that does not parse
        sys.exit(1)
    sys.exit(1 if check_runs() + check_prompts() + check_dep_parsing()
             + check_gate_targets_the_worktree() + check_no_absorbing_states()
             + check_blocked_has_a_recovery_path() + check_waiting_is_annotation_only()
             + check_silent_reviewer_is_not_a_pass()
             + check_review_stage_retries_a_silent_reviewer()
             + check_pre_implementer_stages_restore()
             + check_unmerged_work_survives_reclaim()
             + check_filing_contract_is_stated() + check_reviewer_restore()
             + check_priority_leads_the_queue()
             + check_verdict_parsing() + check_agent_routing()
             + check_config_defaults() + check_config_overrides()
             + check_record_repairs_torn_journal()
             + check_retro_cycle_survives_restart()
             + check_retro_callers_derive_the_cycle()
             + check_retro_no_clobber() + check_retro_cycle_claim_is_atomic()
             + check_retro_orphaned_claim_does_not_skew_forever()
             + check_retro_through_is_snapshotted_before_invoke()
             + check_retro_unanalysed_pass_does_not_advance_through()
             + check_retro_damaged_line_and_tolerant_prompt_command()
             + check_retro_first_pass_still_gets_tolerant_recipe()
             + check_retro_final_does_not_inherit_a_stale_report()
             + check_reviewer_diff_not_silently_truncated()
             + check_reviewer_patch_file_is_complete_and_applies()
             + check_post_fix_empty_diff_does_not_merge()
             + check_rolling_pool_abandons_safely() + check_rolling_pool()
             + check_root_dirty_set_parses_porcelain_z()
             + check_guard_root_writes_reverts_tracked_stray()
             + check_guard_root_writes_catches_stray_on_already_dirty_path()
             + check_guard_root_writes_leaves_untracked_stray_on_disk()
             + check_guard_root_writes_ignores_concurrent_merge()
             + check_guard_root_writes_restores_a_renamed_away_tracked_file()
             + check_guard_root_writes_detects_a_failed_revert_of_a_rename_source()
             + check_root_dirty_set_does_not_read_a_git_failure_as_clean()
             + check_guard_root_writes_revert_is_serialised_against_merges()
             + check_implementer_transcript_path_resolves_from_a_worktree() else 0)
