"""Shared plumbing for the golden HTTP conformance corpus.

Used by capture.py (writes tests/conformance/golden/*.http from the pinned
upstream container) and replay.py (replays the same corpus against a target
and diffs against those goldens). See tests/conformance/corpus.toml for the
manifest format and the normalisation rationale.

Talks HTTP directly via http.client rather than shelling out to curl, so
that request bytes are exactly what this file says and not whatever a given
curl build's defaults happen to be (curl's own default User-Agent embeds its
version). See the comment at the top of corpus.toml for the one place that
matters most: it makes it possible to send a genuinely empty header value or
omit a header outright, both of which curl needs its own argv conventions
for.
"""

from __future__ import annotations

import base64
import http.client
import os
import re
import socket
import subprocess
import time
import tomllib
from dataclasses import dataclass
from pathlib import Path

CONFORMANCE_DIR = Path(__file__).resolve().parent.parent
GOLDEN_DIR = CONFORMANCE_DIR / "golden"
CORPUS_PATH = CONFORMANCE_DIR / "corpus.toml"

# Every container this harness starts carries this label. The container *name*
# is unique per run (see container_name()), so nothing here may ever be reaped
# by name; if a run is SIGKILLed hard enough to skip its `finally`, sweep the
# strays by label instead:
#     docker rm -f $(docker ps -aq --filter label=frankenrust.conformance)
CONTAINER_LABEL = "frankenrust.conformance=upstream"


def container_name(host_port: int) -> str:
    """A container name unique to this run.

    Deliberately not a constant. scripts/gate.sh runs conformance in every
    non-bootstrap profile, and orchestrator/loop.py runs MAX_PARALLEL gates
    concurrently from separate worktrees, so two conformance runs overlapping
    is the normal operating mode, not an edge case. A shared name plus the
    `docker rm -f` that used to precede `docker run` meant the second run tore
    down the first run's still-in-use container: the first run then sat out its
    full wait_for_server timeout and failed the gate for a reason that had
    nothing to do with its own diff. pid distinguishes concurrent processes on
    this host; host_port distinguishes sequential runs within one process.
    """
    return f"frankenrust-conformance-upstream-{os.getpid()}-{host_port}"


def load_corpus() -> dict:
    with open(CORPUS_PATH, "rb") as f:
        corpus = tomllib.load(f)
    validate_corpus(corpus)
    return corpus


def validate_corpus(corpus: dict) -> None:
    """Reject a corpus whose skip_targets carries no reason.

    A `skip_targets` entry says a case is silently excluded from one target's
    replay; without a `skip_reason` that is indistinguishable from a mistake,
    and issue #141 was filed exactly because a silent "not compared" branch
    stayed green through 11,244 lines of unreviewed Rust. Failing the load is
    what keeps that from happening again at the per-case level.
    """
    for case in corpus.get("cases", []):
        skip_targets = case.get("skip_targets")
        if skip_targets and not case.get("skip_reason"):
            raise ValueError(
                f"corpus.toml: case {case.get('name')!r} has skip_targets "
                f"{skip_targets!r} but no skip_reason -- a skip with no reason "
                f"is a silent pass, the same defect class #141 was filed against"
            )


@dataclass
class Response:
    status: int
    reason: str
    headers: list[tuple[str, str]]
    body: bytes


@dataclass
class NormalizeContext:
    document_root: str
    port: int
    server_software: str
    remote_addr: str | None = None


def free_port() -> int:
    # A short-lived probe socket to let the OS hand us an unused ephemeral
    # port; closed before the caller binds anything, same trick every
    # "find a free port" helper uses since there's no atomic reserve-and-hold
    # API for TCP ports.
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def build_body(case: dict) -> bytes:
    if "body" in case:
        return case["body"].encode("utf-8")
    if "body_repeat_char" in case:
        return (case["body_repeat_char"] * case["body_repeat_count"]).encode("ascii")
    if case.get("kind") == "multipart_upload":
        boundary = case["multipart_boundary"]
        field_name = case["multipart_field"]
        filename = case["multipart_filename"]
        content = case["multipart_content"].encode("utf-8")
        parts = [
            f"--{boundary}\r\n".encode(),
            (
                f'Content-Disposition: form-data; name="{field_name}"; '
                f'filename="{filename}"\r\n'
                f"Content-Type: text/plain\r\n\r\n"
            ).encode(),
            content,
            f"\r\n--{boundary}--\r\n".encode(),
        ]
        return b"".join(parts)
    return b""


