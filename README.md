# FrankenRust

An experiment: port [FrankenPHP](https://github.com/php/frankenphp) — a PHP app
server that embeds the interpreter rather than shelling out to FPM — from Go to
Rust, and find out whether it actually matters.

The deliverable is not the port. The deliverable is **a benchmark honest enough
to change your mind either way**, including the outcome where it turns out the
Go layer was never the bottleneck.

## What is actually being ported

Less than it sounds. Upstream is three layers with very different portability:

| Layer | Upstream | Here |
|---|---|---|
| PHP SAPI glue (`frankenphp.c`, 1,961 lines of C) | C | **kept, compiled as-is** |
| Thread orchestration, request plumbing (~5,300 lines Go) | Go | ported to Rust |
| HTTP server, TLS, HTTP/3, config | Caddy | hyper, HTTP/1.1 only |

The C file is already language-neutral — it just calls symbols that happen to be
named `go_*`. Rewriting it in `unsafe` Rust would buy zero performance and cost
enormous correctness, so we compile it and reimplement the **25 callbacks** it
calls into. That list is the porting checklist, in `docs/PORTING-NOTES.md`.

This mirrors how [rav1d](https://www.memorysafety.org/blog/rav1d-performance-optimization/)
ported dav1d: keep the proven hot core, replace the language around it.

## What to expect from the numbers

Be skeptical of a big win. PHP interpreter time dominates almost every realistic
workload, so the server layer is often noise. Rust's structural advantages here
are narrow and specific:

- **No cgo crossing cost.** Upstream crosses the Go↔C boundary several times per
  request; ours is a direct call.
- **No GC.** Should show up in tail latency, not mean throughput.

So the benchmark is built to isolate server overhead rather than measure PHP:
`noop` (PHP does nothing) is where a difference should appear, and `compute`
(CPU-bound PHP) is a **control that must show a tie** — a gap there means the
harness is broken, not the server. Results report p50/p95/p99/p99.9, not just
RPS, because that is where a runtime change actually shows up.

For calibration: rav1d's Rust port started ~11% *slower* than the C original and
took two years of funded work to get under 6%.

## Running the loop

The loop runs coding agents in parallel git worktrees against
`orchestrator/backlog.json`. Work is merged only if it passes `scripts/gate.sh`,
which agents are explicitly forbidden to weaken.

```sh
python3 orchestrator/loop.py run       # drain the backlog
python3 orchestrator/loop.py status    # what is done / running / blocked
python3 orchestrator/loop.py reset <task-id>   # unblock and retry
```

Knobs: `FR_PARALLEL` (default 3), `FR_ATTEMPTS` (3), `FR_WALLCLOCK` (14h),
`FR_AGENT_TIMEOUT` (1h per attempt).

**You have to start it yourself.** The agents run with permission prompts
disabled — that is what "unattended" requires — and authorizing a multi-hour
agent fleet with that much latitude is a decision for you to make, not something
to be started on your behalf. Logs land in `orchestrator/logs/<task-id>/`.

### How a task is processed

```
worktree ──► implementer agent ──► gate ──┬─ fail ─► retry with the failure output (≤3)
                                          │
                                          └─ pass ─► 2 adversarial reviewers (claude + codex,
                                                     independent contexts, only see the diff)
                                                        │
                                                        ├─ BLOCK ─► fixer agent ─► re-gate ─► re-review
                                                        └─ PASS  ─► rebase, re-gate, merge to main
```

The adversarial review stage is lifted from
[Bun's Zig→Rust rewrite](https://bun.com/blog/bun-in-rust), which used
implementer → two independent reviewers → fixer and caught a libuv double-free
and two other critical bugs that a green test suite had missed. The reviewers
are deliberately cross-model: two instances of the same model reviewing one diff
behave closer to one reviewer than to two.

## Benchmarking

```sh
bench/harness/run.sh              # full head-to-head, writes bench/results/<ts>/REPORT.md
bench/harness/run.sh --smoke      # fast sanity check
```

Both servers run as `linux/arm64` containers with identical CPU/memory limits;
load is generated from a **third container on the same docker network**, because
on Docker Desktop for Mac host→container traffic crosses the VM's userspace
network proxy and adds more latency variance than the effect being measured.
`oha --latency-correction` is used rather than `wrk` because `wrk` is closed-loop
and silently deletes tail latency under saturation (coordinated omission).

The harness **refuses to run** if other containers are competing for CPU. Set
`BENCH_ALLOW_NOISY=1` to override; the contending containers get recorded in
`CONTAMINATION.txt` next to the results either way.

> These are relative numbers. Docker Desktop on Apple Silicon runs everything in
> a shared Linux VM; the tax applies equally to both servers so the comparison
> is fair, but the absolute RPS figures do not transfer to a Linux deployment.

## Layout

```
crates/frankenrust-sys      raw FFI: bindgen over PHP headers, compiles upstream's C
crates/frankenrust-core     safe layer: threads, request context, the 25 callbacks
crates/frankenrust-server   hyper HTTP/1.1 server, async↔pthread bridge
vendor/frankenphp/          upstream, READ-ONLY. the behavioural oracle
tests/conformance/          differential tests vs upstream's own PHP fixtures
docs/PORTING-NOTES.md       the pattern map every agent reads first
```

`vendor/frankenphp/testdata/` is the reason this is verifiable at all: 73 PHP
fixtures that are implementation-agnostic, so the same script served by any
correct server must produce the same HTTP response. Upstream's `_executor.php`
even runs each fixture in both regular and worker mode. That corpus is our
oracle, the same way Bun's TypeScript test suite could validate a Rust rewrite
of a Zig codebase.
