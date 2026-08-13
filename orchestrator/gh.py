#!/usr/bin/env python3
"""GitHub Issues as the work queue.

Replaces the static backlog. Issues are the queue, labels are the state
machine, and agents are allowed to write back to it -- questioning a task,
splitting it, or filing what they discovered. The point is that the plan is
allowed to change while the loop runs, which a hand-written JSON file cannot do.

Label state machine:

    fr:ready ──claim──► fr:claimed ──┬─ merged ─────────► issue closed
        ▲    (+fr:waiting            ├─ spec is wrong ──► resolver  ──┐
        │     while deps are open)   └─ failed 3x ──────► fr:blocked  │
        │                                                    │        │
        └──── requeue (re-scoped) ◄── unblocker ◄────────────┘        │
        └──── requeue (re-scoped) ◄───────────────────────────────────┘
                                     ...or either closes it with evidence

**No state in this machine is absorbing.** That is the invariant, and it is
here because it was violated. `fr:questioned` used to be where a challenged
spec waited for a human who was not coming, so the resolver was added to
adjudicate it. `fr:blocked` was then exactly the same defect wearing different
paint -- `block()` set it and nothing anywhere removed it -- and it nearly cost
the project a night: the toolchain issue that twelve others sat behind failed
three times, parked, and was rescued only because a person happened to read the
label at 02:30 and re-scope it by hand. The unblocker is that person, written
down. `requeue` stamps `Revisions: N` and `unblock` stamps `Recoveries: N`, so
neither exchange can run forever.

`fr:waiting` is the one label that is not a state: it is an annotation the loop
maintains so that the dependency filter `claimable()` has always applied is
visible from outside. Nothing schedules on it.

Dependencies live in the issue body as `Depends on: #12, #13`, and mean
*behaviour this issue calls into* -- not files that must exist, which the rebase
onto `main` provides anyway. An issue is claimable only once every issue it
depends on is closed.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from dataclasses import dataclass, field

LABELS = {
    "fr:ready": ("0e8a16", "Claimable by an agent"),
    "fr:claimed": ("fbca04", "An agent is working on this"),
    # Annotation, not a state: it rides alongside fr:ready and nothing schedules
    # on it. claimable() has always filtered on unmet dependencies, but it did so
    # silently -- from the outside all 49 ready issues looked equally available
    # while 10 of them could not be picked by anyone. sync_waiting() mirrors that
    # hidden predicate into the UI so the queue you read is the queue the loop
    # sees. Distinct from fr:blocked on purpose: waiting is the system working,
    # blocked is the system stuck.
    "fr:waiting": ("bfd4f2", "Ready, but waiting on an open dependency"),
    "fr:blocked": ("b60205", "Failed repeatedly; awaiting recovery"),
    "fr:questioned": ("5319e7", "An agent challenged this spec; needs re-scoping"),
    "fr:followup": ("c5def5", "Filed by an agent mid-task"),
    # Filed by the retrospective against the loop itself, and claimable like any
    # other issue -- there is no human to promote it. Changes to prompts, docs
    # and the gate take effect on the next invocation because the loop re-reads
    # them; a merge touching loop.py or gh.py makes the loop re-exec into the
    # new code at the next batch boundary (see loop.py: restart_into_new_code).
    "fr:meta": ("d4c5f9", "Improvement to the loop itself"),
}

ISSUE_RE = re.compile(r"#(\d+)")
# A line that OPENS with "depends on" is metadata; the same words inside a
# sentence are prose. Both halves of that matter, and getting the second one
# wrong is what this pattern is for.
#
# Extraction used to run `depends on:?\s*(.+)` unanchored over the whole body,
# so any sentence containing the phrase donated every issue number to its right.
# #56 -- the issue whose entire job was cutting spurious dependency edges --
# said "Audit every `Depends on:` edge in the port graph -- #8, #10, #11, #12,
# #13" and thereby declared dependencies on all of them. It could not be claimed
# until the port it existed to unblock had finished. The better an issue
# documented the dependency graph, the more unreachable it made itself.
#
# Anchoring fixes that, and the warning below covers the other direction: a line
# that is metadata but yields no #N -- "Depends on: issues 7 and 8", or #37's
# "Depends on: nothing" -- reads as unblocked and gets claimed before its
# prerequisites exist. Bare numbers are deliberately NOT parsed as a fallback:
# "Depends on: PHP 8.5" would invent dependencies on #8 and #5, which is a worse
# failure than the one it fixes.
DEP_LINE_RE = re.compile(r"^[ \t]*[-*]?[ \t]*depends on\b", re.I | re.M)
DEP_RE = re.compile(r"^[ \t]*[-*]?[ \t]*depends on\b:?(.*)$", re.I | re.M)
_warned: set[tuple[int, str]] = set()


def gh(args: list[str], check: bool = False, stdin: str | None = None) -> tuple[int, str]:
    p = subprocess.run(["gh", *args], capture_output=True, text=True, input=stdin)
    out = (p.stdout + p.stderr).strip()
    if check and p.returncode != 0:
        raise RuntimeError(f"gh {' '.join(args)} failed: {out}")
    return p.returncode, out


@dataclass
class Issue:
    number: int
    title: str
    body: str
    labels: list[str] = field(default_factory=list)
    state: str = "OPEN"

    @property
    def deps(self) -> list[int]:
        """Union of every `Depends on:` line, not the first one.

        Taking the first match means an issue whose prose says "depends on the
        state port" above its structured line parses as having no dependencies
        at all -- so it gets claimed before the crate it needs exists, fails
        the gate three times for a reason the implementer cannot fix, and ends
        up blocked. Unioning is also right for a body listing deps as bullets,
        and prose that mentions no issue number contributes nothing.

        Lines only (see DEP_RE): the phrase inside a sentence is prose, and
        reading it as metadata let a body's own description of the dependency
        graph become dependencies.
        """
        body = self.body or ""
        found = {int(n) for m in DEP_RE.finditer(body)
                 for n in ISSUE_RE.findall(m.group(1))}
        for line in body.splitlines():
            if DEP_LINE_RE.match(line) and not ISSUE_RE.search(line):
                key = (self.number, line.strip()[:80])
                if key not in _warned:      # deps is read on every queue poll
                    _warned.add(key)
                    print(f"WARN #{self.number}: dependency line parsed to nothing, "
                          f"issue reads as unblocked: {line.strip()[:120]!r}",
                          file=sys.stderr)
        return sorted(found)

    @property
    def gate(self) -> str:
        """Gate profile, declared in the body as `Gate: bootstrap`."""
        m = re.search(r"^\s*gate:\s*(\w+)", self.body or "", re.I | re.M)
        return m.group(1).lower() if m else "default"

    @property
    def agent(self) -> str:
        m = re.search(r"^\s*agent:\s*(\w+)", self.body or "", re.I | re.M)
        return m.group(1).lower() if m else "codex"

    @property
    def revisions(self) -> int:
        """How many times the resolver has already re-scoped this issue.

        Highest match wins, not the first. The resolver rewrites the body
        itself, so it can leave a stale counter above the new one; reading the
        first would freeze the count and let critic and resolver hand the issue
        back and forth forever with nobody watching.
        """
        found = re.findall(r"^\s*revisions:\s*(\d+)", self.body or "", re.I | re.M)
        return max((int(n) for n in found), default=0)

    @property
    def recoveries(self) -> int:
        """How many times this issue has been rescued out of fr:blocked.

        Separate from `revisions` because they bound different loops. A critic
        objecting to a spec and an implementer failing a gate three times are
        different failures, and letting one exhaust the other's budget means an
        issue that was re-scoped twice gets one rescue instead of two. Highest
        match wins, for the same reason as revisions: the unblocker rewrites the
        body and can leave a stale counter above the new one.
        """
        found = re.findall(r"^\s*recoveries:\s*(\d+)", self.body or "", re.I | re.M)
        return max((int(n) for n in found), default=0)


def ensure_labels() -> None:
    for name, (color, desc) in LABELS.items():
        gh(["label", "create", name, "--color", color, "--description", desc, "--force"])


def fetch(label: str | None = None, state: str = "open") -> list[Issue]:
    args = ["issue", "list", "--state", state, "--limit", "200",
            "--json", "number,title,body,labels,state"]
    if label:
        args += ["--label", label]
    rc, out = gh(args)
    if rc != 0:
        return []
    try:
        raw = json.loads(out)
    except json.JSONDecodeError:
        return []
    return [
        Issue(number=i["number"], title=i["title"], body=i.get("body") or "",
              labels=[l["name"] for l in i.get("labels", [])], state=i.get("state", "OPEN"))
        for i in raw
    ]


def closed_numbers() -> set[int]:
    return {i.number for i in fetch(state="closed")}


def claimable() -> list[Issue]:
    """Ready issues whose dependencies are all closed, most valuable first.

    Order used to be whatever `gh issue list` returned, which is newest-first.
    Issue numbers only increase and agents file followups while they work, so
    every followup an agent filed outranked every port issue still waiting --
    permanently, because nothing ever lowers a number. The loop consumed its
    own exhaust: after one merge the next three picks were all housekeeping it
    had filed for itself, while the issue designated the conformance oracle sat
    sixth and the workspace that gates ten others was unreachable.

    So rank by how much an issue unblocks. An issue ten others depend on is
    worth more than a two-line docs correction, and nothing in the queue said
    so. Housekeeping breaks ties downward rather than being excluded outright:
    the loop repairing itself is what makes an unattended run survivable, and a
    rule that always starves fr:meta would trade one runaway for another.
    """
    done = closed_numbers()
    ready = [i for i in fetch(label="fr:ready") if all(d in done for d in i.deps)]

    # How many still-open issues are waiting on each one.
    dependants: dict[int, int] = {}
    for i in fetch():
        for d in i.deps:
            dependants[d] = dependants.get(d, 0) + 1

    def rank(i: Issue) -> tuple[int, int, int]:
        housekeeping = 1 if {"fr:followup", "fr:meta"} & set(i.labels) else 0
        return (-dependants.get(i.number, 0), housekeeping, i.number)

    return sorted(ready, key=rank)


def sync_waiting() -> tuple[int, int]:
    """Mirror the dependency predicate onto the issues as `fr:waiting`.

    Pure annotation: `claimable()` is unchanged and nothing reads this label
    back. It exists because the scheduling rule was invisible -- `fr:ready` was
    stamped on issues that no worker could claim, so the queue a human read and
    the queue the loop saw disagreed, and the disagreement was exactly the set
    of issues that mattered.

    Only writes on a change. This runs on every poll, which is every 30s while
    the loop is waiting for dependencies, and re-labelling 49 issues a minute
    would spend the API budget on saying nothing new.

    Returns (added, removed) for the caller to log.
    """
    done = closed_numbers()
    added = removed = 0
    for i in fetch(label="fr:ready"):
        waiting = not all(d in done for d in i.deps)
        tagged = "fr:waiting" in i.labels
        if waiting and not tagged:
            gh(["issue", "edit", str(i.number), "--add-label", "fr:waiting"])
            added += 1
        elif tagged and not waiting:
            gh(["issue", "edit", str(i.number), "--remove-label", "fr:waiting"])
            removed += 1
    return added, removed


def dependants() -> dict[int, list[int]]:
    """Open issue numbers waiting on each issue, keyed by the issue they need."""
    out: dict[int, list[int]] = {}
    for i in fetch():
        for d in i.deps:
            out.setdefault(d, []).append(i.number)
    return out


def blocked_gating_work() -> list[tuple[Issue, list[int]]]:
    """Blocked issues that other open issues are waiting on, worst first.

    This is the queue the recovery pass drains, and the ordering is the whole
    point. Last night #5 -- the toolchain twelve issues sat behind -- was
    blocked at 02:30 and the run did not starve: fourteen housekeeping issues
    were claimable, so the loop stayed busy on its own exhaust while the port
    was dead. Recovery keyed on an empty queue would never have fired. What
    makes a block urgent is what it gates, not whether there is other work.
    """
    waiting_on = dependants()
    done = closed_numbers()
    out = [(i, sorted(n for n in waiting_on.get(i.number, []) if n not in done))
           for i in fetch(label="fr:blocked")]
    return sorted((x for x in out if x[1]), key=lambda x: -len(x[1]))


def unblock(n: int, recovery: int) -> bool:
    """Return a recovered issue to the queue, stamping the recovery count.

    Mirrors requeue(), including re-reading the body the unblocker may have
    rewritten, and keeps its own counter so a re-scope by the resolver and a
    rescue after three failed attempts cannot exhaust each other's budget.
    """
    rc, out = gh(["issue", "view", str(n), "--json", "body"])
    if rc != 0:
        return False
    try:
        body = json.loads(out).get("body") or ""
    except json.JSONDecodeError:
        return False
    line = f"Recoveries: {recovery}"
    if re.search(r"^\s*recoveries:\s*\d+", body, re.I | re.M):
        body = re.sub(r"^\s*recoveries:\s*\d+", line, body, count=1, flags=re.I | re.M)
    else:
        body = f"{body.rstrip()}\n\n{line}\n"
    rc, _ = gh(["issue", "edit", str(n), "--body-file", "-"], stdin=body)
    if rc != 0:
        return False
    rc, _ = gh(["issue", "edit", str(n), "--add-label", "fr:ready",
                "--remove-label", "fr:blocked,fr:claimed"])
    return rc == 0


def claim(n: int) -> bool:
    """Take an issue. Returns False if someone else got there first.

    gh has no compare-and-swap, so this re-reads the labels afterwards and
    treats a missing fr:claimed as a lost race. Parallel workers in one process
    are also serialised by a lock in the caller; this guards the rest.
    """
    rc, _ = gh(["issue", "edit", str(n), "--add-label", "fr:claimed",
                "--remove-label", "fr:ready"])
    if rc != 0:
        return False
    rc, out = gh(["issue", "view", str(n), "--json", "labels"])
    if rc != 0:
        return False
    try:
        labels = {l["name"] for l in json.loads(out).get("labels", [])}
    except json.JSONDecodeError:
        return False
    return "fr:claimed" in labels


def release(n: int, label: str = "fr:ready") -> None:
    gh(["issue", "edit", str(n), "--add-label", label, "--remove-label", "fr:claimed"])


def block(n: int, why: str) -> None:
    comment(n, f"**Blocked by the loop.**\n\n{why[:60000]}")
    gh(["issue", "edit", str(n), "--add-label", "fr:blocked", "--remove-label", "fr:claimed"])


def question(n: int, critique: str) -> None:
    comment(n, f"**An agent challenged this issue rather than implementing it.**\n\n{critique[:60000]}")
    gh(["issue", "edit", str(n), "--add-label", "fr:questioned", "--remove-label", "fr:claimed"])


def requeue(n: int, revision: int) -> bool:
    """Return a re-scoped issue to the queue, stamping the revision count.

    The resolver rewrites the body itself; this re-reads whatever it wrote so a
    concurrent edit is not clobbered, then upserts the counter that stops the
    critic and the resolver passing an issue back and forth indefinitely.
    """
    rc, out = gh(["issue", "view", str(n), "--json", "body"])
    if rc != 0:
        return False
    try:
        body = json.loads(out).get("body") or ""
    except json.JSONDecodeError:
        return False
    line = f"Revisions: {revision}"
    if re.search(r"^\s*revisions:\s*\d+", body, re.I | re.M):
        body = re.sub(r"^\s*revisions:\s*\d+", line, body, count=1, flags=re.I | re.M)
    else:
        body = f"{body.rstrip()}\n\n{line}\n"
    rc, _ = gh(["issue", "edit", str(n), "--body-file", "-"], stdin=body)
    if rc != 0:
        return False
    rc, _ = gh(["issue", "edit", str(n), "--add-label", "fr:ready",
                "--remove-label", "fr:claimed,fr:questioned"])
    return rc == 0


def comment(n: int, body: str) -> None:
    subprocess.run(["gh", "issue", "comment", str(n), "--body-file", "-"],
                   input=body, text=True, capture_output=True)


def close(n: int, summary: str) -> None:
    comment(n, summary)
    gh(["issue", "close", str(n)])


def create(title: str, body: str, labels: list[str]) -> int | None:
    rc, out = gh(["issue", "create", "--title", title, "--body", body,
                  "--label", ",".join(labels)])
    if rc != 0:
        return None
    m = ISSUE_RE.search(out) or re.search(r"/issues/(\d+)", out)
    return int(m.group(1)) if m else None


if __name__ == "__main__":
    import sys
    if len(sys.argv) > 1 and sys.argv[1] == "labels":
        ensure_labels()
        print("labels ensured")
    else:
        for i in claimable():
            print(f"#{i.number:<4} [{i.gate}/{i.agent}] {i.title}  deps={i.deps}")
