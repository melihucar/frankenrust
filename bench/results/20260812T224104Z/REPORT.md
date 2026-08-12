# FrankenPHP vs FrankenRust — 20260812T224104Z

Median of repeated runs. Both servers ran as linux/arm64 containers with
identical CPU and memory limits; load was generated from a third container
on the same docker network with `oha --latency-correction`.

> **These are relative numbers, not production numbers.** Docker Desktop on
> Apple Silicon runs everything inside a shared Linux VM. The virtualisation
> tax applies equally to both servers, so the comparison is fair — but the
> absolute RPS figures do not transfer to a Linux deployment.

## `hello`
_Minimal output. Server overhead still dominates._

| server | RPS (median) | spread | p50 ms | p95 ms | p99 ms | p99.9 ms |
|---|---:|---:|---:|---:|---:|---:|
| frankenphp | 24,597.03 | — | 0.62 | 1.29 | 4.54 | 12.32 |

