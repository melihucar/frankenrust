# FrankenPHP vs FrankenRust — 20260812T221950Z

Median of repeated runs. Both servers ran as linux/arm64 containers with
identical CPU and memory limits; load was generated from a third container
on the same docker network with `oha --latency-correction`.

> **These are relative numbers, not production numbers.** Docker Desktop on
> Apple Silicon runs everything inside a shared Linux VM. The virtualisation
> tax applies equally to both servers, so the comparison is fair — but the
> absolute RPS figures do not transfer to a Linux deployment.

## `noop`
_PHP does nothing — this isolates server overhead. The only app where a difference is expected._

| server | RPS (median) | spread | p50 ms | p95 ms | p99 ms | p99.9 ms |
|---|---:|---:|---:|---:|---:|---:|
| frankenphp | 3,302.55 | 3,209–3,346 | 1.29 | 1.84 | 40.19 | 94.49 |

## `hello`
_Minimal output. Server overhead still dominates._

| server | RPS (median) | spread | p50 ms | p95 ms | p99 ms | p99.9 ms |
|---|---:|---:|---:|---:|---:|---:|
| frankenphp | 2,202.84 | 2,092–2,207 | 2.23 | 3.60 | 10.20 | 18.95 |

## `json`
_Small realistic response; exercises headers + output buffering._

| server | RPS (median) | spread | p50 ms | p95 ms | p99 ms | p99.9 ms |
|---|---:|---:|---:|---:|---:|---:|
| frankenphp | 2,362.89 | 2,292–2,466 | 1.64 | 2.28 | 7.50 | 25.43 |

## `compute`
_CPU-bound PHP. **Control**: both servers should tie. A gap here means the harness is broken._

| server | RPS (median) | spread | p50 ms | p95 ms | p99 ms | p99.9 ms |
|---|---:|---:|---:|---:|---:|---:|
| frankenphp | 566.15 | 564–576 | 4,564.49 | 8,967.40 | 9,356.11 | 9,423.38 |

