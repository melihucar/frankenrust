# Fix 3: Per-role unblocker timeout (FR_UNBLOCKER_TIMEOUT)

Date: 2026-08-18. Research only; no files were modified. Discovery agent output
for the implementation plan of: give the unblocker role its own (shorter) agent
timeout so one slow session cannot swallow the recovery lane's wallclock budget.

---

## Q1. Trace the timeout plumbing exactly

`AGENT_TIMEOUT` has exactly 12 references in `orchestrator/loop.py` (plus
documentation in README.md:106, scripts/check_orchestrator.py:507/3230, and
orchestrator/prompts/retrospective.md:24,65).

| Line | Use | Inside `invoke()`? |
|---|---|---|
| 101 | Definition: `AGENT_TIMEOUT = int(os.environ.get("FR_AGENT_TIMEOUT", str(60 * 60)))` | — |
| 552 | Kill condition in heartbeat loop: `if waited >= AGENT_TIMEOUT: proc.kill()` | **yes** |
| 554 | Log `!! {use} timed out after {AGENT_TIMEOUT}s` | **yes** |
| 555-556 | `record("agent_timeout", agent=use, tag=tag, seconds=AGENT_TIMEOUT)` | **yes** |
| 559-560 | Heartbeat log `.. {tag} running Xm (limit {AGENT_TIMEOUT // 60}m, {kb}KB)` | **yes** |
| 1144 | `review_summary()`: "every reviewer was killed by the {AGENT_TIMEOUT//60}-minute timeout..." | no |
| 1147 | `review_summary()`: "one adversarial review ... the other reviewer was killed by the {AGENT_TIMEOUT//60}-minute timeout..." | no |
| 1292 | Docstring in `review_stage()`: "killed by AGENT_TIMEOUT, crashed, hit a quota wall" | no (comment) |
| 2217 | `retro.join(timeout=AGENT_TIMEOUT)` in `cmd_run` finalisation | no |

All four *behavioral* uses inside `invoke()` are at 552-560 and would become
uses of a local `timeout` parameter. The external uses (1144, 1147, 2217)
describe **reviewer** kills and the retro thread — both correctly stay on the
global `AGENT_TIMEOUT`; they must NOT be switched to the unblocker value.

The `record("agent_timeout", ...)` event is read by the retrospective
(orchestrator/prompts/retrospective.md:24,65), which already handles the event
generically ("issues too large, or the timeout too low") — no prompt change
needed; `seconds=` already carries the actual value.

## Q2. `invoke()` signature and call-site compatibility

Signature (loop.py:512-514):

```python
def invoke(agent: str, wt: Path, prompt: str, logdir: Path, tag: str,
           role: str = "implementer", escalate: bool = False,
           model: str | None = None) -> tuple[str, int, str]:
```

Adding `timeout: int = AGENT_TIMEOUT` as a trailing keyword parameter touches
nothing else. All 8 call sites pass exactly 5 positional args and use keywords
for the rest — a defaulted trailing param breaks none of them:

| Line | Caller | Args after `tag` |
|---|---|---|
| 1340 | `review_stage()` (`one()` thread) | `role="reviewer"` |
| 1420 | `resolve_question()` | `role="resolver"` |
| 1503 | `recover_blocked()` — **the target** | `role="unblocker"` |
| 1579 | `work()` critic | `role="critic"` |
| 1609 | `work()` implementer | `role="implementer", model=model, escalate=...` |
| 1648 | `work()` fixer | `role="fixer", model=model` |
| 1720 | `cmd_seed()` planner | `role="planner"` |
| 1954 | `retrospective()` | `role="critic"` |

No caller passes positional args after `model`. The only change needed at the
call site is `role="unblocker"` → `role="unblocker", timeout=UNBLOCKER_TIMEOUT`
at 1503.

Test stubs mirror the current signature in scripts/check_orchestrator.py:572,
1547, 1742, 1857, 2697, 2859 — all accept `role/escalate/model` keywords and
none of them drive `recover_blocked()` (see Q5), so none break.

