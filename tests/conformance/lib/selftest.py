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

import contextlib
import difflib
import io
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import common
import replay


def _indent(text: str) -> str:
    """Inset captured sub-run output so it reads as evidence, not as this run's."""
    return "\n".join("      | " + line for line in text.splitlines())


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


# run_against() labels the frankenrust target's mismatches with this text.
# Grepping main()'s output for it -- rather than only for a non-zero exit --
# is what proves the failure that failed the run came from the frankenrust
# leg specifically, and not from somewhere else in main().
FRANKENRUST_MISMATCH_MARKER = "mismatch against frankenrust golden"


def check_frankenrust_mismatch_fails_the_run(corpus: dict) -> list[str]:
    """A frankenrust golden mismatch must drive replay.main() to a non-zero exit.

    Issue #141: before this fix, replay.py detected a local `frankenrust:bench`
    image, printed "found, but this harness has no replay path for it yet --
    not compared", and exited 0 no matter what the container returned. Nine
    merges landed 11,244 lines of Rust under the `default` gate profile while
    that branch existed only as a print statement -- a green replay proved
    only that the pinned upstream image agrees with itself.

    Asserting on main() rather than on replay_frankenrust() is the whole point
    of this check. The regression surface is the caller, not the helper: main()
    owns the image_exists() branch that decides whether the comparison happens
    at all, and the total_failures.extend() that decides whether its verdict
    reaches the exit code. An earlier version of this check called the helper
    directly and stayed green -- 5/5, rc 0 -- under both mutations it exists to
    catch: reverting the branch to a print, and deleting the extend(). Since no
    frankenrust:bench exists on any host yet, the gate's own replay takes the
    else-branch and never executes that wiring either, so nothing but this
    check covers the one edge that has to hold: helper result -> exit code.

    This does not wait for #15 to build frankenrust:bench. main()'s frankenrust
    leg is pointed at the upstream image by patching FRANKENRUST_BENCH_IMAGE
    (main() reads it at call time for both the existence probe and the replay)
    and given one deliberately-corrupted golden. The upstream leg is stubbed
    out rather than run: it is not what is under test, and against the same
    corrupted golden its own failures would drive main() to 1 on their own --
    masking a dropped extend() and reporting a green that means nothing. It
    also halves the containers this check starts.

    Everything patched is restored in `finally`, and the corrupted golden lives
    in a throwaway temp directory, so the real golden/ is never written to and
    a crash mid-check cannot leave a corrupted golden behind. main()'s output is
    captured rather than printed: it is a deliberate failure and would otherwise
    read, in the gate log, as a real conformance failure a few lines above the
    real run's output.
    """
    case = next((c for c in corpus["cases"] if c["name"] == "hello"), None)
    if case is None:
        return [
            "corpus.toml has no case named 'hello' -- this check needs one cheap case to "
            "replay; repoint it at another case rather than leaving it unable to run"
        ]
    mini_corpus = {**corpus, "cases": [case]}
    upstream_image = corpus["targets"]["upstream"]["image"]

    corrupted = (common.GOLDEN_DIR / "hello.http").read_bytes() + b"\nselftest-corruption\n"

    saved = (
        common.GOLDEN_DIR,
        common.load_corpus,
        common.image_exists,
        replay.FRANKENRUST_BENCH_IMAGE,
        replay.replay_upstream,
    )
    captured = io.StringIO()
    with tempfile.TemporaryDirectory() as tmp:
        (Path(tmp) / "hello.http").write_bytes(corrupted)
        try:
            common.GOLDEN_DIR = Path(tmp)
            common.load_corpus = lambda: mini_corpus
            # Not a blanket True: main() must probe for the same image it then
            # replays against, or the branch it takes is not the one shipped.
            common.image_exists = lambda image: image == upstream_image
            replay.FRANKENRUST_BENCH_IMAGE = upstream_image
            replay.replay_upstream = lambda _corpus: (1, [], [])
            with contextlib.redirect_stdout(captured):
                rc = replay.main()
        finally:
            (
                common.GOLDEN_DIR,
                common.load_corpus,
                common.image_exists,
                replay.FRANKENRUST_BENCH_IMAGE,
                replay.replay_upstream,
            ) = saved

    output = captured.getvalue()
    problems = []
    if rc == 0:
        problems.append(
            "replay.main() exited 0 while its frankenrust leg replayed against a golden "
            "this check deliberately corrupted. A real frankenrust:bench mismatch would "
            "pass the gate silently -- main() must call the frankenrust replay and fold "
            "its failures into total_failures. main() said:\n" + _indent(output)
        )
    if FRANKENRUST_MISMATCH_MARKER not in output:
        problems.append(
            f"replay.main() never reported {FRANKENRUST_MISMATCH_MARKER!r} against a golden "
            "this check deliberately corrupted: either the frankenrust branch did not run, "
            "or its failures were computed and discarded. main() said:\n" + _indent(output)
        )
    return problems


