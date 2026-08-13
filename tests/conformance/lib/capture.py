#!/usr/bin/env python3
"""Capture the golden HTTP corpus from the pinned upstream FrankenPHP image.

Starts the container, replays every case in corpus.toml against it, and
writes tests/conformance/golden/<name>.http. Every case is sent three times
(or corpus.toml's min_replays, if higher) and must come back byte-identical
after normalisation before a golden is written -- a case that is flaky at
capture time would otherwise bake a coin-flip into every future gate run.

Usage: python3 capture.py
"""

from __future__ import annotations

import sys

sys.path.insert(0, str(__import__("pathlib").Path(__file__).resolve().parent))

import common


def capture_case(host: str, port: int, case: dict, defaults: list, ctx_base) -> bytes:
    replays = max(3, case.get("min_replays", 1))
    rendered = None
    for i in range(replays):
        resp = common.send_case(host, port, case, defaults)
        remote_addr = common.discover_remote_addr(resp)
        ctx = common.NormalizeContext(
            document_root=ctx_base.document_root,
            port=port,
            server_software=ctx_base.server_software,
            remote_addr=remote_addr,
        )
        normalized = common.normalize(resp, ctx)
        this_rendered = common.render_http(normalized)
        if rendered is None:
            rendered = this_rendered
        elif this_rendered != rendered:
            raise RuntimeError(
                f"case {case['name']!r} is not deterministic: replay {i + 1}/{replays} "
                f"differs from replay 1 after normalisation.\n"
                f"--- replay 1\n{rendered.decode('latin-1')}\n"
                f"--- replay {i + 1}\n{this_rendered.decode('latin-1')}"
            )
    return rendered


def main() -> int:
    corpus = common.load_corpus()
    target = corpus["targets"]["upstream"]
    defaults = corpus["defaults"]["headers"]

    name, port = common.start_upstream_container(
        target["image"], target["container_port"]
    )
    print(f"--- starting {target['image']} on host port {port}")
    try:
        common.wait_for_server("127.0.0.1", port)
        ctx_base = common.NormalizeContext(
            document_root=target["document_root"],
            port=port,
            server_software=target["server_software"],
        )

        common.GOLDEN_DIR.mkdir(parents=True, exist_ok=True)
        count = 0
        for case in corpus["cases"]:
            rendered = capture_case("127.0.0.1", port, case, defaults, ctx_base)
            golden_path = common.GOLDEN_DIR / f"{case['name']}.http"
            golden_path.write_bytes(rendered)
            print(f"    captured {case['name']} -> {golden_path.relative_to(common.CONFORMANCE_DIR.parent.parent)}")
            count += 1

        print(f"--- captured {count} cases")
        return 0
    finally:
        common.stop_container(name)


if __name__ == "__main__":
    raise SystemExit(main())
