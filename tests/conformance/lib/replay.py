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

A [[cases]] entry may list a target in `skip_targets` to opt that one target
out of that one case -- e.g. a case that reaches a callback only one target
has implemented. common.load_corpus() refuses to load a corpus where
`skip_targets` is set without a `skip_reason`, so a skip can never be silent;
every skip is printed with its case, target and reason, and the skipped count
is reported alongside the compared count so a shrinking comparison is visible
in the final line rather than hidden inside a smaller "compared" number.

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


def run_against(
    target_name: str, host: str, port: int, ctx_base, corpus
) -> tuple[int, list[str], list[tuple[str, str, str]]]:
    defaults = corpus["defaults"]["headers"]
    failures: list[str] = []
    skipped: list[tuple[str, str, str]] = []
    compared = 0

    for case in corpus["cases"]:
        name = case["name"]

        if target_name in case.get("skip_targets", []):
            # common.validate_corpus() guarantees skip_reason is present
            # whenever skip_targets is non-empty -- see load_corpus().
            skipped.append((name, target_name, case["skip_reason"]))
            continue

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

    return compared, failures, skipped


def replay_container_target(
    target_name: str,
    image: str,
    container_port: int,
    document_root: str,
    server_software: str,
    corpus,
) -> tuple[int, list[str], list[tuple[str, str, str]]]:
    name, port = common.start_upstream_container(image, container_port)
    print(f"--- replaying against {target_name} ({image}) on host port {port}")
    try:
        common.wait_for_server("127.0.0.1", port)
        ctx_base = common.NormalizeContext(
            document_root=document_root,
            port=port,
            server_software=server_software,
        )
        return run_against(target_name, "127.0.0.1", port, ctx_base, corpus)
    finally:
        common.stop_container(name)


def replay_upstream(corpus) -> tuple[int, list[str], list[tuple[str, str, str]]]:
    target = corpus["targets"]["upstream"]
    return replay_container_target(
        "upstream",
        target["image"],
        target["container_port"],
        target["document_root"],
        target["server_software"],
        corpus,
    )


def replay_frankenrust(corpus, image: str) -> tuple[int, list[str], list[tuple[str, str, str]]]:
    """Replay against frankenrust:bench, folding failures the same way replay_upstream does.

    corpus.toml has no [targets.frankenrust] yet -- building that image is
    #15's job, out of scope for this issue (#141). Until it exists, this
    reuses the upstream target's container_port/document_root (the container
    that gets started always mounts the same vendor/frankenphp/testdata
    regardless of which image it runs) and assumes server_software =
    "FrankenRust"; whoever lands #15 should replace that assumption with a
    real [targets.frankenrust] section in corpus.toml once the image reports
    its own SERVER_SOFTWARE string.

    `image` is a required parameter rather than a default of
    FRANKENRUST_BENCH_IMAGE so that main() below reads that constant at call
    time -- for the existence probe and for this call, from the same lookup.
    That is the injection point lib/selftest.py uses to point main()'s whole
    frankenrust leg at the upstream image and drive it against a deliberately
    corrupted golden, proving the branch compares (and that main() folds its
    verdict into the exit code) without needing frankenrust:bench to exist.
    A default argument would be bound at def time and could not be redirected.
    """
    target = corpus["targets"]["upstream"]
    return replay_container_target(
        "frankenrust",
        image,
        target["container_port"],
        target["document_root"],
        "FrankenRust",
        corpus,
    )


def main() -> int:
    corpus = common.load_corpus()

    if not common.GOLDEN_DIR.exists() or not any(common.GOLDEN_DIR.glob("*.http")):
        print(f"FAIL: no golden files in {common.GOLDEN_DIR} -- run capture.sh first", file=sys.stderr)
        return 1

    total_failures: list[str] = []
    total_skipped: list[tuple[str, str, str]] = []
    total_compared = 0

    compared, failures, skipped = replay_upstream(corpus)
    total_compared += compared
    total_failures.extend(failures)
    total_skipped.extend(skipped)

    if common.image_exists(FRANKENRUST_BENCH_IMAGE):
        compared, failures, skipped = replay_frankenrust(corpus, FRANKENRUST_BENCH_IMAGE)
        total_compared += compared
        total_failures.extend(failures)
        total_skipped.extend(skipped)
    else:
        print(f"--- {FRANKENRUST_BENCH_IMAGE} not found locally, nothing to compare against it")

    for name, target_name, reason in total_skipped:
        print(f"--- skipped {name} against {target_name}: {reason}")

    print()
    if total_failures:
        print(
            f"conformance: FAIL -- {len(total_failures)} problem(s) across "
            f"{total_compared} case(s) compared, {len(total_skipped)} skipped"
        )
        for f in total_failures:
            print()
            print(f)
        return 1

    print(
        f"conformance: PASS -- {total_compared} case(s) compared, "
        f"{len(total_skipped)} skipped, 0 mismatches"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
