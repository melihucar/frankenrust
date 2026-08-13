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

CONTAINER_NAME = "frankenrust-conformance-upstream"


def load_corpus() -> dict:
    with open(CORPUS_PATH, "rb") as f:
        return tomllib.load(f)


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

    Header values (other than X-Powered-By) and body bytes outside the
    listed substrings are left untouched, including Content-Length -- which
    is deliberately NOT recomputed to match the (shorter or longer)
    normalised body text. Content-Length in the golden reflects real wire
    bytes the server sent; the body text next to it is a normalised
    representation for comparison. The two are allowed to disagree in byte
    count; that is not a bug in this harness.
    """
    php_version = None
    for name, value in resp.headers:
        if name.lower() == "x-powered-by":
            m = _PHP_VERSION_HEADER_RE.match(value)
            if m:
                php_version = m.group(1)

    out_headers = []
    for name, value in resp.headers:
        lname = name.lower()
        if lname in ("date", "server"):
            continue
        if lname == "x-powered-by" and php_version:
            value = value.replace(f"PHP/{php_version}", "PHP/{phpVersion}")
        out_headers.append((name, value))

    body_text = resp.body.decode("latin-1")
    body_text = body_text.replace(ctx.document_root, "{documentRoot}")
    body_text = body_text.replace(f":{ctx.port}", ":{port}")
    if ctx.remote_addr:
        body_text = body_text.replace(ctx.remote_addr, "{remoteAddr}")
    if php_version:
        body_text = body_text.replace(f"PHP/{php_version}", "PHP/{phpVersion}")
    body_text = body_text.replace(ctx.server_software, "{serverSoftware}")
    out_body = body_text.encode("latin-1")

    return Response(resp.status, resp.reason, out_headers, out_body)


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


def start_upstream_container(image: str, host_port: int, container_port: int) -> str:
    testdata_dir = CONFORMANCE_DIR.parent.parent / "vendor" / "frankenphp" / "testdata"
    docker_rm_f(CONTAINER_NAME)
    subprocess.run(
        [
            "docker",
            "run",
            "-d",
            "--name",
            CONTAINER_NAME,
            "-e",
            "SERVER_NAME=:80",
            "-p",
            f"{host_port}:{container_port}",
            "-v",
            f"{testdata_dir}:/app/public:ro",
            image,
        ],
        check=True,
        stdout=subprocess.DEVNULL,
    )
    return CONTAINER_NAME


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
