#!/usr/bin/env python3
"""Self-tests for the conformance harness itself, run before every replay.

A replay that is green tells you the corpus matched the goldens *on this
machine, run serially*. Each check here encodes an invariant that a green
replay cannot see, because every one of them was broken while the replay was
passing:

  check_goldens_self_consistent   -- goldens carried a machine-specific
                                     Content-Length (they matched here and
                                     would have hard-failed on Linux Docker).
  check_corpus_golden_bijection   -- deleting a [[cases]] entry silently
                                     shrank coverage and still printed PASS.
  check_concurrent_isolation      -- two overlapping runs destroyed each
                                     other's container, failing whichever
                                     started first.

Usage: python3 selftest.py
"""

from __future__ import annotations

import difflib
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import common
import replay


def parse_golden(raw: bytes) -> tuple[bytes, list[tuple[str, str]], bytes]:
    """Split a golden into (status line, headers, body).

    Mirrors render_http()'s LF-separated layout: status line, header lines,
    a blank line, then the body verbatim (which may itself contain LFs, hence
    partition on the first blank line only).
    """
    head, sep, body = raw.partition(b"\n\n")
    if not sep:
        raise ValueError("no blank line separating headers from body")
    lines = head.split(b"\n")
    headers = []
    for line in lines[1:]:
        name, _, value = line.partition(b": ")
        headers.append((name.decode("latin-1"), value.decode("latin-1")))
    return lines[0], headers, body


def check_goldens_self_consistent() -> list[str]:
    """Every golden's Content-Length must equal its own (normalised) body.

    This is the check that would have caught the machine-specific goldens.
    Before normalize() restated the header, server-all-vars-ordered.http said
    `Content-Length: 882` above a 902-byte body: the header was the raw wire
    length, which encodes the byte lengths of the client address, the port and
    the document root that the body substitutions had just replaced with
    placeholders. It matched on the capture machine and could not match on a
    host whose Docker gateway address is a different number of characters.
    A golden whose declared length disagrees with the bytes printed next to it
    is, by construction, carrying information from outside the golden.
    """
    failures = []
    for path in sorted(common.GOLDEN_DIR.glob("*.http")):
        try:
            _, headers, body = parse_golden(path.read_bytes())
        except ValueError as e:
            failures.append(f"{path.name}: malformed golden: {e}")
            continue
        for name, value in headers:
            if name.lower() != "content-length":
                continue
            try:
                declared = int(value)
            except ValueError:
                failures.append(f"{path.name}: non-integer Content-Length {value!r}")
                continue
            if declared != len(body):
                failures.append(
                    f"{path.name}: Content-Length: {declared} but the golden body is "
                    f"{len(body)} byte(s). A golden that declares a length it does not "
                    f"contain has re-imported a value normalisation was meant to strip; "
                    f"see normalize() in common.py."
                )
    return failures


def _synthetic_response(
    document_root: str, port: int, remote_addr: str, php_version: str, server_software: str
) -> tuple[common.Response, common.NormalizeContext]:
    """A response shaped like server-all-vars-ordered's, with every normalised
    value parameterised so its length can be varied."""
    body = (
        f"DOCUMENT_ROOT:{document_root}\n"
        f"SCRIPT_FILENAME:{document_root}/x.php\n"
        f"HTTP_HOST:127.0.0.1:{port}\n"
        f"SERVER_PORT:{port}\n"
        f"REMOTE_ADDR:{remote_addr}\n"
        f"REMOTE_HOST:{remote_addr}\n"
        f"SERVER_SOFTWARE:{server_software}\n"
        f"POWERED:PHP/{php_version}\n"
    ).encode("latin-1")
    headers = [
        ("Content-Type", "text/html; charset=UTF-8"),
        ("X-Powered-By", f"PHP/{php_version}"),
        ("Content-Length", str(len(body))),
    ]
    ctx = common.NormalizeContext(
        document_root=document_root,
        port=port,
        server_software=server_software,
        remote_addr=remote_addr,
    )
    return common.Response(200, "OK", headers, body), ctx


