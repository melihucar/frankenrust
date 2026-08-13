#!/usr/bin/env bash
# Verification gate. The orchestrator runs this in each agent's worktree and
# discards the work unless it exits 0. Agents are told to run it themselves too.
#
#   ./scripts/gate.sh [profile]
#
# Profiles:
#   bootstrap   build only — for early tasks that predate the test suite
#   default     build + clippy + fmt + unit tests + conformance vs FrankenPHP
#   bench       default, plus the benchmark harness must run end to end
set -uo pipefail

PROFILE="${1:-default}"
cd "$(dirname "$0")/.." || exit 1
FAILED=()

# rustup installs cargo to ~/.cargo/bin and leaves it to an interactive shell
# profile to put that on PATH. The orchestrator is not an interactive shell and
# neither are the agents it spawns, so without this every cargo step fails as
# "command not found" and the gate reports it as the agent's code being broken.
command -v cargo >/dev/null 2>&1 || export PATH="$HOME/.cargo/bin:$PATH"

step() {
  local name="$1"; shift
  echo "--- gate: $name"
  if "$@"; then
    echo "    PASS $name"
  else
    echo "    FAIL $name"
    FAILED+=("$name")
  fi
}

# Fail closed: an agent that deletes the test suite must not get a green gate.
step "test-suite-intact" bash -c '
  [ -d tests ] || { echo "tests/ is missing"; exit 1; }
  n=$(grep -rl "#\[test\]\|#\[tokio::test\]" tests src 2>/dev/null | wc -l | tr -d " ")
  min=$(cat .gate/min-test-files 2>/dev/null || echo 0)
  [ "$n" -ge "$min" ] || { echo "test files: $n < required $min"; exit 1; }
  ! grep -rn "#\[ignore\]" tests src 2>/dev/null | grep -v "GATE-OK" || {
    echo "found #[ignore] without a GATE-OK justification"; exit 1; }
'

# The orchestrator can now merge changes to itself and restart into them, so a
# broken loop.py would end the run with no human around to restart it. This is
# the check that makes self-modification survivable: it runs in every profile,
# and it is why a syntax error, an import-time crash, or a role with no prompt
# file can never reach main.
step "orchestrator-runnable" python3 scripts/check_orchestrator.py

# Early backlog tasks (the PHP base image, the conformance harness) legitimately
# produce no Rust. Tolerate a missing workspace ONLY in bootstrap; in any other
# profile a vanished Cargo.toml means someone deleted the project.
if [ ! -f Cargo.toml ]; then
  if [ "$PROFILE" = "bootstrap" ]; then
    echo "--- gate: build SKIPPED (no Cargo.toml yet — pre-workspace task)"
  else
    echo "--- gate: build"; echo "    FAIL build (Cargo.toml missing in profile $PROFILE)"
    FAILED+=("build")
  fi
else
  # Routed through scripts/dev.sh: the host has no PHP embed SAPI and no Rust
  # toolchain (see docker/frankenrust-dev.Dockerfile), so a bare `cargo build`
  # here cannot link -lphp. If Docker is unavailable or the image fails to
  # build, dev.sh exits nonzero and this step FAILS — it must never be
  # allowed to silently skip.
  step "build" scripts/dev.sh cargo build --workspace --all-targets
fi

if [ "$PROFILE" != "bootstrap" ]; then
  step "fmt"     scripts/dev.sh cargo fmt --all -- --check
  step "clippy"  scripts/dev.sh cargo clippy --workspace --all-targets -- -D warnings
  step "test"    scripts/dev.sh cargo test --workspace
  step "conformance" bash tests/conformance/run.sh
fi

if [ "$PROFILE" = "bench" ]; then
  step "bench-smoke" bash bench/harness/run.sh --smoke
fi

echo
if [ ${#FAILED[@]} -eq 0 ]; then
  echo "GATE PASS ($PROFILE)"
  exit 0
fi
echo "GATE FAIL ($PROFILE): ${FAILED[*]}"
exit 1