def content_type_for(case: dict) -> str | None:
    if case.get("kind") == "multipart_upload":
        return f'multipart/form-data; boundary={case["multipart_boundary"]}'
    return None


def send_case(
    host: str, port: int, case: dict, defaults: list, timeout: float = 10
) -> Response:
    method = case["method"]
    path = case["path"]
    query = case.get("query")
    full_path = path + ("?" + query if query else "")
    body = build_body(case)

    headers: list[tuple[str, str]] = []
    if not case.get("headers_override", False):
        headers.extend((n, v) for n, v in defaults)
    headers.extend((n, v) for n, v in case.get("headers", []))

    ct = content_type_for(case)
    if ct is not None:
        headers.append(("Content-Type", ct))

    if "auth_user" in case:
        token = base64.b64encode(f'{case["auth_user"]}:'.encode()).decode()
        headers.append(("Authorization", f"Basic {token}"))

    conn = http.client.HTTPConnection(host, port, timeout=timeout)
    try:
        conn.putrequest(method, full_path, skip_accept_encoding=True)
        for name, value in headers:
            conn.putheader(name, value)
        if body:
            conn.putheader("Content-Length", str(len(body)))
        conn.endheaders()
        if body:
            conn.send(body)
        resp = conn.getresponse()
        resp_headers = resp.getheaders()
        resp_body = resp.read()
        return Response(resp.status, resp.reason, resp_headers, resp_body)
    finally:
        conn.close()


_PHP_VERSION_HEADER_RE = re.compile(r"^PHP/(\S+)$")


def normalize(resp: Response, ctx: NormalizeContext) -> Response:
    """Apply exactly the substitutions corpus.toml documents, and nothing else.

    Header values other than X-Powered-By and Content-Length, and body bytes
    outside the listed substrings, are left untouched.

    Content-Length is restated as the length of the *normalised* body. Issue
    #4 says not to normalise Content-Length, and separately says to normalise
    the client address because "without this the goldens are machine-
    specific"; on server-all-vars-ordered those two rules collide, because the
    server derives Content-Length from the un-normalised body and so the
    header re-exports the exact byte lengths of every value the body
    substitutions just hid. Measured: that body is 882 bytes with Docker
    Desktop's 192.168.65.1 (it appears twice, as REMOTE_ADDR and REMOTE_HOST)
    and 878 with Linux Docker's 172.17.0.1 -- both addresses named in the
    issue. Keeping the wire value would hard-fail the gate on every Linux
    host, for every later issue, for a reason unrelated to that issue's work.
    The same mechanism applies to {port} (twice in that body) and, once there
    is a frankenrust target to compare, to {documentRoot} and {phpVersion} in
    exception/response-headers/server-globals.

    So the rule is honoured in the direction that carries signal -- the header
    is kept, its position in the header order is kept, and a response that
    omits it still diffs -- and relaxed only in the byte count, which is a
    restatement of the body length rather than independent information. Note
    that nothing is lost by this: http.client reads exactly Content-Length
    bytes and raises IncompleteRead if the server sends fewer, so a server
    that lies about the length surfaces as a truncated *body* in the diff.
    check_wire_content_length() below keeps that transport guarantee explicit
    rather than assumed.
    """
    check_wire_content_length(resp)

    php_version = None
    for name, value in resp.headers:
        if name.lower() == "x-powered-by":
            m = _PHP_VERSION_HEADER_RE.match(value)
            if m:
                php_version = m.group(1)

    body_text = resp.body.decode("latin-1")
    body_text = body_text.replace(ctx.document_root, "{documentRoot}")
    body_text = body_text.replace(f":{ctx.port}", ":{port}")
    if ctx.remote_addr:
        body_text = body_text.replace(ctx.remote_addr, "{remoteAddr}")
    if php_version:
        body_text = body_text.replace(f"PHP/{php_version}", "PHP/{phpVersion}")
    body_text = body_text.replace(ctx.server_software, "{serverSoftware}")
    out_body = body_text.encode("latin-1")

    out_headers = []
    for name, value in resp.headers:
        lname = name.lower()
        if lname in ("date", "server"):
            continue
        if lname == "x-powered-by" and php_version:
            value = value.replace(f"PHP/{php_version}", "PHP/{phpVersion}")
        elif lname == "content-length":
            value = str(len(out_body))
        out_headers.append((name, value))

    return Response(resp.status, resp.reason, out_headers, out_body)


