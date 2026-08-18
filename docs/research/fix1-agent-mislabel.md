# Fix 1: `gate_fail` / `empty_diff` journal events record the requested agent, not the agent that ran

Date: 2026-08-18
Branch: `support/opencode`
Scope: research only — no files modified.

## Question

The journal (`orchestrator/logs/events.jsonl`, written via `record()` in `orchestrator/loop.py`) logs `empty_diff` and `gate_fail` events with `agent=<requested agent>` — the rotation slot at `loop.py:1600` — rather than the agent that actually ran after a mid-run quota fallback (opencode → claude). The reviewers already solve this correctly (`loop.py:1340-1353`, `agents[idx] = use`). The unblocker for issue #14 was misled by this journal field; the retrospective (`orchestrator/prompts/retrospective.md`) reads the journal.

## TL;DR

- **5 mislabeled sites in `_work()`**: the implementer invoke (`loop.py:1609`) and the fixer invoke (`loop.py:1648`) both **discard** `invoke()`'s first return value, and three records then write the rotation slot: `gate_fail` (`1617`), `empty_diff` (`1630`), `empty_diff` post-fix (`1681-1682`). A fourth record, `merged` (`1702-1703`), has the same mislabel.
- **`review_block` (1645, 1663) carries no `agent` field at all** — nothing to mislabel, though adding one would be an optional improvement.
- `invoke()`'s fallback is fully internal (`loop.py:529-577`): the returned `use` is authoritative per call. The reviewer stage already relies on exactly this (`loop.py:1340, 1349-1352`).
- **Nothing depends on the current semantics.** The retrospective never reads the `agent` field of these records; `scripts/check_orchestrator.py` fixtures are agent-less and its only `empty_diff` assertion checks `phase` only; every `fake_invoke` test stub already returns the resolved agent as its first value.
- **Minimal fix**: capture the implementer invoke into `use` and the fixer invoke into `fuse`; change `agent=agent` → `agent=use` / `agent=fuse` at the three record sites. 5-line change, zero test changes.
- **No recovery-decision risk**: `recover_blocked()` (`loop.py:1446`) reads GitHub labels and issue counters, never the journal.

## Findings

### Q1. All `record(..., agent=...)` call sites in loop.py, classified

Complete list (grep of `agent=` inside `record()` calls):

