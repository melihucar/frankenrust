# Fix 2: claude can never be disabled, and recovery never stops when no agent is usable

Date: 2026-08-18
Branch: `support/opencode`
Scope: research only — no files modified.

## Question

On 2026-08-17 the opencode agent hit its quota at 21:23:40 (`orchestrator` unblock lane), got disabled for the run, fell back to claude — and claude was auth-broken ("Failed to authenticate: OAuth session expired and could not be refreshed", instant rc=1). Nothing disables a broken claude, and recovery never stops when no agent is usable. The loop marched ~26 blocked issues through `recover_blocked()` in ~35 seconds, creating/destroying a worktree per issue and recording 26 useless `recover_failed` events before the wallclock stopped it.

Evidence: `/var/folders/cp/_2r01d3d7ls4vtbx91vgxhjh0000gn/T/opencode/loop-run.log` (the 21:23:40 cascade), `orchestrator/logs/14/claude.impl.1.log` (auth failure text), `orchestrator/logs/events.jsonl` (25 `recover_failed` + 26 `agent_error` in the 21:23:40–21:24:15 window).

## TL;DR

- **Why claude can never be disabled, three independent reasons:** (1) the fallback branch in `invoke()` (`loop.py:570-575`) is gated on `use in FALLBACK_AGENT` and `FALLBACK_AGENT` (`loop.py:509`) maps only `{codex: claude, opencode: claude}` — claude has no entry, so the branch is skipped for claude; (2) `resolve()` returns claude unconditionally at `loop.py:293` (and at the escalate branch `289-292`) with no `_agent_ok("claude")` check, so even a manually-disabled claude (env `FR_CLAUDE_DISABLED=1`, which the `_disabled_agents` init at `151-154` *already* honours for every member of `AGENTS = ("claude","codex","opencode")` at `config.py:20`) would still be resolved and spawned; (3) claude's own quota text ("You've hit your weekly limit", `claude.impl.1.log:120`, `api_error_status:429`) *does* match `LIMIT_PATTERNS`' `"you've hit your"` (`loop.py:140`) — but the pattern branch is unreachable for claude for reason (1).
- **The retry loop at `loop.py:574` is NOT infinite** (`for _ in range(2)` at `loop.py:529` caps it), but with claude disabled each iteration re-runs claude because `resolve()` ignores the disabled state — worst case 2 wasted claude invocations per issue, 3 attempts per issue in `_work()`, and the whole blocked queue marched once per lane poll.
- **The auth failure is verbatim, stable CLI text** that lands in the *result event* of the claude NDJSON stream — `claude.impl.1.log:123` `"result":"Failed to authenticate: OAuth session expired and could not be refreshed"`, `"is_error":true`, `"terminal_reason":"api_error"`, and in the assistant event `claude.impl.1.log:122` with `"error":"authentication_failed"`. The same failure happened the previous day (`claude.unblock.0.log:162-166`, 08-15 16:29-16:30) — recurring, never latched. A one-pattern `AUTH_PATTERNS = ("OAuth session expired and could not be refreshed",)` matched against the *result-event text* (`_final_text`'s return) is tight: it cannot appear in agent-written HTTP/auth code the way "429"/"rate limit" can (the `loop.py:568-569` caution).
- **The journal is additionally useless for this class of failure today:** `_final_text`'s claude branch (`loop.py:469-472`) reports `err = subtype = "success"` for `is_error` results, so every claude auth failure was journalled as `agent_error ... reason="success"` (25 records) and logged as `rc=1 success` (`loop-run.log:170`). The real reason is in the `result` field, which is what `final` already holds.
- **Design (minimal, 4 touch-points):** (a) `_disable_agent`'s hardcoded "; falling back to claude" (`loop.py:255`) becomes conditional; a claude auth/quota branch in `invoke()` disables claude and `break`s; (b) `resolve()` raises a new `NoAgentUsable` when its terminal default (claude) is disabled, `invoke()` catches it pre-spawn and records `agent_unavailable`; (c) a `no_agent_usable()` guard at the top of `recover_blocked()` (`loop.py:1462`) bails with a new journal event, and `cmd_unblock` (`loop.py:2242`) breaks the lane on the same condition; (d) `cmd_run`'s claim loop (`loop.py:2091`) gets the same guard via a direct check — **it must NOT call `recover_blocked()`** (check_orchestrator.py:374-377 forbids it).
- **Test impact:** `check_agent_routing` (scripts/check_orchestrator.py:991-1064) is the only test that touches resolve's disable semantics — add claude-latch cases asserting the raise. `check_blocked_has_a_recovery_path` (301-387) pins that `cmd_run` never calls `recover_blocked()` — satisfied by a separate helper. `check_rolling_pool` (2002-2440) drives the real `cmd_run` with `_disabled_agents` empty — unaffected. `check_verdict_parsing` (837-988) is where claude auth-result parsing tests belong.
- **The "routing check" disable (question 5) is a test artifact:** `check_orchestrator.py:1043` calls `disable_{latch}("routing check")`, run inside every gate via `gate.sh:45`. It pollutes the main journal with `review_diversity_lost reason="routing check"` records (observed 08-17 12:58-12:59, 18:38-18:42, 20:52 and 08-18 06:56, 15:48 in the main `events.jsonl`) when the checker is run from the main checkout, and its "disabled for the rest of this run" line is false noise in gate transcripts. It cannot leak state into the lanes (subprocess env is process-local). The test should pin the latch directly (`loop._disabled_agents = {latch}`) instead of going through the side-effecting `disable_*`.

---

## Findings

### Q1. The disable/fallback machinery, precisely

| Piece | Location | Behaviour |
|---|---|---|
| `_disabled_agents` | `loop.py:151-154` | Set init'd from `FR_<AGENT>_DISABLED == "1"` env for **every member of `AGENTS`** (`config.py:20` = `("claude","codex","opencode")`). The env-persistence machinery already covers claude at import — only `resolve()`/`invoke()` ignore it. |
| `_agent_ok` | `loop.py:235-237` | `agent not in _disabled_agents`, under `_codex_lock`. |
| `codex_ok`/`opencode_ok` | `loop.py:240-245` | Thin wrappers. No `claude_ok` exists. |
| `_disable_agent` | `loop.py:248-260` | Idempotent set-add, sets `os.environ[FR_<AGENT>_DISABLED]="1"` (`254`, survives `restart_into_new_code`'s `os.execve` — prior art `retro-19.md:174`), **hardcoded log "…; falling back to claude" (`255`)**, `record("review_diversity_lost")` (`260`). |
| `resolve` | `loop.py:271-293` | duel unwrap (`278-282`); ValueError for unknown agent (`283-284`); codex/opencode returned only when `not escalate` and `*_ok()` (`285-288`); escalate branch returns opencode-escalated or `("claude", ESCALATED_MODEL)` (`289-292`); **terminal default `("claude", MODELS[role])` unconditionally (`293`)**. No disable check on claude anywhere. |
| `LIMIT_PATTERNS` | `loop.py:138-142` | `"usage limit", "rate limit", "rate_limit", "quota", "429", "too many requests", "insufficient_quota", "you've hit your", "resource_exhausted", "please try again later"`. |
| `_hit_limit` | `loop.py:501-505` | Reads the **last 4000 chars** of the log, lowercased, substring match. |
| `FALLBACK_AGENT` | `loop.py:509` | `{"codex": "claude", "opencode": "claude"}` — **no claude key**. |
| `invoke` fallback loop | `loop.py:529-576` | `for _ in range(2)` (`529`); `resolve` at `526`; quota branch `570-575`: `if use in FALLBACK_AGENT and rc != 0 and _hit_limit(logpath):` → `record("agent_fallback")`, `_disable_agent(use, "exit {rc} with a quota pattern")`, re-`resolve`, `continue`. Otherwise `break` (`576`). |

**When the fallback triggers:** only when the *requested* agent is codex or opencode (membership in `FALLBACK_AGENT`), it exited `rc != 0`, and a limit pattern appears in the log tail — per the `loop.py:568-569` caution ("429"/"rate limit" appear legitimately in the output of an agent writing an HTTP server), a pattern alone is not enough; a failure alone is not enough; both are required.

**Why claude failures can never disable anything:**
1. `use == "claude"` is never `in FALLBACK_AGENT` → the `570-575` branch is skipped for claude, so `_disable_agent` is never reached with `agent="claude"` from the quota path.
2. `resolve()`'s terminal default (`293`) and escalate default (`292`) return claude with no `_agent_ok` check — so even a claude already in `_disabled_agents` (say via `FR_CLAUDE_DISABLED=1` env at boot, `151-154`) is resolved and spawned anyway. The disabled state is honoured for codex/opencode only, and only via the `285-288` checks.
3. Claude's own quota text ("You've hit your weekly limit · resets Aug 20…", `claude.impl.1.log:120`, `api_error_status:429`, `terminal_reason:"api_error"`) matches `"you've hit your"` (`loop.py:140`) — but the pattern branch is unreachable for claude per (1).

### Q2. The actual claude auth failure in the transcripts

`orchestrator/logs/14/claude.impl.1.log` (run lane, 21:24:30Z):
- `:120` — first result event: `"is_error":true,"api_error_status":429,"result":"You've hit your weekly limit · resets Aug 20 at 4pm (Europe/Istanbul)","terminal_reason":"api_error"` — claude's **own weekly quota** (429), separate failure mode, also matches `LIMIT_PATTERNS`.
- `:122` — assistant event: `"content":[{"type":"text","text":"Failed to authenticate: OAuth session expired and could not be refreshed"}],"error":"authentication_failed"`.
- `:123` — final result event: `"is_error":true,"api_error_status":null,"result":"Failed to authenticate: OAuth session expired and could not be refreshed","terminal_reason":"api_error"`.

Same shapes in `claude.impl.2.log:6-7` (21:28:07), `claude.impl.3.log:6-7` (21:31:08), and `claude.unblock.0.log:162-166` — **timestamps 2026-08-15T16:29-16:30Z: the identical auth failure hit the day before**, on issue 14's unblock attempt, and was never latched then either. (That log also shows `invoke()`'s `logpath.open("ab")` append mode (`loop.py:537`) reusing one log file across attempts *and across runs* for the same issue/tag — the 08-15 failure text sat in the file the 08-17 opencode agent read while working issue 14.)

**Why `_final_text` reported `"success"`:** the claude branch (`loop.py:454-476`) sets `err = ev.get("subtype") or "error"` for `is_error` results (`469-472`) — `subtype` is `"success"` for these API-error result events, so the loop logged `!! claude unblock.0 failed: rc=1 success` (`loop-run.log:170`) and journalled 25 × `agent_error reason="success"`. The real reason is in the `result` field, which the same branch already captures into `final` (`468`).

**Proposed `AUTH_PATTERNS`** (tight, mirrors the `loop.py:568-569` caution in the other direction):
```python
AUTH_PATTERNS = (
    "failed to authenticate: oauth session expired and could not be refreshed",
    "you've hit your weekly limit",          # claude's own 429 quota, stable prefix
)
```
Matched **against the result-event text returned by `_final_text`** (`text`/`err` at `loop.py:561`), not the raw 4000-char tail: for claude the failure sentence is always the last result event's `result` field (`impl.1:123`), which `final` already holds. `"oauth session expired and could not be refreshed"` is a 37-char verbatim CLI sentence an agent writing HTTP/auth code cannot plausibly emit; `"you've hit your weekly limit"` is the stable prefix of the quota sentence (the date/locale suffix varies, so match the prefix, and it is already the semantics of `"you've hit your"` in `LIMIT_PATTERNS`). Matching the result event only — not the raw tail — keeps claude agents who write rate-limited HTTP servers in the transcript from tripping it.

### Q3. Design

**(a) Detect claude auth failure in `invoke()` and disable claude.**

In `invoke()` (`loop.py:512-577`), after the existing quota branch (`570-575`), before `break`:
```python
if use == "claude" and rc != 0 and _hit_auth(text, err):
    record("agent_disabled", agent="claude", tag=tag, rc=rc, reason=(err or text)[-300:])
    _disable_agent("claude", f"exit {rc} with an auth or quota failure")
    break
```
- `break`, **not** `continue`: claude has no fallback target; `continue` would re-run the disabled agent (see (b)).
- `_hit_auth(text, err)` is a new helper matching `AUTH_PATTERNS` against the already-extracted result text/err (zero extra file IO; strictly tighter than the raw-tail `_hit_limit`). It subsumes claude's own 429 weekly-limit (`impl.1:120`) via the `"you've hit your weekly limit"` pattern.
- Fix `_disable_agent`'s hardcoded log line (`loop.py:255`):
  ```python
  log(f"!! {agent} disabled for the rest of this run ({reason})"
      + ("" if agent == "claude" else "; falling back to claude"))
  ```
  (`review_diversity_lost` at `260` stays; it is agent-neutral.)
- Optional but recommended: improve the `_final_text` claude branch (`loop.py:469-472`) so `is_error` results with `subtype == "success"` use the `result` text as `err` — turns 25 × `reason="success"` into readable reasons and feeds (a) directly.

**(b) `resolve()` when claude is disabled.**

The retry loop is **not infinite**: `for _ in range(2)` (`loop.py:529`) caps at two iterations. But with claude disabled, `resolve(use, role, escalate)` at `574` re-returns claude (`293`), so a codex/opencode quota failure with a broken claude burns the fallback invocation too, then returns `(claude, rc=1, text)` — and every `_work()` attempt (`loop.py:1599-1611`) re-burns 3 times, plus the critic (`1579`), resolver (`1420`), fixer (`1648`) and reviewers (`1340`).

Make disablement authoritative at resolution:
- New exception `class NoAgentUsable(Exception)` near `loop.py:271`.
- In `resolve()`: at the escalate default (`289-292`) and the terminal default (`293`), before returning claude: `if not _agent_ok("claude"): raise NoAgentUsable(f"claude disabled; no agent usable")`. (`285-288` already short-circuit for codex/opencode; the raise only fires when the *only* reachable result is disabled.)
- In `invoke()`: guard the initial resolve (`526`) and the fallback re-resolve (`574`):
  ```python
  try:
      use, model = resolve(agent, role, escalate, model)
  except NoAgentUsable:
      record("agent_unavailable", agent=agent, tag=tag, reason="all agents disabled")
      return agent, 1, ""
  ```
  and inside the loop, wrap the `574` re-resolve the same way, `record("agent_fallback", agent=use, to=None, tag=tag, rc=rc)` and `break`.

All `resolve()` callers in the tree are `invoke()` (`526`, `574`) and test stubs (`check_orchestrator.py:580`, `1046`, `1055`, `2861`) — all in-process with empty `_disabled_agents` unless a test latches something, so the raise path is only reachable under the new latch cases.

**(c) `recover_blocked()` bails when no agent is usable.**

- New helper next to `_agent_ok` (`loop.py:235`):
  ```python
  def no_agent_usable() -> bool:
      return all(not _agent_ok(a) for a in AGENTS)
  ```
- `recover_blocked()` (`loop.py:1446-1536`): between `recovered = 0` (`1462`) and the `for` loop (`1463`):
  ```python
  if no_agent_usable():
      log("!! no agent usable; recovery cannot proceed")
      record("no_agent_usable", agent="none")
      return 0
  ```
  This alone stops the march for `cmd_unblock`, which calls `recover_blocked()` repeatedly (`loop.py:2249`, every 5-30s per `2255`).
- `cmd_unblock` (`loop.py:2226-2259`): additionally `break` out of the lane loop when `no_agent_usable()` (before `2249`) — the lane exits `rc=1` while blocked issues remain (`2259`), and `supervise.sh`'s crash-loop floor (`scripts/supervise.sh:71-74`) stops the supervisor for a human. A lane that keeps polling a queue it cannot work is the exact 30s×wallclock waste the cascade showed.

**(d) `cmd_run`'s claim loop needs the same guard — as a direct check, not via `recover_blocked()`.**
- `cmd_run` (`loop.py:2043-2223`), after the wallclock check (`2093-2104`) and before the claim budget (`2131`):
  ```python
  if no_agent_usable():
      log("!! no agent usable; stopping claims")
      record("no_agent_usable", agent="none")
      break
  ```
  In-flight futures still complete (the `with` block's `shutdown(wait=True)`), which is correct — a merge already earned is not abandoned.
- **Do not** implement this as `recover_blocked()` from `cmd_run`: `check_blocked_has_a_recovery_path` (scripts/check_orchestrator.py:374-377) fails if `cmd_run()` calls `recover_blocked()`.
- Lane note: `_disabled_agents` is per-process (`loop.py:151-154`), and the run/unblock lanes are separate processes (`supervise.sh:58`). Each lane independently discovers a wall and burns one invocation doing it — acceptable, it matches `invoke()`'s own "a quota wall should cost one wasted invocation" contract (`loop.py:518-520`). Sharing the latch across lanes (file or env from `supervise.sh`) is out of scope but worth a follow-up.

### Q4. What `check_orchestrator.py` already pins about disabled agents / fallback

- **`check_agent_routing` (991-1064)** — the only test of resolve's disable semantics. Routing table `1011-1038`; latch applied via `getattr(loop, f"disable_{latch}")("routing check")` at `1043`, reset at `1045/1053/1061`; `saved_ok` restore `1039, 1062-1063`; bogus-agent ValueError case `1055-1059`. Cases `1023-1026` pin codex/opencode→claude under latch. **Affected**: add cases asserting `NoAgentUsable` when claude is latched — (`"claude", "implementer", False, "claude", RAISE`) and the exhausted-chain case (`"codex", "implementer", False, "codex" + claude latched`, RAISE), mirroring the `1055-1059` pattern. Also affected: the test's side effect itself (see Q5).
- **`check_review_stage_retries_a_silent_reviewer` (500-714)** — `run_case` stub (`572-580`) returns `loop.resolve(agent, "reviewer")[0]`; the `latched=True` case (`649`) disables opencode only; claude stays usable. **Unaffected unless new claude-latch cases are added** (a stub that raises `NoAgentUsable` would be caught by `one()`'s `except` at `loop.py:1341` and retried as silent — actually well-behaved).
- **`check_verdict_parsing` (837-988)** — exercises `_final_text` for codex (`874-919`) and opencode (`925-972`) with quota transcripts; no claude result-event cases today. **Affected only as the natural home for new auth cases**: a claude log whose final result event carries `"Failed to authenticate: OAuth session expired and could not be refreshed"` must yield `err` containing that text (pins the `469-472` improvement) and no verdict.
- **`check_blocked_has_a_recovery_path` (301-387)** — pins `recover_blocked`/`cmd_unblock` existence (`321-323`), `MAX_RECOVERIES >= 1` (`327-328`), recovery order (`349-358`), and that **`cmd_run()` must not contain `recover_blocked()`** (`374-377`) while `cmd_unblock` must (`378-380`). **Affected**: the (d) guard must not call `recover_blocked()`; the new `no_agent_usable()` break in `cmd_unblock` must not break the `378-380` grep (it checks for `recover_blocked()` presence, not absence — fine). The early-bail in `recover_blocked` doesn't change `blocked_needing_recovery()` ordering assertions.
- **`check_no_absorbing_states` (272-298)** — label-level, unaffected.
- **`check_rolling_pool` (2002-2440) and `check_rolling_pool_abandons_safely` (2440+)** — drive the real `cmd_run` against fake `gh`/`work` in-process; `_disabled_agents` is empty in a clean import, so the `no_agent_usable()` break never fires. **Unaffected**, but the guard must sit before the claim budget without changing drain/wallclock semantics, and must not fire when only *some* agents are disabled.
- **`check_post_fix_empty_diff_does_not_merge` (2794-2899)** — `fake_invoke` at `2859-2874` calls `loop.resolve(agent, role, escalate)[0]`; claude usable → unaffected.
- **`check_config_defaults` (1075+) / `check_config_overrides` (1109+)** — subprocess with `_clean_env` (`1067-1072`, strips `FR_*`); unaffected.
- **All `check_retro_*` (1234-1905)** — stub or run `retrospective()`, which calls `invoke()` directly (`loop.py:1954`); an invoke-level no-agent bail produces `rc=1, text=""` → `record("retrospective", cycle, analysed=False)` (`loop.py:1966-1969`), a graceful path that already exists. Unaffected by the tests (they stub invoke).
- `main()` registration list: `scripts/check_orchestrator.py:3708-3720` — no change needed unless a new check is added.

### Q5. The "routing check" disable in gate subprocesses

Origin: **not the loop, not gate.sh directly** — it is `check_agent_routing` (scripts/check_orchestrator.py:1043) calling `disable_{latch}("routing check")`, and that checker is what every gate runs as the `orchestrator-runnable` step (`scripts/gate.sh:45`, every profile). Three real effects:

1. **Journal pollution when the checker runs in the main checkout.** `_disable_agent` (`loop.py:260`) records `review_diversity_lost reason="routing check"` — the main `orchestrator/logs/events.jsonl` carries 2 such records per main-checkout checker run (observed 08-17 12:58-12:59, 18:38-18:42, 20:52; 08-18 06:56, 15:48), plus one `review_diversity_lost reason="test wall"` (12:56). The retrospective reads the journal (`prompts/retrospective.md:12-26`) and counts `review_diversity_lost` as cross-model review lost — test noise masquerading as run evidence.
2. **No state leaks into the lanes.** The gate is a subprocess of the agent's worktree shell; `os.environ["FR_OPENCODE_DISABLED"]="1"` (`loop.py:254`) is process-local and dies with the checker. The `finally` (`1045/1053/1061`) restores `_disabled_agents` but not the env — irrelevant across processes. The `saved_ok` restore (`1062-1063`) covers the function refs.
3. **False "disabled for the rest of this run" noise in gate transcripts.** The log line (`loop.py:255`) lands in gate output, which agents echo into their own transcripts when they run the gate themselves (`prompts/implementer.md:47`). `_hit_limit` scans those transcripts (`loop.py:504`) — the line contains no `LIMIT_PATTERNS` substring, so no false latch, but it reads as a real run event.

Fix recommendation: `check_agent_routing` should pin the latch without the side effects — replace `getattr(loop, f"disable_{latch}")("routing check")` (`1043`) with `loop._disabled_agents = {latch}` (as `check_review_stage_retries_a_silent_reviewer`'s `run_case` already pins `codex_ok`/`opencode_ok` lambdas at `563-584`), and restore the env vars in the `finally` for belt-and-braces. This also stops the routing test from being the thing that would set `FR_CLAUDE_DISABLED=1` in gate subprocesses once claude is disableable.

### Q6. What the retrospective reads from the polluted journal

`orchestrator/prompts/retrospective.md`:
- Event list at `:21-26` includes `recover_failed`, `agent_fallback`, `agent_error`, `review_diversity_lost` — no "no agent usable" concept.
- `:28-33`: `recovery_exhausted` is called the most serious event; `:35-40`: `work_crash`/`agent_error`/`low_disk` are "loop failing rather than an agent failing" and must be accounted for before diagnosing issues.
- The cascade's 25 `recover_failed` + 26 `agent_error reason="success"` (`events.jsonl`, 21:23:40–21:24:15) read as 25 distinct per-issue failures — the retrospective's instruction to "count first, then read only the distinct ones" (`:42-44`) would surface 25 identical `reason="success"` records and, without the real reason text (see Q2's `_final_text` gap), cannot attribute them to one auth failure. The only truthful signals in the window: `agent_fallback opencode→claude` (1 record) and `review_diversity_lost opencode/quota` (1 record).
- A new journal event (e.g. `no_agent_usable`) plus the `reason` fix for `agent_error` would make this class countable as one systemic event; the retrospective event list (`:21-26`) should gain it.

## Proposed fix design — exact touch-points

1. `orchestrator/loop.py` — add `AUTH_PATTERNS` beside `LIMIT_PATTERNS` (`138-142`).
2. `orchestrator/loop.py` — add `_hit_auth(text, err)` beside `_hit_limit` (`501-505`); match against result-event text, not raw tail.
3. `orchestrator/loop.py` — `_disable_agent` (`248-260`): conditional "; falling back to claude" (`255`).
4. `orchestrator/loop.py` — `resolve` (`271-293`): raise `NoAgentUsable` when the terminal default claude is disabled (`289-293`).
5. `orchestrator/loop.py` — `invoke` (`512-577`): guard initial resolve (`526`) and fallback re-resolve (`574`); add the claude auth branch before `break`; record `agent_unavailable`.
6. `orchestrator/loop.py` — `_final_text` claude branch (`466-472`): `err` from `result` text when `is_error` (readable journal reasons).
7. `orchestrator/loop.py` — new `no_agent_usable()` near `_agent_ok` (`235`); guard `recover_blocked()` (`1462-1463`), `cmd_unblock` (`2242-2249`), `cmd_run` claim loop (`2093-2131`).
8. `scripts/check_orchestrator.py` — `check_agent_routing` (`1043`): pin `_disabled_agents` directly; restore env in `finally`; add claude-latch cases asserting `NoAgentUsable`.
9. `scripts/check_orchestrator.py` — `check_verdict_parsing` (`837-988`): add claude auth-result event cases.
10. `orchestrator/prompts/retrospective.md` — add `no_agent_usable` to the event list (`:21-26`).

## Affected tests

- **`check_agent_routing`** (991-1064) — must change (latch mechanism + new claude cases).
- **`check_blocked_has_a_recovery_path`** (301-387) — must NOT break: guard `cmd_run` without `recover_blocked()`.
- **`check_review_stage_retries_a_silent_reviewer`** (500-714) — unaffected as-is; safe if extended (raise → silent-reviewer path is already handled at `loop.py:1341`).
- **`check_verdict_parsing`** (837-988) — unaffected as-is; the home for new auth cases.
- **`check_rolling_pool` / `check_rolling_pool_abandons_safely`** (2002-2440+) — unaffected (empty `_disabled_agents`); verify the `cmd_run` guard doesn't alter drain semantics.
- **`check_config_defaults` / `check_config_overrides`** (1075-1167) — unaffected (`_clean_env` subprocesses).
- **All `check_retro_*`** — unaffected (graceful `analysed=False` path already exists).

## Risks

- **False auth latch on legitimate agent output**: mitigated by matching only the result-event text and requiring `rc != 0`; the primary pattern is a verbatim CLI sentence. The `"you've hit your weekly limit"` prefix is stable across the date/locale suffix.
- **`NoAgentUsable` reaching callers that don't expect it**: only `invoke()` and test stubs call `resolve()`; all `invoke()` callers already tolerate a failed invoke (empty text → "no decision" paths at `loop.py:1427-1443`, `1506-1529`, `1590-1591`, retrospective `1964-1969`). The reviewer path (`loop.py:1338-1353`) retries a silent reviewer exactly once — bounded.
- **A lane break in `cmd_unblock` looks like a crash to `supervise.sh`**: exiting `rc=1` with stuck issues under the 60s floor (`supervise.sh:71-74`) stops the supervisor for a human — intended, but the lane's `FR_WALLCLOCK` restart loop must not re-march. The `no_agent_usable()` break is deterministic, so a restart re-exits immediately; the crash-loop floor catches it.
- **Per-lane disable state divergence**: each lane burns one invocation per wall before latching. Accepted (matches `loop.py:518-520`'s contract); cross-lane latch sharing is a follow-up.
- **`_final_text` err change** (`subtype="success"` → result text) alters `agent_error` journal `reason` values; the retrospective's "count first" flow (`retrospective.md:42-44`) is the consumer — the change makes counts *distinct* (good), but existing issue-filing patterns keyed on `reason="success"` are not known to exist.