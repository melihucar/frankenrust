# FrankenPHP vs FrankenRust — 20260812T222725Z

Median of repeated runs. Both servers ran as linux/arm64 containers with
identical CPU and memory limits; load was generated from a third container
on the same docker network with `oha --latency-correction`.

> **These are relative numbers, not production numbers.** Docker Desktop on
> Apple Silicon runs everything inside a shared Linux VM. The virtualisation
> tax applies equally to both servers, so the comparison is fair — but the
> absolute RPS figures do not transfer to a Linux deployment.

## `compute`
_CPU-bound PHP. **Control**: both servers should tie. A gap here means the harness is broken._

| server | RPS (median) | spread | p50 ms | p95 ms | p99 ms | p99.9 ms |
|---|---:|---:|---:|---:|---:|---:|
| frankenphp | 568.18 | 560–576 | 8.13 | 24.33 | 63.61 | 104.56 |