def check_skip_targets_require_reason() -> list[str]:
    """A `skip_targets` entry with no `skip_reason` must fail the load.

    Issue #163: a per-case, per-target skip (added so a case that kills one
    target's server process, e.g. finish-request against frankenrust before
    #14, does not take the whole replay down with it) is a silent "not
    compared" if nothing forces a reason to be written down -- exactly the
    defect class #141 was filed against, one level down. common.load_corpus()
    calls common.validate_corpus() for this reason; this check drives that
    function directly against a synthetic corpus so it needs no golden files,
    no container and does not depend on the real corpus.toml ever containing
    a bad entry to exercise the failure path.
    """
    bad_corpus = {
        "targets": {
            "upstream": {
                "image": "unused",
                "container_port": 80,
                "document_root": "/",
                "server_software": "Unused",
            }
        },
        "defaults": {"headers": []},
        "cases": [
            {
                "name": "unreasoned-skip",
                "fixture": "x.php",
                "method": "GET",
                "path": "/x.php",
                "skip_targets": ["frankenrust"],
            }
        ],
    }
    try:
        common.validate_corpus(bad_corpus)
    except ValueError:
        return []
    return [
        "common.validate_corpus() accepted a case with skip_targets but no "
        "skip_reason -- a skip with no reason is a silent pass"
    ]


def check_skipped_case_not_counted_as_compared(corpus: dict) -> list[str]:
    """A case skipped for a target must not count as compared, fail, or run.

    Same injection point as check_frankenrust_mismatch_fails_the_run: point
    replay.main()'s frankenrust leg at the pinned upstream image so this needs
    no frankenrust:bench, and stub replay_upstream so its "1 case(s)" is the
    only comparison in the total. Unlike that check, the mini corpus's one
    case is skip_targets = ["frankenrust"] rather than pointed at a corrupted
    golden -- a case that is skipped must never reach replay_case() at all
    (there is no golden for it in this run's real golden/ dir, so reaching the
    comparison would itself fail with "no golden file"), so a clean pass here
    with the skip reported is the only correct outcome: 0 added to compared,
    0 failures, the skip line printed, exit 0.
    """
    case = next((c for c in corpus["cases"] if c["name"] == "hello"), None)
    if case is None:
        return [
            "corpus.toml has no case named 'hello' -- this check needs one cheap case to "
            "replay; repoint it at another case rather than leaving it unable to run"
        ]
    reason = "selftest: exercising the skip path, not a real defect"
    skip_case = {**case, "skip_targets": ["frankenrust"], "skip_reason": reason}
    mini_corpus = {**corpus, "cases": [skip_case]}
    upstream_image = corpus["targets"]["upstream"]["image"]

    saved = (
        common.load_corpus,
        common.image_exists,
        replay.FRANKENRUST_BENCH_IMAGE,
        replay.replay_upstream,
    )
    captured = io.StringIO()
    try:
        common.load_corpus = lambda: mini_corpus
        common.image_exists = lambda image: image == upstream_image
        replay.FRANKENRUST_BENCH_IMAGE = upstream_image
        replay.replay_upstream = lambda _corpus: (1, [], [])
        with contextlib.redirect_stdout(captured):
            rc = replay.main()
    finally:
        (
            common.load_corpus,
            common.image_exists,
            replay.FRANKENRUST_BENCH_IMAGE,
            replay.replay_upstream,
        ) = saved

    output = captured.getvalue()
    problems = []
    if rc != 0:
        problems.append(
            "replay.main() exited non-zero when the only frankenrust-leg case was skipped "
            "-- a skip must never be treated as a failure. main() said:\n" + _indent(output)
        )
    if "1 case(s) compared, 1 skipped" not in output:
        problems.append(
            "replay.main() did not report exactly 1 case(s) compared (the upstream stub's "
            "count, unchanged by the skip) and 1 skipped -- either the skipped case was "
            "counted as compared, or the skipped count was not folded into the final line. "
            "main() said:\n" + _indent(output)
        )
    if f"skipped {skip_case['name']} against frankenrust: {reason}" not in output:
        problems.append(
            "replay.main() did not print the skipped case, its target and its reason on "
            "their own line. main() said:\n" + _indent(output)
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
            "frankenrust mismatch fails the run",
            lambda: check_frankenrust_mismatch_fails_the_run(corpus),
        ),
        ("skip_targets requires skip_reason", lambda: check_skip_targets_require_reason()),
        (
            "skipped case not counted as compared",
            lambda: check_skipped_case_not_counted_as_compared(corpus),
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