| Site | Event | Field value | Classification |
|---|---|---|---|
| `loop.py:260` | `review_diversity_lost` | `agent=agent` — the agent being disabled | **Actual** (the disabled agent is the one being reported; correct) |
| `loop.py:555` | `agent_timeout` | `agent=use` — inside `invoke()`, `use` is the running agent | **Actual** (updated on fallback at `574`) |
| `loop.py:567` | `agent_error` | `agent=use` — inside `invoke()` | **Actual** |
| `loop.py:572` | `agent_fallback` | `agent=use` (the failed cheap agent), `to=claude` | **Actual** |
| `loop.py:1347` | `agent_error` | `agent=agent` — the **requested** reviewer | **Requested-only** (but defensible: `invoke()` raised, so no agent completed; see Risks) |
| `loop.py:1617` | `gate_fail` | `agent=agent` — rotation slot (`loop.py:1600`) | **Requested-only — MISLABELED** |
| `loop.py:1630` | `empty_diff` | `agent=agent` — rotation slot | **Requested-only — MISLABELED** |
| `loop.py:1681-1682` | `empty_diff` (post-fix) | `agent=agent` — rotation slot | **Requested-only — MISLABELED** (worse: the work was the *fixer's*, a separate invoke) |
| `loop.py:1702-1703` | `merged` | `agent=agent` — rotation slot | **Requested-only — MISLABELED** (same class; adjacent scope) |

`review_block` (`loop.py:1645-1646` initial, `1663-1664` post-fix) is **absent from this table**: it has **no `agent` field at all**. No mislabel, but also no agent info.

Under the current config (`orchestrator/config.py:58`, `DUEL_AGENTS = ["opencode"]`), every duel issue's rotation slot is `opencode` and `FALLBACK_AGENT` (`loop.py:509`) maps opencode→claude — so **every** `gate_fail`/`empty_diff` after a quota latch is attributed to opencode while claude ran the work. That is exactly the mislead that misdirected the #14 unblocker.

### Q2. Do the preceding `invoke()` calls capture or discard?

`invoke()` returns `(agent_used, rc, output)` (`loop.py:512-515`, `577`).

- **`loop.py:1609-1611` — implementer invoke, DISCARDED**:
  ```python
  with guard_root_writes(tid, f"impl.{attempt}"):
      invoke(agent, wt, prompt_for("implementer", issue, extra), logdir,
             f"impl.{attempt}", role="implementer", model=model,
             escalate=(attempt >= MAX_ATTEMPTS))
  ```
  Return value dropped entirely. This is the invoke whose agent the `gate_fail` (1617) and `empty_diff` (1630) records describe.
- **`loop.py:1648-1649` — fixer invoke, DISCARDED**:
  ```python
  with guard_root_writes(tid, f"fix.{attempt}"):
      invoke(agent, wt, prompt_for("fixer", issue, f"\n{blocking}\n"), logdir,
             f"fix.{attempt}", role="fixer", model=model)
  ```
  Return value dropped. This is the invoke whose agent the post-fix `empty_diff` (1681-1682) record describes.
- `loop.py:1579` (critic): **captured but unused** — `used, _, critique = invoke(...)`. Evidence the codebase already half-prefers capture; nothing records it.
- `loop.py:1420` (resolver), `1503` (unblocker), `1954` (retro): capture as `_, _, out` / `_, rc, text` — no journal record needs the agent, so discarding is fine there.
- `loop.py:1340` (reviewer): **captured and used** — `use, _, text = invoke(...)`, then `agents[idx] = use` (`1352`) with the comment at `1349-1351` explaining the actual-vs-requested distinction. This is the pattern the fix should mirror.

### Q3. Does anything depend on the current (mislabeled) semantics?

**No.**

- `orchestrator/prompts/retrospective.md` — the only consumer. It reads `gate_fail` `.tail` (`retrospective.md:17`) and event counts (`:16`); the events list (`:21-26`) names event types, never the `agent` field. No field-level dependency on `agent` for any event.
- `scripts/check_orchestrator.py` — the `gate_fail` fixtures at `1258`, `1543`, `1724`, `1844` are minimal agent-less records used to test `_next_retro_cycle()`/retro watermarks; no assertion reads an `agent` field anywhere in the file (grep for `.get("agent")`, `["agent"]`, `agent=` in record assertions: zero hits). The only `empty_diff` assertion (`2903-2904`) filters on `f.get("phase") == "post-fix"` only.
- `orchestrator/gh.py`, `config.py`, `replay.py` (if any): no reads of these records (grep of `gate_fail|empty_diff` repo-wide in `*.py` hits only loop.py and check_orchestrator.py).

### Q4. What would break if the fields carried the actual agent?

**Nothing found.**

- Tests: all `fake_invoke` stubs that drive `_work()`/`review_stage()` already return the resolved agent as the first tuple element and are 3-tuples: `check_orchestrator.py:1547-1556`, `1742-1748`, `1857-1863`, `1977-1979`, `2697-2707`, `2859-2874` (e.g. `2861`: `use = loop.resolve(agent, role, escalate)[0]` then `return use, 0, ...`). Capturing the first return value is therefore **test-compatible with zero test edits**.
- `check_post_fix_empty_diff_does_not_merge` (`2794-2912`) asserts `phase == "post-fix"` presence (`2903-2908`); an added `agent=` field does not disturb the filter.
- Retrospective: reads counts and `.tail`; a corrected `agent` field only adds signal.
- The `merged` record's `agent` field (`1702-1703`) is likewise unread anywhere.

### Q5. Minimal robust change

**Edit plan** (all in `orchestrator/loop.py`, `_work()`):

1. **`loop.py:1609-1611`** — capture the implementer invoke:
   ```python
   with guard_root_writes(tid, f"impl.{attempt}"):
       use, _, _ = invoke(agent, wt, prompt_for("implementer", issue, extra), logdir,
                          f"impl.{attempt}", role="implementer", model=model,
                          escalate=(attempt >= MAX_ATTEMPTS))
   ```
   (No `use` name collision: `_work()` body at 1566-1711 defines only `used` for the critic at 1579. `use` matches the reviewer convention at `1340`.)
2. **`loop.py:1617`** — `record("gate_fail", ..., agent=agent, ...)` → `agent=use`.
3. **`loop.py:1630`** — `record("empty_diff", ..., agent=agent)` → `agent=use`.
4. **`loop.py:1648-1649`** — capture the fixer invoke:
   ```python
   with guard_root_writes(tid, f"fix.{attempt}"):
       fuse, _, _ = invoke(agent, wt, prompt_for("fixer", issue, f"\n{blocking}\n"), logdir,
                           f"fix.{attempt}", role="fixer", model=model)
   ```
5. **`loop.py:1681-1682`** — `record("empty_diff", ..., agent=agent, phase="post-fix")` → `agent=fuse`.

Optional adjacent sites (same class, recommend including or filing separately):
6. **`loop.py:1702-1703`** — `merged` record: `agent=agent` → `agent=use`. Caveat: after a fix pass the merged content is implementer + fixer work; `use` names the implementer, which is the primary author. If full truth is wanted, a second field (`fixer=fuse`) could be added — but nothing reads it, so keep it minimal.
7. **`loop.py:1645, 1663`** — `review_block` has no agent; optionally add `agent=use` (implementer) to both records. Out of scope for the mislabel fix; adding fields is additive and risk-free.

**Authoritative `use` note**: the fallback happens entirely inside `invoke()`'s internal loop (`loop.py:529-576`): on quota failure `use, model = resolve(use, role, escalate)` (`574`) re-resolves and the loop `continue`s; the final `return use, rc, text` (`577`) returns the agent that actually ran. So the returned first value is authoritative for every call, including on the last attempt where `escalate=True` (`1609-1611`; `resolve()` at `289-292` may still yield opencode-escalated or claude). The reviewer path (`1340`, `1349-1352`) already treats it as authoritative, and its silent-retry round (`1394-1399`) re-invokes and overwrites `agents[idx]` per round — same per-call semantics.

### Q6. Does recording the actual agent change recovery decisions?

**No.** `recover_blocked()` (`loop.py:1446-1534`) reads only `gh.blocked_needing_recovery()` and `issue.recoveries` (`1472`, `1463`); it never opens the journal. The unblocker prompt (`1496-1501`) points at transcript files in `logdir`, not at `events.jsonl`. Journal reading in loop.py is confined to retrospective plumbing (`_next_retro_cycle`, watermark slicing; `loop.py:1906, 1921, 1931`) — none of which touches the `agent` field.

## Risks

1. **None found for the core 5-site change.** Verified: no consumer reads the `agent` field of `gate_fail`/`empty_diff`/`merged`; all test stubs are return-compatible; the retrospective prompt has no field dependency.
2. **`loop.py:1347` (`agent_error`, reviewer thread)** stays "requested" — and should: `invoke()` raised, so no agent completed; the requested name is the honest attribution, and the comment at `1342-1346` treats it as such.
3. **Naming collision**: introducing `use` in `_work()` is safe (verified: only `used` exists, at `1579`); introducing `fuse` likewise.
4. **Behavioral change is journal-only** — no control-flow or gate/merge decision reads these fields, so the change cannot alter which issues merge or block.
5. Optional sites 6-7 are additive; if skipped, the `merged` record remains the one lingering requested-agent field (mild inconsistency, unread).

## Sources

- `orchestrator/loop.py` — `record()` def `203-232`; `invoke()` `512-577` (fallback `570-575`, return `577`); `resolve()` `271-293`; `FALLBACK_AGENT` `509`; reviewer capture `1340-1353`; `_work()` rotation `1596-1603`; implementer invoke `1609-1611`; `gate_fail` `1617-1618`; `empty_diff` `1630`; fixer invoke `1648-1649`; `review_block` `1645-1646, 1663-1664`; post-fix `empty_diff` `1681-1682`; `merged` `1702-1703`; critic `used` `1579`; resolver `1420`; unblocker `1503`; retro `1954`; `recover_blocked()` `1446-1534`; reviewer-thread `agent_error` `1347`.
- `orchestrator/config.py` — `DUEL_AGENTS` `58`, `ROLE_AGENT` `27`, env overrides `125-128`.
- `orchestrator/prompts/retrospective.md` — journal evidence `12-26`; `gate_fail` `.tail` read `17`.
- `scripts/check_orchestrator.py` — `gate_fail` fixtures `1258, 1543, 1724, 1844`; `fake_invoke` stubs `1547, 1742, 1857, 1977, 2697, 2859`; `empty_diff` post-fix assertion `2903-2908`; `check_agent_routing` `991-1064` (resolve-table semantics, unchanged).
- Git: `bdf0cb4` (silent-reviewer retry; introduced `agents[idx] = use`), `987fb1d`/`8c982d7` (opencode duel config).