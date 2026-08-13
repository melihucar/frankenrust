#!/usr/bin/env bash
# Captures the golden HTTP corpus from the pinned upstream FrankenPHP
# container. Run this whenever tests/conformance/corpus.toml changes; the
# actual work lives in lib/capture.py (docker lifecycle, request building,
# normalisation) -- this script just locates python3 and hands off to it.
set -euo pipefail
cd "$(dirname "$0")"

command -v python3 >/dev/null 2>&1 || {
  echo "capture.sh requires python3 on PATH" >&2
  exit 1
}
command -v docker >/dev/null 2>&1 || {
  echo "capture.sh requires docker on PATH" >&2
  exit 1
}

exec python3 lib/capture.py