def check_wire_content_length(resp: Response) -> None:
    """Assert the declared Content-Length matches the body actually received.

    Under http.client this is close to tautological -- read() consumes exactly
    the declared number of bytes -- which is precisely why normalize() can
    restate the header against the normalised body without losing detection.
    Stating it here keeps that reasoning checkable instead of assumed, and
    would fire if the transport under this harness ever changed.
    """
    for name, value in resp.headers:
        if name.lower() != "content-length":
            continue
        try:
            declared = int(value)
        except ValueError:
            raise RuntimeError(f"non-integer Content-Length from server: {value!r}") from None
        if declared != len(resp.body):
            raise RuntimeError(
                f"server declared Content-Length: {declared} but sent "
                f"{len(resp.body)} body byte(s)"
            )


_REMOTE_ADDR_RE = re.compile(rb"REMOTE_ADDR:([0-9]+\.[0-9]+\.[0-9]+\.[0-9]+)")


def discover_remote_addr(resp: Response) -> str | None:
    m = _REMOTE_ADDR_RE.search(resp.body)
    if m:
        return m.group(1).decode("ascii")
    return None


def render_http(resp: Response) -> bytes:
    # LF, not CRLF, for the stored status-line/header block -- this is a
    # normalised representation for comparison (see normalize()'s docstring),
    # not the raw wire bytes, and git's line-ending normalisation would
    # silently rewrite a checked-in CRLF to LF on some platforms/configs,
    # which would make every golden fail to match its own freshly-rendered
    # self after a clone. Matches upstream's own golden-file convention
    # (vendor/frankenphp/testdata/server-all-vars-ordered.txt is LF-only too).
    lines = [f"HTTP/1.1 {resp.status} {resp.reason}".encode("latin-1")]
    for name, value in resp.headers:
        lines.append(f"{name}: {value}".encode("latin-1"))
    header_block = b"\n".join(lines)
    return header_block + b"\n\n" + resp.body


def docker_rm_f(name: str) -> None:
    subprocess.run(
        ["docker", "rm", "-f", name],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def start_upstream_container(
    image: str, container_port: int, attempts: int = 3
) -> tuple[str, int]:
    """Start the pinned upstream image; return (container name, host port).

    Owns port selection as well as the container, because the two have to
    agree and both have to be unique to this run -- see container_name(). No
    pre-emptive `docker rm -f` here: the name belongs to this run alone, so
    there is never anything of ours to clean up first, and removing by a name
    we did not create is exactly the bug this shape exists to prevent.

    free_port() is inherently racy (the probe socket is closed before docker
    binds), so a concurrent run can take the port in between and `docker run`
    fails to allocate it. That is the same spurious-concurrent-failure class,
    so retry with a fresh port rather than red-lining someone else's gate.
    """
    testdata_dir = CONFORMANCE_DIR.parent.parent / "vendor" / "frankenphp" / "testdata"
    for attempt in range(1, attempts + 1):
        host_port = free_port()
        name = container_name(host_port)
        result = subprocess.run(
            [
                "docker",
                "run",
                "-d",
                "--name",
                name,
                "--label",
                CONTAINER_LABEL,
                "-e",
                "SERVER_NAME=:80",
                "-p",
                f"{host_port}:{container_port}",
                "-v",
                f"{testdata_dir}:/app/public:ro",
                image,
            ],
            capture_output=True,
            text=True,
        )
        if result.returncode == 0:
            return name, host_port
        # A failed `docker run -d` can still leave the container created but
        # not started, which would hold the name; drop ours before retrying.
        docker_rm_f(name)
        if attempt == attempts:
            raise RuntimeError(
                f"docker run failed {attempts} time(s) for {image}; "
                f"last attempt on host port {host_port}: {result.stderr.strip()}"
            )
    raise AssertionError("unreachable")


def wait_for_server(host: str, port: int, timeout: float = 30) -> None:
    deadline = time.monotonic() + timeout
    last_err = None
    while time.monotonic() < deadline:
        try:
            conn = http.client.HTTPConnection(host, port, timeout=2)
            conn.request("GET", "/hello.php")
            resp = conn.getresponse()
            resp.read()
            conn.close()
            if resp.status == 200:
                return
        except (ConnectionRefusedError, OSError, http.client.HTTPException) as e:
            last_err = e
        time.sleep(0.5)
    raise RuntimeError(f"server on {host}:{port} never became ready: {last_err}")


def stop_container(name: str) -> None:
    docker_rm_f(name)


def image_exists(image: str) -> bool:
    result = subprocess.run(
        ["docker", "image", "inspect", image],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    return result.returncode == 0
