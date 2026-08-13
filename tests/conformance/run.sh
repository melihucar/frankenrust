#!/usr/bin/env bash
# Replays the golden HTTP corpus against upstream (always) and against
# frankenrust:bench (if that image exists) and diffs the normalised response
# against tests/conformance/golden/*.http. Wired into every non-bootstrap
# gate profile by scripts/gate.sh. The actual work lives in lib/replay.py;
# this script just locates python3 and hands off to it, and turns "can't
# even find the tools to run this" into a hard failure rather than a skip --
# a conformance gate that quietly skips is worse than no gate at all.
set -euo pipefail
cd "$(dirname "$0")"

command -v python3 >/dev/null 2>&1 || {
  echo "conformance: FAIL (python3 not found on PATH)" >&2
  exit 1
}
command -v docker >/dev/null 2>&1 || {
  echo "conformance: FAIL (docker not found on PATH)" >&2
  exit 1
}

# lib/selftest.py exercises invariants a green replay cannot see (goldens that
# are self-consistent rather than machine-specific, corpus/golden coverage not
# silently shrinking, and two concurrent runs not removing each other's
# container). It runs first because a harness that is wrong about those is not
# in a position to judge anything else.
python3 lib/selftest.py
python3 lib/replay.py
