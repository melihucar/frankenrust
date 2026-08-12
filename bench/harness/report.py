#!/usr/bin/env python3
"""Aggregate oha JSON runs into the comparison table.

Reports the MEDIAN across repetitions plus the min-max spread, because a single
run on a thermally-throttled laptop is noise. Throughput and latency come from
separate runs by design: percentiles measured while saturating the server are
not meaningful.
"""
from __future__ import annotations

import json
import statistics
import sys
from collections import defaultdict
from pathlib import Path

APP_NOTES = {
    "noop": "PHP does nothing — this isolates server overhead. The only app where a difference is expected.",
    "hello": "Minimal output. Server overhead still dominates.",
    "json": "Small realistic response; exercises headers + output buffering.",
    "compute": "CPU-bound PHP. **Control**: both servers should tie. A gap here means the harness is broken.",
}


def load(d: Path) -> dict:
    runs: dict[tuple[str, str, str], list[dict]] = defaultdict(list)
    for f in sorted(d.glob("*.json")):
        parts = f.stem.split(".")
        if len(parts) != 4:
            continue
        server, app, mode, _rep = parts
        try:
            runs[(server, app, mode)].append(json.loads(f.read_text()))
        except json.JSONDecodeError:
            print(f"<!-- unparseable: {f.name} -->", file=sys.stderr)
    return runs


def pct(run: dict, key: str) -> float | None:
    for holder in ("latencyPercentiles", "latency_percentiles"):
        if holder in run and key in run[holder]:
            return run[holder][key] * 1000.0  # oha reports seconds
    return None


def med(vals: list[float | None]) -> float | None:
    clean = [v for v in vals if v is not None]
    return statistics.median(clean) if clean else None


def fmt(v: float | None, unit: str = "") -> str:
    return f"{v:,.2f}{unit}" if v is not None else "—"


def main() -> int:
    d = Path(sys.argv[1])
    runs = load(d)
    if not runs:
        print("No result files found. Did any server actually start?")
        return 1

    servers = sorted({k[0] for k in runs})
    apps = sorted({k[1] for k in runs}, key=lambda a: list(APP_NOTES).index(a) if a in APP_NOTES else 99)

    out = [f"# FrankenPHP vs FrankenRust — {d.name}", ""]
    out.append("Median of repeated runs. Both servers ran as linux/arm64 containers with")
    out.append("identical CPU and memory limits; load was generated from a third container")
    out.append("on the same docker network with `oha --latency-correction`.")
    out.append("")
    out.append("> **These are relative numbers, not production numbers.** Docker Desktop on")
    out.append("> Apple Silicon runs everything inside a shared Linux VM. The virtualisation")
    out.append("> tax applies equally to both servers, so the comparison is fair — but the")
    out.append("> absolute RPS figures do not transfer to a Linux deployment.")
    out.append("")

    for app in apps:
        out.append(f"## `{app}`")
        if app in APP_NOTES:
            out.append(f"_{APP_NOTES[app]}_")
        out.append("")
        out.append("| server | RPS (median) | spread | p50 ms | p95 ms | p99 ms | p99.9 ms |")
        out.append("|---|---:|---:|---:|---:|---:|---:|")
        for s in servers:
            tp = runs.get((s, app, "throughput"), [])
            lat = runs.get((s, app, "latency"), [])
            rps_all = [r.get("summary", {}).get("requestsPerSec") for r in tp]
            rps_all = [r for r in rps_all if r is not None]
            rps = statistics.median(rps_all) if rps_all else None
            spread = f"{min(rps_all):,.0f}–{max(rps_all):,.0f}" if len(rps_all) > 1 else "—"
            row = [s, fmt(rps), spread]
            # oha's key is literally "p99.9" -- not "p99.90"/"p99.900"
            for p in ("p50", "p95", "p99", "p99.9"):
                row.append(fmt(med([pct(r, p) for r in lat])))
            out.append("| " + " | ".join(row) + " |")
        out.append("")

        if len(servers) == 2:
            a, b = servers
            ra = [r.get("summary", {}).get("requestsPerSec") for r in runs.get((a, app, "throughput"), [])]
            rb = [r.get("summary", {}).get("requestsPerSec") for r in runs.get((b, app, "throughput"), [])]
            ra = [x for x in ra if x is not None]
            rb = [x for x in rb if x is not None]
            if ra and rb:
                delta = (statistics.median(rb) / statistics.median(ra) - 1) * 100
                verdict = "within noise" if abs(delta) < 5 else ("faster" if delta > 0 else "slower")
                out.append(f"**{b} vs {a}: {delta:+.1f}% throughput ({verdict}).**")
                if abs(delta) < 5:
                    out.append("A sub-5% delta on this hardware should be read as a tie.")
                out.append("")

    print("\n".join(out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
