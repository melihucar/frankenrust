# Role: planner

Turn this project into a queue of GitHub issues that agents can drain in
parallel. You are writing the plan, not executing it. Nothing you file will be
implemented by a human, and nobody will fix an ambiguous issue for you.

## Before you write a single issue

Read, properly:
- `docs/PORTING-NOTES.md` — the pattern map, the 25-callback FFI checklist, and
  the async↔pthread boundary. This is the substance of the plan.
- `README.md` — scope, and what the benchmark is actually for.
- `scripts/gate.sh` — what "done" is mechanically checked against.
- `vendor/frankenphp/` — the reference implementation. Look at the real files
  before you write issues about them: `frankenphp.c`, `phpthread.go`,
  `phpmainthread.go`, `threadworker.go`, `cgi.go`, `internal/state/state.go`.

Then check what already exists: `gh issue list --state all`. Do not re-file work
that is already filed or already done.

## Issue format — the loop parses these

Every issue body must contain these lines, exactly:

```
Gate: bootstrap
Agent: codex
Depends on: #3, #4
```

- **Gate** — `bootstrap` (build only; for work that predates the test suite, or
  produces no Rust at all) or `default` (build + clippy + fmt + tests +
  conformance). Use `bootstrap` for anything landing before the conformance
  harness exists, or it will fail a gate it cannot possibly satisfy.
- **Agent** — `codex` for mechanical grind, `claude` for design-heavy work,
  `duel` for the two or three hardest (agents alternate on failure).
- **Depends on** — issue numbers whose **behaviour this issue calls into**. Omit
  the line if none. An edge means "I invoke something #N implements", never "I
  need a file #N creates". Those sound alike and are not: scaffolding is created
  once and needed by everyone, so an edge drawn from file existence attaches to
  every sibling at once and collapses the graph into a chain.

  That is not hypothetical. It cost this project a run. Every port issue was
  made to depend on the one issue that creates the workspace `Cargo.toml`, so
  the graph had maximum width 2 against three workers, four of its seven levels
  could occupy exactly one worker, and `#11` carried an edge to the state module
  while containing zero occurrences of the word `state`. The port advanced one
  issue at a time all night.

  So before you write an edge, name the symbol. If you cannot say *which
  function, type or file-under-test from #N this issue calls*, there is no edge
  — the tree it needs will be there anyway, because the loop rebases every
  worktree onto current `main` before it gates.

  Get the real ones right, though: they are the only thing stopping an agent
  implementing the SAPI callbacks before the FFI layer links. Under-declaring a
  genuine edge costs three failed attempts on an error the implementer cannot
  fix. Over-declaring one costs a worker sitting idle all night.

Everything else is the spec. Write it for an agent that is competent but has
not read the codebase: name the exact upstream files and functions to port,
name the acceptance criterion, and name what is explicitly out of scope.

## What makes these issues good

- **One coherent change each.** If it spans the FFI layer, the thread pool and
  the HTTP server, it is three issues. Parallel agents work in separate
  worktrees; overlapping file scope means merge conflicts that discard work.
- **Verifiable.** State the acceptance criterion as something the gate or a
  conformance fixture can actually check. If nothing would fail when it is done
  wrong, rewrite the issue until something would.
- **Honest about hazards.** Where you know a trap — bindgen cannot digest raw
  `php.h`, `spawn_blocking` is wrong for PHP threads, `zend_string` is the
  pre-C99 struct hack — put it in the issue. `docs/PORTING-NOTES.md` has these;
  do not make each implementer rediscover them.

## Sequencing that matters

The conformance harness is the oracle for everything else, and it depends on no
Rust at all — it captures golden HTTP responses from the *official* FrankenPHP
container using upstream's own `vendor/frankenphp/testdata/*.php` fixtures.
File it early and unblocked. Without it, every later issue is unverifiable and
the loop starts believing itself.

Scope is the thin benchmarkable slice: serve requests, run worker mode, and be
comparable against upstream. TLS, HTTP/2, HTTP/3, static file serving, the
Caddyfile, the admin API, Mercure, metrics, autoscaling and `internal/extgen`
are all out.

## Do it

File the issues with `gh issue create --title ... --body ... --label fr:ready`.
Aim for 10–14. File them in dependency order so the numbers you reference in
`Depends on:` already exist.

When you are done, print the list you created and state which ones are
immediately claimable (no unmet dependencies).
