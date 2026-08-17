# FrankenRust

**This is an experiment about agentic loops. Porting FrankenPHP to Rust is the
workload, not the goal.**

The question is whether a loop of LLM agents — planning, implementing,
reviewing, and repairing itself, with **no human in the loop** — can sustain
real engineering work overnight. Not "can an agent write a function", which is
settled, but: does the *system* hold up over hours, once the easy tasks are
gone, when nobody is awake to notice it has started doing something stupid?

That question needs a workload with three properties, and porting
[FrankenPHP](https://github.com/php/frankenphp) — a PHP app server that embeds
the interpreter rather than shelling out to FPM — has all three:

- **Verifiable without judgement.** Upstream is the behavioural oracle. Ported
  code either reproduces its HTTP responses byte-for-byte or it does not, so
  "done" is a command's exit status rather than an agent's opinion of itself.
- **Genuinely hard.** FFI, `unsafe`, thread affinity, and a C library that will
  segfault rather than return an error. Work an agent cannot bluff through.
- **Falsifiable at the end.** The port is only worth anything if it is faster,
  and it might well not be. A benchmark that says "no difference" is a real
  result, and the loop has no way to talk its way out of it.

So the port is a substrate chosen to make the loop's failures *visible*. Most
of what has been learned so far is not about Rust or PHP. It is about the ways
an unattended loop goes wrong — grading itself against the wrong directory,
merging empty diffs, filing work it then prioritises above the actual project,
and parking the one issue everything else depended on in a state nothing could
get it out of. Every one of those happened here, and each is written up in
`orchestrator/logs/retro-*.md` — including the ones the loop diagnosed about
itself.

If you only read two things, read `orchestrator/loop.py` (the whole system, one
file) and those retrospectives.

## The workload: what is actually being ported

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

None of which the loop is allowed to know in advance. The benchmark's job here
is to be the one claim an agent cannot write its way around: a loop that grades
its own work will eventually conclude that its own work is good, and the only
defence is a number produced by something that does not care. If FrankenRust
ends up slower, that is a successful experiment and a failed port, and the two
are not the same thing.

## Running the loop

GitHub Issues are the queue. Labels are the state machine, dependencies are
declared in the issue body (`Depends on: #3`), and agents can write back to the
queue — filing what they discover, or refusing an issue they think is wrong.
Work merges only if it passes `scripts/gate.sh`, which agents are forbidden to
weaken.

```sh
python3 orchestrator/loop.py seed      # planner agent files the initial issues
python3 orchestrator/loop.py run       # drain the queue
python3 orchestrator/loop.py status    # ready / claimed / blocked / questioned
python3 orchestrator/loop.py retro     # retrospective on demand
```

Knobs: `FR_PARALLEL` (3), `FR_ATTEMPTS` (3), `FR_WALLCLOCK` (8h),
`FR_AGENT_TIMEOUT` (1h/attempt), `FR_MAX_REVISIONS` (2 re-scopes per issue).
Who runs what, and with which model, lives in **`orchestrator/config.py`** —
the weekly split change is a one-line edit there (or a `FR_*` value in
`orchestrator/.env`, copied from `.env.example`). Precedence: real
environment variables > `.env` file > config.py defaults. The knobs:
`FR_AGENT_<ROLE>` (implementer/fixer/critic/reviewer/planner/resolver/
unblocker/retro), `FR_MODEL_<ROLE>` (claude table), `FR_OPENCODE_MODEL_<ROLE>`
(opencode table), `FR_REVIEWER1`/`FR_REVIEWER2` (review roster),
`FR_DUEL_AGENTS`/`FR_DUEL_MODELS` (duel rotation), `FR_MODEL_ESCALATE`/
`FR_OPENCODE_MODEL_ESCALATE` (final attempt of a failing issue).

Right now everything defaults to **opencode** on free models — the run is
testing end to end, and claude is quota-starved. **claude** and **codex** stay
fully wired (`Agent: codex` in an issue body opts the issue into codex), so
the day the weekly limit resets, moving the judgement roles (review, critic,
resolver) back to claude is a config change, not a code change. Codex and
opencode run on quotas that can run out mid-run. When one does, the loop
detects it, falls back to claude for everything remaining, and keeps going.

**You have to start it yourself.** The agents run with permission prompts
disabled — that is what "unattended" requires — and authorizing a multi-hour
agent fleet with that much latitude is a decision for you to make, not something
to be started on your behalf. Logs land in `orchestrator/logs/<task-id>/`.

### How an issue is processed

```
claim ─► critic ─┬─ REVISE ─► resolver ─┬─ REWRITE ─► re-scope, back to fr:ready
                 │                      ├─ CLOSE   ─► killed, with evidence
                 │                      └─ PROCEED ─┐ (critic overruled)
                 └─ PROCEED ────────────────────────┴─► implementer
                         │
└─► gate ─┬─ fail ─► retry (≤3, escalating model)
                                    └─ pass ─► 2 adversarial reviewers
                                        (both opencode right now;
                                         cross-vendor slots via FR_REVIEWER1/2,
                                         independent contexts, diff only)
                                        ├─ BLOCK ─► fixer ─► re-gate ─► re-review
                                        └─ PASS  ─► rebase, re-gate, merge, close
```

The **critic** runs before any code is written. The issues were filed by an
agent, not by someone who read the codebase, so they may be wrong, oversized, or
already done. An agent that faithfully implements a bad issue produces work that
passes the gate and looks like progress — that is the failure mode this stage
exists to prevent.

The **resolver** exists because the critic's objection would otherwise be a dead
end. Parking an issue as `fr:questioned` assumes a human will come back and
re-scope it; nobody is coming. So a second agent researches the objection
against `vendor/frankenphp/` and the tree, and must return a decision —
re-scope, kill it with evidence, or overrule the critic. It may re-scope an
issue twice; after that it has to decide outright, or critic and resolver will
hand the same issue back and forth indefinitely.

### Self-improvement

Every stage outcome is appended to `orchestrator/logs/events.jsonl`. After every
merge — the only event that produces new evidence — a **retrospective** reads
that journal, not the agent transcripts, which are too large and unstructured to
yield anything but impressions. It looks for patterns rather than incidents: one
gate failure is a hard task; four with the same error is a broken harness. Three
`critic_revise` events with the same reason means the *planner prompt* is wrong,
not the three issues. It runs on its own thread, so it never stalls a worker.

It then files its own fixes, and it is allowed to fix anything — including
itself. Changes to prompts, docs, the gate and the bench take effect on the next
issue, since those are re-read every invocation. A merged change to `loop.py` or
`gh.py` cannot take effect in a process that already imported them, so the
orchestrator **re-execs into the new code** at the next batch boundary, once no
agent is mid-flight. Queue state lives in GitHub issues, so the successor picks
up exactly where its predecessor stopped.

What makes that survivable is `scripts/check_orchestrator.py`, wired into every
gate profile: it parses both modules, runs `loop.py status`, and verifies every
role the loop can dispatch has a prompt file. A syntax error or a missing prompt
cannot reach `main`, because it would end the run with nobody there to restart
it. The retrospective is told, in as many words, that it may never weaken that
check nor propose removing the gate, the reviewers, or the critic — a loop that
can delete its own checks eventually will, since that makes every subsequent
issue trivially closeable.

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
orchestrator/loop.py        the entire system: claim, critique, implement, gate, review, merge
orchestrator/gh.py          GitHub Issues as the work queue, labels as the state machine
orchestrator/prompts/       one file per role — the only place agent behaviour is specified
orchestrator/logs/          events.jsonl (the journal) and retro-*.md (the loop on itself)
scripts/gate.sh             the merge gate. agents are forbidden to weaken it
scripts/check_orchestrator.py   the loop's self-checks, run in every gate profile

crates/frankenrust-sys      raw FFI: bindgen over PHP headers, compiles upstream's C
crates/frankenrust-core     safe layer: threads, request context, the 25 callbacks
crates/frankenrust-server   hyper HTTP/1.1 server, async↔pthread bridge
vendor/frankenphp/          upstream, READ-ONLY. the behavioural oracle
tests/conformance/          differential tests vs upstream's own PHP fixtures
docs/PORTING-NOTES.md       the pattern map every agent reads first
```

The top half is the experiment; the bottom half is the workload it runs on.

`vendor/frankenphp/testdata/` is the reason this is verifiable at all: 73 PHP
fixtures that are implementation-agnostic, so the same script served by any
correct server must produce the same HTTP response. Upstream's `_executor.php`
even runs each fixture in both regular and worker mode. That corpus is our
oracle, the same way Bun's TypeScript test suite could validate a Rust rewrite
of a Zig codebase.
