#!/usr/bin/env python3
"""Replay the golden HTTP corpus against a target and diff against goldens.

Always replays against the pinned upstream image and fails on any mismatch
-- that makes the harness self-testing with no Rust in the tree: if the
corpus is not deterministic, upstream disagrees with its own goldens and
this exits non-zero. If a `frankenrust:bench` image exists locally, also
replays against it and fails on mismatch; today that image does not exist,
so that half is a no-op (logged, not silently skipped).

Never exits 0 while skipping the corpus: if a target can't be reached, that
is a failure, not a skip.

Usage: python3 replay.py
"""

from __future__ import annotations

import difflib
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import common

FRANKENRUST_BENCH_IMAGE = "frankenrust:bench"


def diff_text(expected: bytes, actual: bytes) -> str:
    return "\n".join(
        difflib.unified_diff(
            expected.decode("latin-1").splitlines(),
            actual.decode("latin-1").splitlines(),
            fromfile="golden",
            tofile="live",
            lineterm="",
        )
    )


def replay_case(host: str, port: int, case: dict, defaults: list, ctx_base) -> list[bytes]:
    replays = max(1, case.get("min_replays", 1))
    renderings = []
    for _ in range(replays):
        resp = common.send_case(host, port, case, defaults)
        remote_addr = common.discover_remote_addr(resp)
        ctx = common.NormalizeContext(
            document_root=ctx_base.document_root,
            port=port,
            server_software=ctx_base.server_software,
            remote_addr=remote_addr,
        )
        normalized = common.normalize(resp, ctx)
        renderings.append(common.render_http(normalized))
    return renderings


def run_against(target_name: str, host: str, port: int, ctx_base, corpus) -> tuple[int, list[str]]:
    defaults = corpus["defaults"]["headers"]
    failures: list[str] = []
    compared = 0

    for case in corpus["cases"]:
        name = case["name"]
        golden_path = common.GOLDEN_DIR / f"{name}.http"
        if not golden_path.exists():
            failures.append(f"{name}: no golden file at {golden_path}")
            continue
        golden = golden_path.read_bytes()

        try:
            renderings = replay_case(host, port, case, defaults, ctx_base)
        except Exception as e:  # noqa: BLE001 - report as a case failure, not a crash
            failures.append(f"{name}: request failed against {target_name}: {e}")
            continue

        first = renderings[0]
        for i, rendering in enumerate(renderings[1:], start=2):
            if rendering != first:
                failures.append(
                    f"{name}: non-deterministic against {target_name} -- replay {i}/"
                    f"{len(renderings)} differs from replay 1:\n"
                    + diff_text(first, rendering)
                )

        if first != golden:
            failures.append(
                f"{name}: mismatch against {target_name} golden "
                f"({len(renderings)} replay(s), all identical to each other: "
                f"{all(r == first for r in renderings)}):\n" + diff_text(golden, first)
            )

        compared += 1

    return compared, failures


def replay_upstream(corpus) -> tuple[int, list[str]]:
    target = corpus["targets"]["upstream"]
    port = common.free_port()
    print(f"--- replaying against upstream ({target['image']}) on host port {port}")
    common.start_upstream_container(target["image"], port, target["container_port"])
    try:
        common.wait_for_server("127.0.0.1", port)
        ctx_base = common.NormalizeContext(
            document_root=target["document_root"],
            port=port,
            server_software=target["server_software"],
        )
        return run_against("upstream", "127.0.0.1", port, ctx_base, corpus)
    finally:
        common.stop_container(common.CONTAINER_NAME)


def main() -> int:
    corpus = common.load_corpus()

    if not common.GOLDEN_DIR.exists() or not any(common.GOLDEN_DIR.glob("*.http")):
        print(f"FAIL: no golden files in {common.GOLDEN_DIR} -- run capture.sh first", file=sys.stderr)
        return 1

    total_failures: list[str] = []
    total_compared = 0

    compared, failures = replay_upstream(corpus)
    total_compared += compared
    total_failures.extend(failures)

    # Auto-detect only: issue #4 is scoped to upstream-vs-goldens (there is no
    # frankenrust:bench image yet, and no Rust in the tree to build one from).
    # A later issue that adds a frankenrust replay target should make this
    # branch call run_against(...) and fold its failures into total_failures
    # the same way replay_upstream does, so a mismatch fails the gate per
    # "fail on mismatch" above. Until then this is a loud no-op, not a skip
    # of anything this issue is responsible for verifying.
    if common.image_exists(FRANKENRUST_BENCH_IMAGE):
        print(
            f"--- {FRANKENRUST_BENCH_IMAGE} found, but this harness has no replay path for "
            "it yet (issue #4 scope: upstream only) -- not compared"
        )
    else:
        print(f"--- {FRANKENRUST_BENCH_IMAGE} not found locally, nothing to compare against it")

    print()
    if total_failures:
        print(f"conformance: FAIL -- {len(total_failures)} problem(s) across {total_compared} case(s) compared")
        for f in total_failures:
            print()
            print(f)
        return 1

    print(f"conformance: PASS -- {total_compared} case(s) compared, 0 mismatches")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