def check_normalisation_is_length_invariant() -> list[str]:
    """normalize() must not leak the *lengths* of the values it substitutes.

    check_goldens_self_consistent catches the artifact; this catches the
    mechanism, with no container involved. Two responses that differ only in
    the document root, port, client address, PHP version and server software
    -- each a different number of characters -- must normalise to the same
    bytes, because those are exactly the five things the corpus declares are
    allowed to vary by machine and by target. Before Content-Length was
    restated against the normalised body, these two differed by the sum of
    those length deltas in the header while being identical in the body, which
    is how a golden captured on Docker Desktop (client address 192.168.65.1)
    hard-failed on Linux Docker (172.17.0.1) forever after.
    """
    a, ctx_a = _synthetic_response("/app/public", 52432, "192.168.65.1", "8.4.1", "FrankenPHP")
    b, ctx_b = _synthetic_response("/srv/www", 8081, "172.17.0.1", "8.5.0-dev", "FrankenRust")

    rendered_a = common.render_http(common.normalize(a, ctx_a))
    rendered_b = common.render_http(common.normalize(b, ctx_b))
    if rendered_a == rendered_b:
        return []
    return [
        "normalize() is not invariant under the lengths of the values it substitutes; "
        "goldens captured on one machine cannot match on another.\n"
        + "\n".join(
            difflib.unified_diff(
                rendered_a.decode("latin-1").splitlines(),
                rendered_b.decode("latin-1").splitlines(),
                fromfile="docker-desktop-shaped",
                tofile="linux-docker-shaped",
                lineterm="",
            )
        )
    ]


def check_corpus_golden_bijection(corpus: dict) -> list[str]:
    """corpus.toml cases and golden/*.http must be exactly one-to-one.

    replay.py reports a case with no golden, but nothing reported a golden
    with no case: deleting a [[cases]] entry dropped it from the run while
    still printing PASS, so coverage could shrink silently. Guard both
    directions, and refuse to treat an empty corpus as success.
    """
    failures = []
    cases = corpus.get("cases", [])
    if not cases:
        return ["corpus.toml defines no [[cases]]; an empty corpus is a failure, not a pass"]

    case_names = [c["name"] for c in cases]
    duplicates = {n for n in case_names if case_names.count(n) > 1}
    for name in sorted(duplicates):
        failures.append(f"corpus.toml: duplicate case name {name!r} (goldens would collide)")

    golden_names = {p.stem for p in common.GOLDEN_DIR.glob("*.http")}
    for name in sorted(set(case_names) - golden_names):
        failures.append(f"case {name!r} in corpus.toml has no golden/{name}.http")
    for name in sorted(golden_names - set(case_names)):
        failures.append(
            f"golden/{name}.http has no [[cases]] entry in corpus.toml -- it is never "
            f"replayed, so its coverage is gone while the run still passes"
        )
    return failures


def check_concurrent_isolation(corpus: dict) -> list[str]:
    """Two overlapping harness runs must not tear down each other's container.

    This is the check that would have caught the shared container name.
    scripts/gate.sh runs conformance in every non-bootstrap profile and
    orchestrator/loop.py gates MAX_PARALLEL worktrees at once, so overlapping
    runs are normal. With a constant name and a `docker rm -f` before
    `docker run`, starting the second container killed the first; the first
    run then burned its whole wait_for_server timeout and failed the gate on
    someone else's diff.

    Modelled at the container lifecycle rather than by shelling out to two
    full run.sh invocations: it exercises the same start/stop code path, is
    deterministic instead of timing-dependent, and costs one extra container.
    """
    target = corpus["targets"]["upstream"]
    image, container_port = target["image"], target["container_port"]
    failures: list[str] = []
    first = second = None

    try:
        first = common.start_upstream_container(image, container_port)
        common.wait_for_server("127.0.0.1", first[1])

        second = common.start_upstream_container(image, container_port)
        common.wait_for_server("127.0.0.1", second[1])

        if first[0] == second[0]:
            failures.append(
                f"both containers were named {first[0]!r}: concurrent runs share an "
                f"identity and will remove each other"
            )

        # The load-bearing assertion: starting the second run's container must
        # have left the first run's alone and serving.
        try:
            common.wait_for_server("127.0.0.1", first[1], timeout=5)
        except RuntimeError as e:
            failures.append(f"starting a second container killed the first: {e}")

        # ...and so must tearing it down again.
        common.stop_container(second[0])
        second = None
        try:
            common.wait_for_server("127.0.0.1", first[1], timeout=5)
        except RuntimeError as e:
            failures.append(f"stopping a second container killed the first: {e}")
    finally:
        for started in (first, second):
            if started is not None:
                common.stop_container(started[0])

    return failures