## Q3. Default value for the unblocker timeout — evidence

Measured unblock-session durations from `orchestrator/logs/loop.log` heartbeat
lines and `events.jsonl` (span from `~~ recovering #N` to the next mark):

**08-17 run (lane wallclock 8h, opencode free model):** 14, 16, 12, 7, 13, 9,
5, 4, 11, 8, 3, 6, 2, 22 min — median ~8 min, max **22 min** (#35,
20:52→21:14).

**08-13/14/15 runs (claude):** 11, 13, 5, 11, 10.5, 6.5, 7, 7, 10, 4.5, 6 min
(#11, #8, #79, #20, #10, #39, #40, #13 r0, #13 r1, #135, #168) — max 13 min.

**The incident (08-18, FR_WALLCLOCK=1800):** `#14 unblock.1` started
15:49:11, heartbeat until 16:34:11 (44m54s), lane log:
`!! unblock wallclock limit reached, stopping` at 16:34:06 and
`unblock lane end: recovered=1 still-blocked=[27 issues]`
(loop.log:655467-655483). The session ran 15 minutes past the 30-minute lane
budget and the lane recovered exactly ONE issue; 27 remained blocked. The
wallclock check runs only *between* `recover_blocked()` calls (loop.py:2243)
and `recover_blocked()` breaks only before *starting* new recoveries
(loop.py:1470-1471) — the in-flight session is never killed, by design, "its
worktree is crash-preserved either way" (loop.py:1468-1469).

**Recommended default: 25 minutes** (`str(25 * 60)`).
Evidence: longest observed successful unblock is 22 min (#35, 08-17); median
~8-9 min. 25 min = ~3x median and ~1.1x the historical max, while fitting
inside a 30-minute lane budget so the next blocked issue can at least be
*started*. The plan brief's stated range ("most unblocks take 12-22 min") is
confirmed by the 08-17 run (12-22 min band; median of that run ~8.5 min).

**Why a kill is cheap (mitigates the risk of a too-aggressive cap):**
- `recover_blocked()`'s `finally: retire_worktree(tid)` (loop.py:1534-1535)
  runs even on a timeout; `retire_worktree` (loop.py:643-660) commits any
  uncommitted work to `issue/<tid>` and frees the worktree — nothing lost.
- A timeout returns `(use, 124, "")` (loop.py:557); empty output hits the
  "no decision" path (loop.py:1527-1529), which does NOT increment
  `issue.recoveries` and does NOT touch the fr:blocked label — the issue is
  retried next lane run with its MAX_RECOVERIES budget intact.
- `MAX_RECOVERIES=2` (loop.py:129) is a per-issue lifetime budget, untouched
  by timeouts.

## Q4. Per-role knob vs per-invoke parameter — and the fixer

Recommended: **both, one layer each.**
1. A module knob `UNBLOCKER_TIMEOUT = int(os.environ.get("FR_UNBLOCKER_TIMEOUT",
   str(25 * 60)))` next to the other knobs (loop.py:98-107). Matches the
   existing `FR_<ROLE>` naming convention (README.md:111-113: `FR_AGENT_<ROLE>`,
   `FR_MODEL_<ROLE>`, `FR_OPENCODE_MODEL_<ROLE>`).
2. A generic `timeout: int = AGENT_TIMEOUT` parameter on `invoke()`, so any
   future role (or duel rotation) can override without new plumbing. The
   unblocker call site passes `timeout=UNBLOCKER_TIMEOUT`; every other call
   site keeps the default and is untouched.

Other roles: **no evidence any of them needs its own timeout.**
- **Fixer (1648):** runs in the claim lane (`cmd_run`, 8h default wallclock,
  parallel workers) — a fixer overrun stalls only its own worker, and a
  60-min cap is far above observed fixer work. No wallclock-budget
  interaction to fix.
- **Reviewers:** silence is already handled — killed reviewers are retried
  once (loop.py:1292-1298) and disclosed in the merge comment via
  `review_summary()` (loop.py:1142-1148). The 60-min reviewer timeout is the
  *disclosure text*, not a budget problem.
- **Resolver:** adjudication like the unblocker, but runs in the claim lane
  with no lane-budget interaction; no overrun evidence.
- The unblock lane is uniquely sensitive because it is *serial and
  single-threaded* (one recovery at a time, cmd_unblock loop.py:2242-2255) and
  its budget is typically 30-60 min — one 45-min session wastes the lane.

## Q5. check_orchestrator.py pins

Searched `TIMEOUT|agent_timeout|heartbeat|FR_AGENT` across scripts/:

- **No assertion pins `AGENT_TIMEOUT`'s value or behavior.** All matches are
  docstrings/comments: check_orchestrator.py:507 ("60-minute AGENT_TIMEOUT
  (loop.py:440)" — stale line ref, prose only), :3230 ("With AGENT_TIMEOUT=3600,
  a merge landing ... is the common case" — prose), :1079-1157 (FR_AGENT_<ROLE>
  env handling — unrelated role-selection knobs), :3574 (GATE_TIMEOUT prose).
- `check_blocked_has_a_recovery_path()` (check_orchestrator.py:301-383) pins
  recovery structurally: unblocker.md prompt exists (:319), `recover_blocked`/
  `cmd_unblock`/`gh.unblock` exist (:321-326), `MAX_RECOVERIES >= 1` (:327),
  recovery order by dependants (:353-358), `cmd_run` must NOT call
  `recover_blocked()` and `cmd_unblock` must (:374-380), `"unblock": cmd_unblock`
  dispatch (:381). None of these are affected by a timeout parameter.
- No test drives `recover_blocked()` with a stubbed `invoke` (the only stub
  signatures: 572, 1547, 1742, 1857, 2697, 2859 — all drive review/resolve/
  retro/work paths). None would receive the new `timeout=` kwarg.
- The one place a future stub could break: if a new test exercises
  `recover_blocked()` with a stub that lacks `timeout=`, it TypeErrors. Stub
  signatures should be updated to include it when written.

**Affected tests: none.** No test needs modification. (Optional hygiene:
refresh the stale `loop.py:440` reference in the :507 docstring.)

## Q6. Interactions

- **Heartbeat loop (547-560):** unchanged mechanics. `started = time.time()`
  (545) is inside the `for _ in range(2)` quota-retry loop, so the timeout is
  per-attempt, not cumulative across a claude fallback — same semantics apply
  to `timeout=`. The heartbeat log line (559-560) prints
  `(limit {AGENT_TIMEOUT // 60}m, ...)`; with a per-invoke timeout this must
  print the *local* value or a capped unblock reads as "limit 60m" while dying
  at 25m — operator-misleading. Kill/log/record at 552-556 likewise switch to
  the local `timeout`.
- **`_start`/`WALLCLOCK_LIMIT`:** `_start` (loop.py:158) is module-global,
  process-lifetime. `cmd_unblock` checks `time.time() - _start > WALLCLOCK_LIMIT`
  at 2243 (between `recover_blocked()` calls) and `recover_blocked()` at
  1470 (before each new recovery). The unblocker timeout is the *only* thing
  that can bound the in-flight session — the wallclock deliberately cannot
  (loop.py:1466-1469). This is exactly the hole the 45-min #14 session fell
  into: nothing bounds a single session except AGENT_TIMEOUT (60m) and the
  lane budget (30m) — so the session ran to 45m and the lane died with
  recovered=1. A 25-min cap changes the arithmetic: session A dies at 25m
  (budget intact, issue still blocked, no recoveries burned), session B
  *starts* within the 30m budget; the lane recovers 2 issues per run in the
  typical case (one ~12-22m session + one partial) instead of 1.
- **Quota retry in invoke (529-576):** unchanged. A killed unblock returns
  rc=124 with empty text; the "no decision" branch (1527-1529) is the
  absorbing-state guard and already handles this shape (it is the same shape
  a crash produces).
- **retro.join(timeout=AGENT_TIMEOUT) (2217):** unrelated; stays global.

## Recommended design (exact shape)

1. **loop.py ~102** (after `AGENT_TIMEOUT`):
   `UNBLOCKER_TIMEOUT = int(os.environ.get("FR_UNBLOCKER_TIMEOUT", str(25 * 60)))`
2. **invoke() signature (512-514):** add trailing
   `timeout: int = AGENT_TIMEOUT` parameter.
3. **invoke() body:** use `timeout` at 552, 554, 555-556, 560 (kill check,
   kill log, `record(..., seconds=timeout)`, heartbeat limit line). All four
   are inside invoke(); no other line changes.
4. **Call site 1503** (recover_blocked): pass `timeout=UNBLOCKER_TIMEOUT`.
   All other 7 call sites unchanged (default applies).
5. **README.md:106** knob list: add `FR_UNBLOCKER_TIMEOUT` (25m).

### Why not alternatives
- *Lower the global AGENT_TIMEOUT*: would kill legitimate implementer/
  reviewer/planner sessions elsewhere; reviewers' timeout disclosure text
  (1144/1147) and retro.join (2217) would silently change meaning. The
  implementer is the role that *should* get an hour.
- *Timeout only in cmd_unblock's wallclock check*: doesn't bound the session
  (can't kill a subprocess from inside `recover_blocked`'s loop — the kill
  machinery lives in invoke's heartbeat).
- *Clamp `min(timeout, WALLCLOCK_LIMIT)` in invoke*: couples two unrelated
  knobs; WALLCLOCK varies per lane while UNBLOCKER_TIMEOUT is role-scoped.

## Risks

1. **Killing a legitimate long unblock:** bounded by the evidence (max
   observed 22m; cap 25m) and by the safety net — a killed unblock preserves
   the worktree (retire_worktree, loop.py:1534-1535, 643-660), leaves the
   issue fr:blocked with its MAX_RECOVERIES budget untouched (1527-1529), and
   is retried next lane run. Worst case: one wasted 25-min session.
2. **Test stubs:** six invoke stubs in check_orchestrator.py (572, 1547, 1742,
   1857, 2697, 2859) mirror the old signature; none currently break (no test
   drives recover_blocked), but any *future* test driving `recover_blocked()`
   with a stub must include `timeout=`. Also the `check_blocked_has_a_recovery_path`
   source-text checks (374-381) are insensitive to the change.
3. **Misleading heartbeat limit:** if 559-560 keeps printing
   `AGENT_TIMEOUT // 60`, operators see "limit 60m" during a 25m-capped
   session. Must use the local value.
4. **record("agent_timeout", seconds=...) semantics:** the retrospective
   (prompts/retrospective.md:24,65) reads this generically; `seconds=1500`
   is self-describing. No prompt change.
5. **Config drift:** FR_UNBLOCKER_TIMEOUT > FR_AGENT_TIMEOUT is meaningless
   (kill already happens at the smaller value... actually the local value
   wins — an operator setting 3600 for the unblocker just restores today's
   behavior). No clamping needed; document the default only.

## Surprises found

- The 08-18 45-min session **completed and recovered #14** (recovered=1) — it
  was not a hang, just a very slow session; the wallclock did its *designed*
  thing (never interrupt an in-flight recovery) and the lane still died at
  16:34:06 with 27 issues left. The fix bounds the session, not the lane.
- The 08-17 run's sessions (opencode free model, the current default) cluster
  at 2-22 min with median ~8 — the "12-22 min" intuition overstates the
  middle; 25m is ~3x the median, comfortably generous.
- `record("agent_timeout")` and the retro prompt already carry `seconds=`, so
  the new cap produces journal evidence with zero prompt changes.
- check_orchestrator.py:507 cites "loop.py:440" for AGENT_TIMEOUT — already
  stale (it is loop.py:101/552); a pre-existing doc drift unrelated to this
  fix, worth fixing while touching the file.