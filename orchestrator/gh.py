#!/usr/bin/env python3
"""GitHub Issues as the work queue.

Replaces the static backlog. Issues are the queue, labels are the state
machine, and agents are allowed to write back to it -- questioning a task,
splitting it, or filing what they discovered. The point is that the plan is
allowed to change while the loop runs, which a hand-written JSON file cannot do.

Label state machine:

    fr:ready ──claim──► fr:claimed ──┬─ merged ─► issue closed
        ▲                            ├─ failed ─► fr:blocked
        │                            └─ spec is wrong ─► resolver
        └──── requeue (re-scoped) ─────────────┴─ or closed with evidence

fr:questioned is a last resort, not a resting place: nobody is coming to
re-scope it. The resolver adjudicates the objection and puts the issue back in
play or kills it, and `requeue` stamps `Revisions: N` so that exchange cannot
run forever.

Dependencies live in the issue body as `Depends on: #12, #13`. An issue is
claimable only once every issue it depends on is closed.
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
    "fr:blocked": ("b60205", "Failed repeatedly; needs a human"),
    "fr:questioned": ("5319e7", "An agent challenged this spec; needs re-scoping"),
    "fr:followup": ("c5def5", "Filed by an agent mid-task"),
    # Filed by the retrospective against the loop itself, and claimable like any
    # other issue -- there is no human to promote it. Changes to prompts, docs
    # and the gate take effect on the next invocation because the loop re-reads
    # them; a merge touching loop.py or gh.py makes the loop re-exec into the
    # new code at the next batch boundary (see loop.py: restart_into_new_code).
    "fr:meta": ("d4c5f9", "Improvement to the loop itself"),
}

DEP_RE = re.compile(r"depends on:?\s*(.+)", re.I)
ISSUE_RE = re.compile(r"#(\d+)")
# A line that opens with "depends on" is metadata, not prose. If one of those
# yields no #N at all -- "Depends on: issues 7 and 8" -- the issue reads as
# unblocked and gets claimed before its prerequisites exist. Bare numbers are
# deliberately NOT parsed as a fallback: "Depends on: PHP 8.5" would invent
# dependencies on #8 and #5, which is a worse failure than the one it fixes.
DEP_LINE_RE = re.compile(r"^[ \t]*[-*]?[ \t]*depends on\b", re.I | re.M)
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