def check_frankenrust_replay_branch_detects_mismatch(corpus: dict) -> list[str]:
    """replay.py's frankenrust branch must fold a mismatch into total_failures.

    Issue #141: before this fix, replay.py detected a local `frankenrust:bench`
    image, printed "found, but this harness has no replay path for it yet --
    not compared", and exited 0 no matter what the container returned. Nine
    merges landed 11,244 lines of Rust under the `default` gate profile while
    that branch existed only as a print statement -- a green replay proved
    only that the pinned upstream image agrees with itself.

    This does not wait for #15 to build frankenrust:bench: replay_frankenrust()
    takes `image` as a parameter precisely so this check can drive the exact
    function main() would call, pointed at the upstream image instead, against
    a golden this check corrupts itself. If replay_frankenrust() is reverted to
    a print (or its failures stop being returned), this goes red because
    `failures` comes back empty for a golden that cannot possibly match.

    The real golden directory is never touched: common.GOLDEN_DIR is pointed at
    a throwaway temp directory containing one deliberately-wrong copy of
    golden/hello.http for the duration of the call, then restored, so a crash
    mid-check cannot leave a corrupted golden behind.
    """
    case = next(c for c in corpus["cases"] if c["name"] == "hello")
    mini_corpus = {**corpus, "cases": [case]}
    upstream_image = corpus["targets"]["upstream"]["image"]

    real_golden = (common.GOLDEN_DIR / "hello.http").read_bytes()
    corrupted = real_golden + b"\nselftest-injected-corruption\n"

    real_golden_dir = common.GOLDEN_DIR
    with tempfile.TemporaryDirectory() as tmp:
        (Path(tmp) / "hello.http").write_bytes(corrupted)
        common.GOLDEN_DIR = Path(tmp)
        try:
            compared, failures = replay.replay_frankenrust(mini_corpus, image=upstream_image)
        finally:
            common.GOLDEN_DIR = real_golden_dir

    problems = []
    if compared != 1:
        problems.append(
            f"replay_frankenrust compared {compared} case(s) against a 1-case corpus -- "
            "expected exactly 1"
        )
    if not any("mismatch" in f for f in failures):
        problems.append(
            "replay_frankenrust did not report a mismatch against a golden this check "
            "deliberately corrupted -- a real mismatch against frankenrust:bench would "
            "pass the gate silently"
        )
    return problems


def main() -> int:
    corpus = common.load_corpus()

    checks = [
        ("goldens self-consistent", lambda: check_goldens_self_consistent()),
        ("normalisation length-invariant", lambda: check_normalisation_is_length_invariant()),
        ("corpus/golden bijection", lambda: check_corpus_golden_bijection(corpus)),
        ("concurrent isolation", lambda: check_concurrent_isolation(corpus)),
        (
            "frankenrust replay branch detects mismatch",
            lambda: check_frankenrust_replay_branch_detects_mismatch(corpus),
        ),
    ]

    all_failures = []
    for label, check in checks:
        failures = check()
        status = "ok" if not failures else f"FAIL ({len(failures)})"
        print(f"    selftest: {label}: {status}")
        all_failures.extend(failures)

    if all_failures:
        print()
        print(f"conformance selftest: FAIL -- {len(all_failures)} problem(s)")
        for f in all_failures:
            print(f"  - {f}")
        return 1

    print(f"    selftest: {len(checks)} check(s) passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
