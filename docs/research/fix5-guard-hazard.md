# fix5: guard_root_writes() hazard to human edits in ROOT

Research date: 2026-08-18. Branch: `support/opencode`. Research only — no files
modified.

## Q1. What exactly does the `root_write` record contain today?

**TL;DR** Five fields: `ts` (added by `record()`), `event="root_write"`, `issue`
(the guard's `tid`), `stage` (its `tag`), `paths` (all changed paths, sorted),
`reverted` (tracked paths, `git checkout HEAD --`-ed), `not_reverted`
(untracked paths, left on disk). **No content, no diff, no fingerprint.** The
pre-revert content of a reverted file is not captured anywhere.

The record() call (orchestrator/loop.py:925-927):

```python
log(f"    !! stray write(s) to ROOT during {tag} ({tid}): {changed}")
record("root_write", issue=tid, stage=tag, paths=changed,
       reverted=tracked, not_reverted=untracked)
```

- `changed` is the content-fingerprint delta, computed at loop.py:918-919 as
  `sorted(p for p, fp in after.items() if before.get(p) != fp)`.
- `tracked`/`untracked` split by `git cat-file -e HEAD:<p>` (loop.py:921-924).
- `record()` itself (loop.py:203-232) prepends `{"ts": now(), "event": ...}`
  and appends one JSON line per event; fields are whatever `**fields` carried.

A second event exists: `root_write_revert_failed` (loop.py:985),
`record("root_write_revert_failed", issue=tid, stage=tag, paths=failed)` —
zero occurrences in the live journal.

**The incident, verbatim from orchestrator/logs/events.jsonl** (the only two
`root_write` events ever recorded, both 2026-08-17, both from the unblock
lane's `unblock.0` stage):

```json
{"ts":"2026-08-17T20:15:14+00:00","event":"root_write","issue":"unblock-26",
 "stage":"unblock.0","paths":["orchestrator/loop.py"],
 "reverted":["orchestrator/loop.py"],"not_reverted":[]}
{"ts":"2026-08-17T20:51:42+00:00","event":"root_write","issue":"unblock-30",
 "stage":"unblock.0","paths":["README.md","orchestrator/loop.py",
 "scripts/check_orchestrator.py","scripts/supervise.sh"],
 "reverted":["README.md","orchestrator/loop.py","scripts/check_orchestrator.py",
 "scripts/supervise.sh"],"not_reverted":[]}
```

The 20:51:42 record is the lane-split implementation being eaten — the exact
four files named in the incident report. A human reading this record can learn
*what was destroyed* but nothing about *what was lost*; the diff of the
reverted paths exists nowhere.

## Q2. Is anything recorded when nothing changed? Journal noise?

**TL;DR** No. `if changed:` (loop.py:920) gates both the log line and the
`record()`; a clean stage writes nothing. Also, a failed before-capture
disarms the guard for the stage with only a log line, no event (loop.py:897-902).
**Do not add a per-stage `guard_armed` event** — record() is append-per-event,
and every worktree agent stage is guarded (6 sites), so that would add roughly
a thousand+ events to a journal that is already the retrospective's input.

Journal today (orchestrator/logs/events.jsonl, 3925 events):

| count | event |
|---:|---|
| 1737 | agent_error |
| 1560 | recover_failed |
| 96 | review_verdicts |
| 84 | review_block |
| 74 | gate_fail |
| 55 | empty_diff |
| 54 | blocked |
| 41 | retrospective |
| 39 | merged |
| 32 | review_diversity_lost |
| 31 | critic_revise |
| 29 | resolved |
| 28 | pushed |
| 26 | recovered |
| 11 | branch_preserved |
| 8 | self_restart |
| 6 | agent_fallback |
| 5 | rebase_conflict |
| 4 | reclaimed |
| 2 | **root_write** |
| 1 each | journal_reset, agent_timeout, review_incomplete |

`root_write` is the rarest meaningful event after the one-offs. A
`guard_armed`-per-stage event would swamp the journal (the two noise events
alone are 3297 of 3925 lines) and would not help the human-victim case: the
victim needs to find *revert* events, and there is exactly one event name for
that today. Keep record-on-detection-only. The retrospective reads this file
(line 206-210 docstring: "The retrospective reads THIS"); noise is a real cost.

## Q3. Can the guard distinguish human edits at all?

**TL;DR** No, not for the same file — and the current design already
implements every defensible version of the trade. Details per sub-question:

**(i) Content plausibility matching — impossible.** The guard's own docstring
says it in the "accepted trade" paragraph (loop.py:867-872):

> If a strayed-upon path also carried legitimate uncommitted local edits, those
> go with the stray, because nothing here can tell the two apart once they are
> in the same file. That is the accepted trade for closing the prompt-poisoning
> hole, and it is the reason the capture below must never mistake a git failure
> for a clean ROOT: a false positive here is not a harmless extra checkout, it
> is data loss.

The only signal available is `before`/`after` content fingerprints
(loop.py:814-846, 898-919). Both states are just bytes on disk; nothing
records who wrote them. "What the agent could plausibly write" is not a
classifier the loop can run — the agent writes arbitrary code, docs, and
scripts.

**(ii) Diff of the reverted paths — currently NOT captured.** The
`record("root_write", ...)` call (loop.py:926-927) carries paths only. No
`git diff` is ever run against ROOT in the guard; `worktree_diff()`
(loop.py:989-1007) is a worktree-only helper. At the moment of detection the
working-tree content of each changed path is still on disk — the diff
`git diff HEAD -- <path>` at that instant is exactly the content about to be
destroyed (human edits *plus* agent stray). Capturing it into the record is
the single highest-value change available: it turns the journal from a
notification into a recovery source. Risk is negligible — `root_write` fires
~never (2 events in 3925).

**(iii) "Refuse to revert if the path was already dirty at run start" — would
weaken the guard by design, and a test pins the opposite.** What the guard
does today: the `before` fingerprint (loop.py:898) is the *per-stage*
baseline. Pre-existing dirt is in that baseline, so a path that is dirty and
*untouched* by the agent is never reverted (no fingerprint change). But if the
agent *does* write to that same path, the delta includes it and the revert
goes to HEAD — human edit and all. The 879-882 comment explains why there is
no "already dirty" exemption and why keying on `root_dirty_set()`'s status
codes would be wrong:

> Keyed on _root_fingerprints(), a content diff, not on root_dirty_set()'s
> status codes -- see that function's docstring for why a status-code delta
> would ship disarmed on every path that was already dirty when the run
> started.

`root_dirty_set()` (loop.py:713-723) returns `path -> XY status code` for
every dirty path — it is the primitive #185/#186 consume, and the guard
deliberately does not use it for the delta. Crucially, #185's preflight leaves
ROOT dirty for the whole run, and occurrence 2 of #168 (reviewer.md carrying
uncommitted residue) is *exactly* the already-dirty path shape. The test
`check_guard_root_writes_catches_stray_on_already_dirty_path`
(scripts/check_orchestrator.py:3120-3169) pins that a stray layered on
pre-existing dirt MUST be detected and reverted to HEAD (asserts
`content != "v1\n"` fails). Any "skip paths dirty at run start" rule breaks
this test and reopens the prompt-poisoning hole — which is why the docstring
frames a false negative as the worse failure (loop.py:778-784: an empty
baseline "destroys content no agent wrote, which is the exact failure class
that ended six earlier review rounds on this change").

So the asymmetry is deliberate and tested: **the guard may eat a human edit on
a same-file overlap; it may not fail to revert an agent stray.** The only
remaining moves are (a) warn the human, (b) make the destruction attributable
and recoverable. Recording the revert BEFORE doing it and recording the diff
BEFORE reverting (both under `_merge_lock` is unnecessary; a plain diff read
has no index-lock interaction, unlike checkout) are the implementable halves
of (b). "Record before revert" is already satisfied in the ordering sense —
`record()` at 926-927 runs before the checkout loop at 947-953 — but the
record lacks the content.

## Q4. What does check_orchestrator.py test about the guard?

Eight checks, all `check_guard_root_writes_*`, registered at
scripts/check_orchestrator.py:3712-3719, all monkeypatching `loop.record`/
`loop.log` to capture events into a local list:

| check | line | asserts |
|---|---|---|
| reverts_tracked_stray | 3043 | revert restores HEAD content; `root_write` exists with `paths==["reviewer.md"]`, `reverted==["reviewer.md"]`; no `root_write_revert_failed`; also the `git add`-ed stray shape |
| catches_stray_on_already_dirty_path | 3120 | stray appended to pre-dirty tracked path is reverted AND journalled |
| leaves_untracked_stray_on_disk | 3172 | untracked stray survives on disk; in `not_reverted`; no revert_failed event |
| ignores_concurrent_merge | 3222 | ff-merge mid-stage: NO `root_write` event, merged content survives |
| restores_a_renamed_away_tracked_file | 3283 | 4 rename shapes; old path restored, in `reverted`; new path on disk; no revert_failed |
| detects_a_failed_revert_of_a_rename_source | 3387 | forced checkout failure → `root_write_revert_failed` names rename source AND plain control |
| root_dirty_set_does_not_read_a_git_failure_as_clean | 3467 | failed `git status` raises; failed before-capture → no revert of pre-existing dirt, no `root_write` event |
| revert_is_serialised_against_merges | 3550 | revert blocks on `_merge_lock`; after release, reverts and journals |

**Which break under the proposed changes:**

- Adding a `diff`/content field to the `root_write` record: **none break** —
  every assertion reads the record via `.get(...)` on `paths`, `reverted`,
  `not_reverted`, or checks event *existence* (`any(e == "root_write" ...)`).
  No test asserts the field set is closed.
- Adding a new event name (e.g. `guard_armed`): **none break** — the
  existence checks are equality on specific names, not "no events at all"
  (except `ignores_concurrent_merge` at 3271 and `root_dirty...` at 3541,
  which check for the absence of `root_write` specifically).
- Changing revert behavior on pre-dirty paths (a "skip human dirt" rule):
  **`catches_stray_on_already_dirty_path` (3120) breaks by design** — and so
  does the prompt-poisoning guarantee it pins.
- Changing `paths`/`reverted`/`not_reverted` semantics: breaks 3043, 3090,
  3172, 3283, 3387.

Note the checks use `loop.guard_root_writes("135", "fix.1", root=repo)` with
tid/tag conventions that must match the `issue=`/`stage=` fields only in
meaning, not format — a field-rename would break nothing unless the field
*disappears*.

## Q5. What does README.md / docs/ARCHITECTURE.md currently document?

**README.md — nothing about the guard.** The "Running the loop" section is
README.md:81-129: lanes (`run`/`unblock` under `scripts/supervise.sh`, 97-103),
knobs (105-115), current opencode defaults (117-123), and the human-decision
paragraph at 125-128:

> **You have to start it yourself.** The agents run with permission prompts
> disabled — that is what "unattended" requires — and authorizing a multi-hour
> agent fleet with that much latitude is a decision for you to make, not
> something to be started on your behalf. Logs land in
> `orchestrator/logs/<task-id>/`.

The self-improvement paragraph (161-186) describes the journal
(`events.jsonl`) and the retrospective but never mentions `root_write`.
`grep -i 'guard\|stray\|root_write' README.md` → zero hits (the only hit is
"flock-guarded journal" at 103, unrelated).

**docs/ARCHITECTURE.md — not a loop document.** It is the Rust-port design doc
("Every claim about upstream behaviour below cites
`vendor/frankenphp/<file>:<line>`", lines 1-9). `grep guard|stray|ROOT|dirty`
→ only Rust-server content (e.g. "the lock guarding the handler" at 305). The
orchestrator's only documentation is README.md plus the docstrings in
loop.py and the retros. So the README warning is the *only* home for the
hazard short of writing new docs.

## Q6. Minimal robust design

Recommended, in order of value/effort:

**(a) README warning.** Placement: end of the "Running the loop" section,
directly after the "You have to start it yourself." paragraph (README.md:128),
before "### How an issue is processed". That paragraph is the one place a
human decides to interact with a live loop; the warning must ride with it.
Suggested wording (blockquote, matching the file's `> ` style at 213):

> > **Do not edit tracked files in the repo root while `run`/`unblock` are
> > live.** Every worktree agent stage is bracketed by a guard that reverts any
> > tracked file an agent writes during the stage — including files that
> > carried your uncommitted edits (same-file writes are indistinguishable;
> > see `guard_root_writes()` in `orchestrator/loop.py`). Uncommitted edits to
> > files an agent does *not* touch are safe. If a revert eats your work, the
> > event is in `orchestrator/logs/events.jsonl` (`{"event": "root_write", ...}`)
> > with the paths, and the record now also carries the reverted diff. This has
> > happened: 2026-08-17, `unblock.0`, four files.

The wording must be concrete about (1) the danger window is *same file, live
stage*, (2) untouch ed files are safe, (3) where the evidence lives.

**(b) Capture the reverted diff in the `root_write` record.** In
`guard_root_writes()`, before the checkout loop (loop.py:947), for each
`tracked` path run `git(["diff", "HEAD", "--", p], cwd=root)` and add the
concatenation to the record as a new field, e.g. `reverted_diff={p: diff}`.
Timing is the point: this is the last moment the destroyed content exists on
disk. The record call becomes:

```python
record("root_write", issue=tid, stage=tag, paths=changed,
       reverted=tracked, not_reverted=untracked,
       reverted_diff={p: diff_for(p) for p in tracked})
```

`record()` already serialises JSON (loop.py:211, 232); a diff is just a
string. Truncate per-path at a sane bound (e.g. 8 KB/path) — content over
that is still recoverable from the human's editor or reflog, and the journal
is read by the retrospective. This is test-safe per Q4.

**(c) Make the log line carry the recovery pointer.** The current line
(loop.py:925):

```python
log(f"    !! stray write(s) to ROOT during {tag} ({tid}): {changed}")
```

Already prominent (`!!`, stage, tid, paths). Extend it to name the journal
evidence and the hazard, e.g. `... : {changed} -- reverted to HEAD; diff in
events.jsonl (root_write); uncommitted local edits on these paths were
destroyed`. The `!!` level is the right one; no escalation needed.

**(d) Do NOT add a `guard_armed` event** (Q2) and do NOT weaken the revert
(Q3iii). The record-on-detection-only behavior is correct.

**(e) Optionally, second event.** If the diff is considered too heavy for the
`root_write` record (it is not — the event is ~never), a separate
`root_write_reverted_content` event with `issue`, `stage`, `paths`,
`diff` achieves the same attribution with no schema change to `root_write`.
Preferred: fold into `root_write`; one event, one lookup, and the incident
postmortem is a single JSON object.

## Q7. Can the LOOP avoid eating human edits without weakening the guard?

**Fundamentally: no, for same-file overlap.** The guard's only signal is a
content fingerprint delta across a stage (loop.py:818-819, 918-919); git
attributes no authorship, and a human edit and an agent stray in the same file
are the same bytes before the agent wrote. Every apparent escape fails:

- **"Refuse revert if the path was dirty at run start"** — already covered by
  Q3(iii): the fingerprint baseline *is* "dirty at start", and the guard must
  revert changes layered on it or #168's occurrence-2 shape ships disarmed
  (loop.py:879-882, 322-329 of the issue spec). Test 3120-3169 pins it.
- **Grace period** — the revert must land within the stage: `prompt_for()`
  re-reads ROOT's tracked prompt files on every subsequent invocation
  (loop.py:855-860), so a deferred revert is a prompt-poisoning window, not a
  courtesy.
- **Pause when a human is detected** — no human detector exists; the loop is
  unattended by design (README.md:6-11).
- **Prompt-files-only revert** — the poisoning motive is prompt files, but the
  guard's own scope is all tracked paths: a stray to `gate.sh`, `config.py`,
  or `gh.py` is equally fed to later stages (the docstring's "no gate, no
  reviewer, no commit, no journal entry" argument at 855-860 generalises).
  Narrowing the revert reopens the wider hole #168 closed.
- **Best-effort attribution** — already documented as deliberate
  (loop.py:884-889): under MAX_PARALLEL>1 either worker's guard may revert a
  shared stray and attribute it to the wrong `tid`. The revert is correct; the
  journal may name the wrong issue. A human victim should therefore not
  over-trust `issue=` — the README warning should say "the *paths* are
  reliable; the issue attribution is best-effort".

What the loop CAN do — and what this fix is — is make the destruction
(a) foreseeable (README), (b) attributable (the record already names paths,
stage, lane; it will name the diff), and (c) recoverable (the diff is the
recovery artifact: reverse-apply the agent's stray portion, which is the
delta between the record's before-fingerprint state and the diff). The human
still loses the edit; they no longer lose it silently.

## Affected tests

- **Breaking**: none, if the change is README + extra `reverted_diff` field +
  log-line text (Q4). `catches_stray_on_already_dirty_path` (3120) breaks only
  under the rejected Q3(iii) weakening.
- **Watch**: `reverts_tracked_stray` (3084-3094) and `restores_a_renamed_away...`
  (3369-3381) assert on the `root_write` record; they tolerate extra fields
  but any change to `paths`/`reverted` semantics breaks them.
- **If `git diff` is captured inside the guard**, note `_merge_lock`: the
  read-only captures deliberately stay OUTSIDE the lock (3571-3574); `git
  diff` is read-only and lock-safe, keep it outside.
- The guard tests monkeypatch `loop.git` (3387: `failing_checkout`) — the new
  `git diff` call must not assume rc==0 (capture `"<diff failed>"` on nonzero,
  mirroring the `<unreadable>` handling at 842-844).

## Risks

- **Journal size**: one diff per reverted path per event; bounded by how often
  the guard fires (2 events in ~6 weeks). Truncate per-path to bound it.
- **Diff of a staged stray**: `git diff HEAD -- <p>` covers index+worktree vs
  HEAD; the two-arg forms (`git diff -- <p>` vs `--cached`) do not. `HEAD --`
  is the correct one, matching the checkout's own `HEAD --` choice (862-866).
- **Rename-shaped strays**: `git diff HEAD -- <old>` on a deleted path
  produces the deletion diff — correct (that is the lost content).
- **README overpromise**: the warning must not claim human edits are
  *preserved*; it claims *evidence and recovery*, and says the same-file case
  is a hard limit.
- **No code currently consumes `root_write`** (grep: only loop.py's two
  `record()` sites); the retrospective reads the journal generically, so a new
  field is invisible to it until a retro pattern is written. The diff field is
  for humans.