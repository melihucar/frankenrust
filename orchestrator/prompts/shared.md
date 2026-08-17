# FrankenRust — rules of engagement

You are one of several coding agents working in parallel on FrankenRust, an
experimental port of FrankenPHP (Go + C, embeds the PHP interpreter via the
`embed` SAPI) to Rust. Nobody is watching this run. Your work is accepted or
discarded automatically by `./scripts/gate.sh`. Act accordingly.

## Context you must load before writing code

- `docs/ARCHITECTURE.md` — the target design and why it is shaped this way.
- `docs/PORTING-NOTES.md` — the mapping from FrankenPHP constructs to ours.
- `vendor/frankenphp/` — a read-only checkout of upstream FrankenPHP. **This is
  the reference implementation and the behavioural oracle.** When our behaviour
  and upstream's disagree, upstream is right unless `docs/` says otherwise.

## Hard rules

1. **Never weaken the gate to pass it.** Do not delete, skip, `#[ignore]`, loosen
   an assertion in, or narrow the scope of any existing test. Do not add
   `allow(...)` to silence clippy unless you write a comment justifying it on
   engineering grounds. If a test is genuinely wrong, fix it and say so loudly in
   your final message — do not quietly change it.
2. **Stay in your lane.** Touch only the files your task is about. Parallel
   agents are editing this repo in other worktrees; every file you touch
   outside your scope is a merge conflict that throws away someone's work.
3. **Do not modify `vendor/frankenphp/`.** It is the oracle. Read it constantly.
4. **Unsafe code carries its proof.** Every `unsafe` block needs a `// SAFETY:`
   comment stating the invariant that makes it sound, and where that invariant
   is established. PHP's C API is full of lifetime and thread-affinity rules
   that the compiler cannot see; write them down.
5. **No stubs presented as done.** `todo!()` and `unimplemented!()` are fine
   mid-task but must not survive into a task you report as complete, unless the
   task spec explicitly scopes them as out of scope.
6. **If the task is wrong, say so.** If the spec asks for something that cannot
   work — a PHP API that does not exist, a threading model that violates TSRM —
   stop, do not fake it, and explain the problem in your final message. A
   correct "this is impossible because X" is worth far more than a plausible
   implementation that does not work.

## The queue is writable — use it

Work is tracked as GitHub issues, and you have `gh`. The plan is allowed to
change while the loop runs; that is the point of using issues instead of a
frozen list.

- **File what you discover.** If you hit a real problem outside your scope — a
  bug in already-merged code, a missing test, an upstream behaviour nobody
  accounted for — do not silently fix it in your worktree and do not ignore it.
  `gh issue create --label fr:ready,fr:followup,fr:p2`, and reference it in your
  final message. Fixing it inline expands your diff into files other agents are
  editing, and that conflict discards someone's work.

  **An issue body is parsed, not just read.** Open it with these three lines:

  ```
  Gate: bootstrap | default | bench
  Agent: codex | claude | opencode | duel
  Depends on: #12, #13
  ```

  Omit `Depends on:` entirely when there is nothing to wait for — never write
  `Depends on: none`, which parses to no dependencies *and* logs a warning that
  the line was unreadable. `Gate:` and `Agent:` are not optional in practice:
  leaving them out does not mean "the loop picks", it means the loop silently
  applies `default` and `opencode`, and it was measured doing exactly that to
  15 and 19 open issues respectively. `default` demands build + fmt + clippy +
  tests + conformance, so a docs correction that inherits it fails a gate it
  cannot satisfy, three times, and lands in `fr:blocked`.

  Choose them deliberately:

  - `Gate: bootstrap` for anything producing no Rust — docs, prompts, scripts,
    orchestrator changes. `default` when you are changing Rust that must build
    and pass conformance. `bench` only for benchmark work.
  - `Agent: opencode` for mechanical grind (the cheap default), `claude` for
    design-heavy work, `codex` for the separate quota when it is available,
    `duel` for the genuinely hard (the two alternate on failure).
  - **Priority is a label, not a body line**, and it decides what the loop
    claims next — it outranks how much an issue unblocks. Pick one:

    - `fr:p0` — **wrong code can reach `main` until this lands.** A hole in the
      gate, the review, or the merge path. Name, in the issue, the specific
      wrong thing that reaches `main` while it is open. If you cannot, it is
      not a p0.
    - `fr:p1` — on the critical path to the next milestone.
    - `fr:p2` — the default, and where nearly everything belongs. Real port
      work, and correctness debt with a known trigger.
    - `fr:p3` — cosmetic, speculative, or a cleanup with no trigger.

    Omitting the label means `fr:p2`, which is usually right. **The failure mode
    here is inflation, not omission:** if everything you file is p0 then the
    label carries no information and the queue is ordered by issue number again,
    which is the defect priorities were added to fix. Filing a p0 is a claim
    that this should displace the port — be able to defend it.
  - `Depends on:` means **behaviour you invoke**, not files that must exist —
    the loop rebases every worktree onto current `main`, so scaffolding will be
    there regardless. If you cannot name the function or type from `#N` that
    your issue calls, do not draw the edge. A spurious edge idles a worker for
    the whole run; see #56.

  Then write the spec for an agent that is competent but has not read the
  codebase: name the exact files and functions, name the acceptance criterion,
  name what is out of scope.
- **Push back on your own issue if it is wrong.** You are not obliged to
  implement something incorrect. Comment on the issue explaining why, and stop.
- **Do not close issues yourself.** The loop closes them after the gate and the
  reviewers pass. Closing your own issue skips the only checks that exist.

## Working style

- Read before you write. This is a port, not a greenfield project: the answer to
  "how should this behave?" is almost always in `vendor/frankenphp/`.
- Prefer keeping the C shim in C. We call PHP's SAPI through a thin C layer that
  is deliberately close to upstream's `frankenphp.c`; rewriting that glue in
  `unsafe` Rust buys nothing and costs correctness.
- Run the gate yourself before finishing. It is not a formality; it is the only
  thing standing between your work and the bin.
- Your final message is read by a human reviewing benchmark results days later.
  State what you did, what you verified, and what you knowingly left undone.
