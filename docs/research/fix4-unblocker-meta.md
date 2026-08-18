# Research: let the unblocker file `fr:meta` issues against the loop itself

Date: 2026-08-18. Branch: support/opencode. Research only — no files modified.

Context: recovering #14, the unblocker diagnosed two defects *in the loop
machinery* (the journal records the requested agent, not the actual one; the
claude fallback is auth-broken and cascades) but its only decision outcomes are
SPLIT/REQUEUE/CLOSE, so it could only write a comment. This research answers
whether the unblocker may instead file an `fr:meta` issue ("Improvement to the
loop itself", gh.py:67) and what the prompt change must say.

---

## Q1. How do agents currently write to GitHub? What gh auth do they have?

**TL;DR**: `gh` is available to every agent through `agent_env()` (PATH
extension) and inherits the supervisor process's environment, so agents are
authenticated as the same GitHub user the loop runs as (the repo owner). Three
prompts already tell agents to `gh issue create`; none mentions `fr:meta`.

### Instructions already in prompts

- **unblocker.md:53-59** (SPLIT) — exact words: *"**Do it yourself** before
  emitting the line: `gh issue create` the new pieces with valid `Gate:`,
  `Agent:` and `Depends on:` lines, then narrow this issue's body to whichever
  piece remains."* No flags are spelled out; the body-line contract is
  delegated to the format below. **unblocker.md:62-64** (REQUEUE) — *"rewrite
  the body first with `gh issue edit`."* The unblocker already performs GitHub
  writes before emitting its decision line; filing an issue is a smaller step
  than the SPLIT surgery it already does.
- **planner.md:95** — `gh issue create --title ... --body ... --label fr:ready`.
- **shared.md:49** — `gh issue create --label fr:ready,fr:followup,fr:p2`,
  with the parsed-body template at shared.md:53-59
  (`Gate: bootstrap | default | bench` / `Agent: codex | claude | opencode |
  duel` / `Depends on: #12, #13`). shared.md is prepended to **every** prompt,
  including the unblocker's (prompt_for, loop.py:580-587), so the unblocker
  already has the body contract in context.
- **retrospective.md:73** — `gh issue create --label fr:ready,fr:meta,fr:p2`
  — the one existing place that files `fr:meta`. This is the command the
  unblocker should be pointed at, verbatim.
- The loop's own writer, `gh.create()` (gh.py:477-483), uses exactly
  `--title` / `--body` / `--label <comma-joined>`.

### Agent environment (agent_env(), loop.py:479-498)

`agent_env()` copies `os.environ` and adds exactly three things:
`FRANKENRUST_AGENT=1`, `OPENCODE_DISABLE_AUTOUPDATE=1`, and a PATH extension
with `~/.cargo/bin`, `/opt/homebrew/bin`, `/usr/local/bin` (loop.py:488-497).
**It does not set `GH_TOKEN` or anything GitHub-specific.** Authentication is
whatever the supervising process inherited: the operator's interactive `gh`
login (`~/.config/gh/hosts.yml` + keyring) or an exported `GH_TOKEN`. `gh auth
status` in this checkout shows the repo owner account (melihucar) with `repo`
scope. Neither `scripts/supervise.sh` nor `scripts/dev.sh` sets a token, so the
loop and every agent it spawns run `gh` as the same user — agents can create,
edit and label issues exactly as the loop can. (Confirmation of the general
mechanism: agents already do `gh issue create` under SPLIT, and the unblocker
on #14 ran `gh issue edit` and label moves with no auth failure — the failures
it hit were claude's *OAuth session*, not gh's.)

---

## Q2. How does an issue get its `fr:meta` label? What fields does the loop require?

**TL;DR**: `fr:meta` is created by `ensure_labels()`; `gh issue create -l
fr:meta` works directly because both lanes ensure labels at start. The loop
parses three body lines (`Gate:`, `Agent:`, `Depends on:`) with anchored
regexes; all three are optional in the *parser* but two silently default
(`default` gate, `opencode` agent), which is exactly what a loop-defect issue
must not inherit.

- `fr:meta` is in `LABELS` at gh.py:67 (`("d4c5f9", "Improvement to the loop
  itself")`) and created by `ensure_labels()` (gh.py:187-189, `--force`).
  Called at run-lane start (loop.py:2044) **and** unblock-lane start
  (loop.py:2239), so the label always exists before any unblocker runs —
  `gh issue create --label fr:meta` cannot fail on a missing label, and
  GitHub's create API would silently ignore an unknown label anyway.
- The parser the loop applies to any issue it picks up:
  - `Depends on:` — `DEP_LINE_RE = re.compile(r"^[ \t]*[-*]?[ \t]*depends on\b", re.I | re.M)` and `DEP_RE = re.compile(r"^[ \t]*[-*]?[ \t]*depends on\b:?(.*)$", re.I | re.M)` (gh.py:97-98); consumed by `Issue.deps` (gh.py:133-144). A line that **opens** with "depends on" is metadata; the same words mid-sentence are prose (the #56 lesson, gh.py:80-96). A `Depends on:` line that yields no `#N` warns and reads as *unblocked* (gh.py:136-143).
  - `Gate:` — `re.search(r"^\s*gate:\s*(\w+)", body, re.I|re.M)` (gh.py:149-150), default `"default"`.
  - `Agent:` — `re.search(r"^\s*agent:\s*(\w+)", body, re.I|re.M)` (gh.py:154-158), default `"opencode"`.
  - `Revisions:` / `Recoveries:` counters are also read (gh.py:160-184) but not required on a fresh issue.
- A missing `Gate:` silently inherits `default` (build + fmt + clippy + tests +
  conformance) — a docs/orchestrator-only change then fails a gate it cannot
  satisfy, three times, and lands in `fr:blocked` (shared.md:63-68 measured
  this on 15 and 19 open issues). So the prompt must pin `Gate: bootstrap`
  (shared.md:72-74: "docs, prompts, scripts, orchestrator changes").

---

## Q3. What happens to an agent-filed `fr:meta` issue once filed? Can it deadlock the queue?

**TL;DR**: it is claimed and implemented like any other `fr:ready` issue — the
loop has no special fr:meta handling except a *downward* tiebreak in
`rank_key()`. It cannot deadlock if it carries no `Depends on:` line; it can
effectively deadlock if it depends on the still-blocked issue it came from.

- **Claiming**: the run lane claims straight from `gh.claimable()` (loop.py:2155-2161), which is every `fr:ready` issue with all deps closed (gh.py:298-307). The planner is irrelevant to claiming — the planner only seeds the queue. The `fr:meta` label's only scheduling effect is the `housekeeping` term in `rank_key()` (gh.py:246), which **breaks ties downward** (port work first), and the gh.py:62-67 docstring: "Filed by the retrospective against the loop itself, and claimable like any other issue — there is no human to promote it."
- **Implementation**: the issue's own `Gate:`/`Agent:` lines route it; with `Gate: bootstrap` an orchestrator-only fix passes the gate, merges, and — because a merge touching `loop.py`/`gh.py` changes `_source_fingerprint()` (loop.py:1997-2001) — the loop detects `self_update_pending()` and `restart_into_new_code()` re-executes into the new code at the next batch boundary (loop.py:2020-2040, gh.py:62-67). The loop repairs itself. This is exactly the "loop fixes itself" property the plan wants.
- **Deadlock analysis** (this is the one real hazard):
  - No `Depends on:` line → claimable immediately. Safe.
  - `Depends on: #14` (the blocked issue being recovered) → `claimable()` waits for #14 to close (gh.py:299). Normally the unblock lane resolves #14 and the meta issue becomes claimable — a delay, not a deadlock. But if #14's recovery budget is spent (`issue.recoveries >= MAX_RECOVERIES`, loop.py:1472-1482 — budget is 2, loop.py:129), #14 is terminal, and a meta issue depending on it is `fr:waiting` forever — the exact absorbing-state failure mode this repo is organized against (gh.py:19-28). **The prompt must forbid a `Depends on:` line** (or allow one only onto already-closed issues).
  - A malformed `Depends on: none` parses to zero deps + a warning (gh.py:136-143) — harmless but noisy; the template should just omit the line.
  - `Gate: default` on an orchestrator-only change → 3 failed attempts → `fr:blocked` → the unblocker recovers *its own meta issue*, and a loop-defect issue failing on the loop's own gate is the correct failure but a waste; `Gate: bootstrap` avoids it.

---

## Q4. Would a dupe-filed `fr:meta` issue be harmful? Is there any dedup?

**TL;DR**: dupes are a measured, live problem — #122 exists specifically
because two identical issues were filed 3m42s apart and "nothing checks whether
an issue is already filed." Nothing in gh.py/loop.py compares titles. The
guardrail must therefore be an explicit prompt instruction, and the model for
it already exists (retrospective.md:118-124).

- **No dedup anywhere in code**: gh.py has no title-matching logic (grep of
  gh.py/loop.py for title comparison/dedup: nothing); `fetch()` (gh.py:192-208)
  just lists by label. The only anti-duplication mechanism in the system is the
  retrospective prompt's "Do not file the same issue twice" section
  (retrospective.md:118-124): *"Before filing anything: `gh issue list --state
  all --label fr:meta`. If the problem is already filed, say so and file
  nothing."*
- **The dupe failure is documented and recent**: #122 ("orchestrator/prompts:
  nothing checks whether an issue is already filed — #120 and #121 are the same
  defect 3m42s apart, and four open pairs collide on one function") — the
  queue already contains duplicate meta issues; a queue full of them "starves
  the actual port" (retrospective.md:122-124).
- **Concrete relevance to #14**: the second defect the unblocker diagnosed
  (the quota latch / fallback cascade) is *already filed* as #126 ("the codex
  quota latch is permanent, survives restart, and arms on one substring"),
  OPEN, unimplemented. Had the unblocker filed it again, it would have been a
  dupe. The first defect (journal records requested agent, loop.py:1617/1630
  record `agent=` while invoke's actual `use` is discarded at loop.py:1609)
  is *not* filed — no fr:meta title mentions empty_diff/gate_fail agent
  attribution — so today there is exactly one genuinely new meta issue waiting
  to be filed from #14's recovery. This pair of facts is the whole argument
  for making the dedup check an explicit, quoted command in the prompt.
- **Guardrails the prompt should state** (all evidenced from this repo's own
  failures): file only when the defect is evidenced from **this issue's
  transcripts** (`orchestrator/logs/14/` — the prompt already tells the
  unblocker to read them, unblocker.md:21-23); never for scope/spec problems
  (those are SPLIT and the resolver's job); run the `gh issue list --state all
  --label fr:meta` check and if a near-match exists, reference it instead;
  one defect per issue; end with a real RECOVERY line regardless.

---

## Q5. What does check_orchestrator.py verify about prompts? Which checks touch unblocker.md?

**TL;DR**: three checks touch `unblocker.md`, none asserts its content beyond
existence; **no test asserts the RECOVERY lines or the prompt's structure**.
A content change to unblocker.md requires no check updates.

- **check_blocked_has_a_recovery_path** (check_orchestrator.py:303-340): only
  that the file exists (319-320), that `loop.recover_blocked`/`loop.cmd_unblock`
  and `gh.blocked_needing_recovery`/`gh.unblock` exist, `MAX_RECOVERIES >= 1`,
  and a behavioural test of `blocked_needing_recovery()` ordering. No prompt
  text asserted.
- **check_implementer_transcript_path_resolves_from_a_worktree**
  (check_orchestrator.py:3630-3679): mentions unblocker.md only in a comment
  (3643-3648) explaining why the implementer check is needed; the assertions
  are about implementer.md. The unblocker's `logs/<N>` path is injected as an
  absolute path by the loop (loop.py:1499-1501), so no path check applies.
- **check_root_dirty_set...** (check_orchestrator.py:3416-3434): uses
  unblocker.md as a *fixture control file* in a temp repo (`write_text(
  "unblocker\n")`) — the content is irrelevant, only that a plain tracked file
  exists. Not affected.
- **check_verdict_parsing** (check_orchestrator.py:837-912) asserts
  `reviewer.md` quotes "VERDICT: BLOCK" and `critic.md` "VERDICT: REVISE" —
  it does not touch unblocker.md, and no counterpart asserts "RECOVERY:". A
  grep for `RECOVERY` across scripts/ finds only loop.py:1506/1512/1513.
- **check_filing_contract_is_stated** (check_orchestrator.py:808-834) asserts
  shared.md's fenced body template only. **check_prompts** (61-70) checks that
  a prompt file exists for every role; **check_parses** (34-45) parses only
  .py sources; **check_runs** (48-58) runs `loop.py status`.
- One structural note: the RECOVERY tokens are parsed from the **final
  message** only (`_final_text`, loop.py:379-405; verdict-echo lesson at
  check_orchestrator.py:837-855 and loop.py:383-397), so quoting
  "RECOVERY: REQUEUE" inside unblocker.md's instructions is safe — the prompt
  is not part of the parse. The one hazard would be adding a *new* token
  prefixed "RECOVERY:" that the parser does not recognize — see Q7.

---

## Q6. Should the meta issue be linked to the blocked issue (#N in the body)?

**TL;DR**: yes — cite the source issue in prose (and the transcript paths), but
never as a `Depends on:` line. Prose mention is provably safe (anchored
parser); the existing fr:meta convention is exactly this shape.

- **Safety**: `Issue.deps` only matches lines *opening* with "depends on"
  (DEP_LINE_RE, gh.py:97-98; anchored per the #56 fix, gh.py:80-96), so
  "Found while recovering #14" in prose adds no dependency edge. The
  #56-history is the cautionary tale in the other direction: a sentence
  containing "depends on" *did* create edges until the anchor was added.
- **Existing convention** (measured from live issues, `gh issue list --label
  fr:meta`): bodies open with the two metadata lines, then attribute the
  finding: #206: *"Found by retrospective 38 (`orchestrator/logs/retro-38.md`,
  Finding 1)."*; #122: *"Retrospective 4 (`orchestrator/logs/retro-4.md`,
  Finding 1)."*; #198: *"Found by retrospective 34
  (`orchestrator/logs/retro-34.md`, Finding 2)."* Meta issues also
  pre-emptively disclaim neighbours: #206: *"**This is not #188** ... #44 ...
  #62 ... #45."* — the dedup convention made explicit. Titles are
  `orchestrator: ...`, `prompts: ...`, `gate: ...`, `queue: ...` prefixes with
  a measured-defect statement. For an unblocker-filed issue the analogue is
  "Found while recovering #14 (`orchestrator/logs/14/`,
  `opencode.unblock.1`)." Note the transcripts live in the main checkout's
  `orchestrator/logs/<N>/`, which the unblocker prompt already points at
  (unblocker.md:21-23, loop.py:1499-1501).
- Bodies also carry a `## Evidence` section with counts and quoted errors,
  `## What to change` with file:line, an acceptance criterion, and "Out of
  scope" (see #202's body for the full shape) — the prompt should tell the
  unblocker to write this shape, not free-form prose.

---

## Q7. Where in unblocker.md should the new outcome go? Does the parser need changes?

**TL;DR**: the filing is an *action*, not a fourth decision. It belongs (a) as
a fifth diagnosis bullet under "Diagnose before you decide", and (b) as a
"Do it" paragraph in "Decide" before the decision lines. The parser
(loop.py:1506-1522) needs **zero changes** — it only greps the three existing
tokens, and a meta-filing unblocker still ends with one of them.

- Parser, exactly (loop.py:1506-1522):
  ```python
  if "RECOVERY: CLOSE" in out:            # 1506 → gh.close + comment(out[-30000:])
  if "RECOVERY: SPLIT" in out or "RECOVERY: REQUEUE" in out:   # 1512
      decision = "split" if "RECOVERY: SPLIT" in out else "requeue"
      if gh.unblock(issue.number, issue.recoveries + 1):       # 1514
          gh.comment(..., out[-30000:])                        # 1515-1516
  # else: "unblocker gave no decision" → stays blocked (1524-1529)
  ```
  An unblocker that files a meta issue and ends `RECOVERY: REQUEUE` hits
  line 1512, `gh.unblock()` re-reads the body it may have rewritten and stamps
  `Recoveries: N` (gh.py:378-402), and the posted comment carries the last
  30,000 chars of the final message — which will include the meta issue number
  and its evidence. Nothing else consumes the output.
- **Do not introduce a fourth "RECOVERY: ..." token.** The parser's else-branch
  (1524-1529) treats any unrecognized outcome as *no decision* — the issue
  stays `fr:blocked`, recreating the absorbing state the unblocker exists to
  end (gh.py:19-28, loop.py:1524-1526). If the prompt ever instructs a
  "RECOVERY: META" line, it must be reworked into the parser; the design below
  deliberately avoids that.
- **Placement**: the file's flow is Diagnose (28-47) → Decide (49-68) → Bias
  (70-86). The fifth diagnosis bullet slots after "**The environment is
  wrong.**" (unblocker.md:40-42) and before "**The work is genuinely
  hard...**" (43-45); the action paragraph slots after the SPLIT entry
  (53-59), because SPLIT already establishes the "do the GitHub writes first,
  then emit the line" pattern the meta filing reuses. No change to Bias needed
  beyond one sentence (see design below).

---

## Recommended prompt change (anchored to unblocker.md's existing style)

The file's voice: second person, evidence-first, concrete commands in code
spans, a reason for every rule. The added text follows that.

### 1. Diagnosis bullet — after line 42 (the "environment is wrong" bullet)

```markdown
- **The loop itself is wrong.** Nothing about this issue failed it: the
  machinery did — the journal recorded an agent that never ran, a fallback
  routed work to a dead auth, a counter or a label the loop maintains was
  wrong. The issue will fail identically next round until the loop is fixed,
  and nobody but you can file that. Name the defect with the log line that
  proves it.
```

### 2. Action paragraph — after line 59 (end of the SPLIT entry)

```markdown
**If the diagnosis is the last one, file the loop defect before you decide.**
`gh issue create` an `fr:meta` issue, then end with `RECOVERY: REQUEUE`
(the issue is fine; what failed it is not). Body:

```
Gate: bootstrap
Agent: opencode
```

then `## The defect`, the log line that proves it, and where it lives —
`orchestrator/logs/<N>/` in the main checkout. Label it
`gh issue create --title "orchestrator: <the defect>" --body ... --label
fr:ready,fr:meta,fr:p2` — never add a `Depends on:` line (an edge to this
issue strands the fix behind it). First run `gh issue list --state all
--label fr:meta`; if the defect is already filed, name it in your comment and
file nothing. One defect per issue, and only one you saw fail here — a
defect you suspect but cannot quote from this issue's transcripts is not a
defect.
```

(When laid out, the fenced block is the exact Gate/Agent template from
shared.md:53-59, which is already in the unblocker's context; the prompt
restates it so the copy-paste surface is local.)

### 3. One Bias sentence — after line 68 (after the CLOSE entry) or in the
CLOSE paragraph

```markdown
Filing the loop defect is not an outcome: this issue still needs one of the
three lines above. The meta issue is how the next round fails differently;
the decision line is what lets it.
```

### Why these choices

- **`Gate: bootstrap`** — orchestrator/prompts changes produce no Rust; the
  `default` gate would fail them (shared.md:63-68) and recycle the meta issue
  through fr:blocked. Matches every existing fr:meta body (measured: #122,
  #126, #198, #202, #206 all open `Gate: bootstrap`).
- **`Agent: opencode`** — the all-opencode default (shared.md:75-76,
  config.py:27-36); the retrospective's `Agent: claude` bodies are a
  different stage's convention and, while claude is auth-broken, a claude
  issue burns three attempts and parks (exactly #14's round 1). `opencode` is
  the honest default today.
- **`fr:p2`** — the default band (shared.md:90-91); a loop defect the
  unblocker files should not outrank the port without the retrospective's
  promotion evidence rules (retrospective.md:85-97).
- **No new token** — keeps loop.py untouched; the parser tolerates the
  filing because the decision line is still REQUEUE (loop.py:1512-1520).
- **"orchestrator: " title prefix** — matches the 59-issue fr:meta corpus
  convention (all titles measured start with a subsystem prefix).

---

## Affected tests

- **None fail.** No check asserts unblocker.md content beyond existence
  (Q5). `check_verdict_parsing` pins reviewer/critic tokens only;
  `check_filing_contract_is_stated` pins shared.md only. RECOVERY lines are
  asserted nowhere.
- Optional (not required) hardening a future PR could add: a
  `check_unblocker_outcomes` mirroring `check_verdict_parsing` that asserts
  unblocker.md still names exactly the three RECOVERY tokens and that the
  new action paragraph does not introduce a fourth "RECOVERY:" token — the
  loop's "no decision" branch (loop.py:1524-1529) is the absorbing state the
  prompt text could accidentally reintroduce.

## Risks

1. **Dupe filing** (Q4): mitigated by the quoted `gh issue list --state all
   --label fr:meta` command — same mechanism the retrospective uses, and #122
   proves the mechanism is load-bearing, not decorative. Residual risk: title
   similarity is fuzzy; the prompt says "if the defect is already filed" and
   #206's "This is not #188..." convention shows agents handle near-misses
   well when told to.
2. **A `Depends on:` edge to the recovering issue** would strand the fix
   behind the very block it describes — and behind a terminal block (recovery
   budget spent, loop.py:1472-1482) it is a permanent fr:waiting. Forbidden
   explicitly; the template omits the line.
3. **The meta issue claims a worker the port could have had.** `rank_key()`
   already sorts housekeeping below port work at equal priority (gh.py:246),
   so a p2 meta issue takes a worker only when nothing better is claimable —
   the designed behaviour since #23.
4. **Self-fixing latency**: a merged meta issue restarts the loop into the
   new code at the next batch boundary (loop.py:2020-2040), which is fast but
   not instant; an issue currently blocked on the defect will re-fail once
   before the fix lands. Acceptable — the unblocker's REQUEUE already says
   "say what changed".
5. **Echo hazard** (pre-existing, unchanged): the RECOVERY parse reads the
   final message only (loop.py:379-405); quoting "RECOVERY: REQUEUE" in the
   prompt is safe unless an agent's *last message* quotes the prompt. The
   "no decision" branch (1524-1529) still catches a genuine absence; the
   false-positive direction (echo → REQUEUE) exists today and is not made
   worse by this change.

## Answers at a glance

| Question | Answer |
|---|---|
| Q1 agents write via | `gh` in PATH via agent_env() (loop.py:488-497); auth inherited from supervisor = repo owner; unblocker.md:54 already has them creating issues |
| Q2 fr:meta label | ensure_labels (gh.py:187-189) at both lane starts (loop.py:2044, 2239); `-l fr:meta` works; body needs `Gate:`/`Agent:`/`Depends on:` (gh.py:97-98, 149-158); defaults are `default`/`opencode` — pin `Gate: bootstrap` |
| Q3 post-filing | claimed/implemented like any fr:ready issue (loop.py:2155, gh.py:246 housekeeping tiebreak); merge of loop.py/gh.py triggers restart_into_new_code (loop.py:2007-2040); deadlock only via a `Depends on:` edge to the blocked issue — forbid it |
| Q4 dupes | no code dedup anywhere; #122 is the live dupe failure; retrospective.md:118-124 is the model guardrail; #14's fallback defect is already #126 — dedup check is not hypothetical |
| Q5 tests | 3 checks touch unblocker.md, none asserts content; RECOVERY lines asserted nowhere; no check updates needed |
| Q6 linking | cite `#N` + transcript paths in prose — safe (anchored parser, gh.py:97-98); never as `Depends on:`; mirrors "Found by retrospective N (logs/...)" convention |
| Q7 placement/parser | new diagnosis bullet after unblocker.md:42; action paragraph after :59; one Bias sentence; parser at loop.py:1506-1522 needs no change — filing + REQUEUE hits the existing branch |