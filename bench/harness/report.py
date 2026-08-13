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
import tempfile
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


def _throughputs(runs: dict, server: str, app: str) -> list[float]:
    vals = [r.get("summary", {}).get("requestsPerSec") for r in runs.get((server, app, "throughput"), [])]
    return [v for v in vals if v is not None]


def verdict_lines(runs: dict, servers: list[str], app: str) -> list[str]:
    """Throughput-delta lines for `app`, one per non-baseline server.

    The baseline is frankenphp when it is among `servers`, otherwise the
    first server in sorted order -- so a 3-way run (frankenphp, frankenrust,
    pasir) gets a verdict against frankenphp for each of the other two,
    instead of the old code's silent no-op once a third server existed.
    """
    if len(servers) < 2:
        return []
    baseline = "frankenphp" if "frankenphp" in servers else servers[0]
    ra = _throughputs(runs, baseline, app)
    if not ra:
        return []
    lines: list[str] = []
    for s in servers:
        if s == baseline:
            continue
        rb = _throughputs(runs, s, app)
        if not rb:
            continue
        delta = (statistics.median(rb) / statistics.median(ra) - 1) * 100
        verdict = "within noise" if abs(delta) < 5 else ("faster" if delta > 0 else "slower")
        lines.append(f"**{s} vs {baseline}: {delta:+.1f}% throughput ({verdict}).**")
        if abs(delta) < 5:
            lines.append("A sub-5% delta on this hardware should be read as a tie.")
        lines.append("")
    return lines


def build_report(d: Path) -> list[str] | None:
    runs = load(d)
    if not runs:
        return None

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
            lat = runs.get((s, app, "latency"), [])
            rps_all = _throughputs(runs, s, app)
            rps = statistics.median(rps_all) if rps_all else None
            spread = f"{min(rps_all):,.0f}–{max(rps_all):,.0f}" if len(rps_all) > 1 else "—"
            row = [s, fmt(rps), spread]
            # oha's key is literally "p99.9" -- not "p99.90"/"p99.900"
            for p in ("p50", "p95", "p99", "p99.9"):
                row.append(fmt(med([pct(r, p) for r in lat])))
            out.append("| " + " | ".join(row) + " |")
        out.append("")

        out.extend(verdict_lines(runs, servers, app))

    return out


def main() -> int:
    d = Path(sys.argv[1])
    out = build_report(d)
    if out is None:
        print("No result files found. Did any server actually start?")
        return 1
    print("\n".join(out))
    return 0


# --- --selftest ---------------------------------------------------------------
# Nothing in any gate profile exercised the verdict logic before #6, which is
# exactly how a 3-server run went silently unreported: the len(servers) == 2
# gate degraded to a no-op the moment frankenrust (#15) landed, and nothing
# caught it. This drives build_report() against synthetic oha JSON written to
# a tmp dir -- no docker, no real bench run -- and checks the baseline-relative
# rewrite against a frozen copy of the old two-server-only algorithm so a
# future change to the delta math or wording has to break this test to ship.
def _legacy_verdict_lines(runs: dict, servers: list[str], app: str) -> list[str]:
    """Frozen copy of the pre-#6 delta block (len(servers) == 2 only)."""
    lines: list[str] = []
    if len(servers) == 2:
        a, b = servers
        ra = _throughputs(runs, a, app)
        rb = _throughputs(runs, b, app)
        if ra and rb:
            delta = (statistics.median(rb) / statistics.median(ra) - 1) * 100
            verdict = "within noise" if abs(delta) < 5 else ("faster" if delta > 0 else "slower")
            lines.append(f"**{b} vs {a}: {delta:+.1f}% throughput ({verdict}).**")
            if abs(delta) < 5:
                lines.append("A sub-5% delta on this hardware should be read as a tie.")
            lines.append("")
    return lines


def _write_run(d: Path, server: str, app: str, mode: str, rep: int, rps: float) -> None:
    payload = {
        "summary": {"requestsPerSec": rps},
        "latencyPercentiles": {"p50": 0.001, "p95": 0.002, "p99": 0.003, "p99.9": 0.004},
    }
    (d / f"{server}.{app}.{mode}.{rep}.json").write_text(json.dumps(payload))


SELFTEST_OK = True


def st(desc: str, ok: bool) -> None:
    global SELFTEST_OK
    if ok:
        print(f"ok - {desc}")
    else:
        print(f"FAIL - {desc}")
        SELFTEST_OK = False


def run_selftest() -> int:
    global SELFTEST_OK
    SELFTEST_OK = True

    # -- 3 servers: one verdict line per non-baseline server, vs frankenphp --
    with tempfile.TemporaryDirectory() as tmp:
        d = Path(tmp)
        for rep in (1, 2, 3):
            _write_run(d, "frankenphp", "hello", "throughput", rep, 1000.0)
            _write_run(d, "frankenrust", "hello", "throughput", rep, 1200.0)
            _write_run(d, "pasir", "hello", "throughput", rep, 800.0)
        out = build_report(d)
        st("3 servers: build_report does not crash", out is not None)
        text = "\n".join(out or [])
        st(
            "3 servers: frankenrust vs frankenphp line present with the right delta",
            "**frankenrust vs frankenphp: +20.0% throughput (faster).**" in text,
        )
        st(
            "3 servers: pasir vs frankenphp line present with the right delta",
            "**pasir vs frankenphp: -20.0% throughput (slower).**" in text,
        )
        st(
            "3 servers: no cross line between the two non-baseline servers",
            "vs frankenrust" not in text and "vs pasir" not in text,
        )
        verdict_count = sum(1 for line in (out or []) if line.startswith("**") and " vs " in line)
        st("3 servers: exactly one verdict line per non-baseline server", verdict_count == 2)

    # -- 2 servers: byte-identical to the pre-#6 two-server-only algorithm ---
    with tempfile.TemporaryDirectory() as tmp:
        d = Path(tmp)
        for rep in (1, 2, 3):
            _write_run(d, "frankenphp", "hello", "throughput", rep, 1000.0)
            _write_run(d, "frankenrust", "hello", "throughput", rep, 1150.0)
        runs = load(d)
        servers = sorted({k[0] for k in runs})
        new_lines = verdict_lines(runs, servers, "hello")
        legacy_lines = _legacy_verdict_lines(runs, servers, "hello")
        st("2 servers: verdict line is actually produced (test isn't vacuous)", len(new_lines) > 0)
        st("2 servers: baseline-relative output is byte-identical to the old algorithm", new_lines == legacy_lines)

        full = build_report(d)
        st("2 servers: full report does not crash", full is not None)

    # -- 1 server: no verdict line, no crash ----------------------------------
    with tempfile.TemporaryDirectory() as tmp:
        d = Path(tmp)
        for rep in (1, 2, 3):
            _write_run(d, "frankenphp", "hello", "throughput", rep, 1000.0)
        out = build_report(d)
        st("1 server: build_report does not crash", out is not None)
        has_verdict = any(line.startswith("**") and " vs " in line for line in (out or []))
        st("1 server: no verdict line", not has_verdict)

    print()
    if SELFTEST_OK:
        print("SELFTEST PASS")
    else:
        print("SELFTEST FAIL")
    return 0 if SELFTEST_OK else 1


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--selftest":
        sys.exit(run_selftest())
    sys.exit(main())
