#!/usr/bin/env bash
# Verification gate. The orchestrator runs this in each agent's worktree and
# discards the work unless it exits 0. Agents are told to run it themselves too.
#
#   ./scripts/gate.sh [profile]
#
# Profiles:
#   bootstrap   build only — for early tasks that predate the test suite
#   default     build + clippy + fmt + unit tests + miri + conformance vs FrankenPHP
#   bench       default, plus the benchmark harness must run end to end
set -uo pipefail

PROFILE="${1:-default}"
cd "$(dirname "$0")/.." || exit 1
FAILED=()

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
# `cargo test` on a tree with no tests passes in green, so this is the only
# step that can see it. The check and the floor's rationale live in
# check_test_suite.sh; it is a separate script rather than an inline body
# because its two greps are pure pattern-matching over source text -- a pattern
# that silently matches nothing produces a *green* gate, which is exactly the
# failure two reviewers found here by hand (#58). Its negative cases run first,
# so the patterns are gate-enforced rather than eyeballed. No Rust toolchain,
# so it runs in every profile.
step "test-suite-intact" bash -c 'bash scripts/check_test_suite.sh --selftest \
  && bash scripts/check_test_suite.sh'

# The orchestrator can now merge changes to itself and restart into them, so a
# broken loop.py would end the run with no human around to restart it. This is
# the check that makes self-modification survivable: it runs in every profile,
# and it is why a syntax error, an import-time crash, or a role with no prompt
# file can never reach main.
step "orchestrator-runnable" python3 scripts/check_orchestrator.py

# docs/ cites vendor/frankenphp/<file>:<line> as its evidence for every claim
# about upstream behaviour. Those citations rot silently: a vendor bump shifts
# line numbers, a doc edit outpaces the code it describes, and nothing short
# of a reviewer re-reading the cited lines catches it. This runs both the
# checker's own negative cases and the real check against docs/, so the
# negative cases are gate-enforced rather than rotting unrun. No Rust
# toolchain, so it runs in every profile, including bootstrap, since docs rot
# before Cargo.toml exists too.
step "doc-citations" bash -c 'python3 scripts/check_doc_citations.py --selftest \
  && python3 scripts/check_doc_citations.py'

# scripts/dev.sh is the only route to a Rust toolchain, so a bug in which image
# tag or which target/ volume it picks is a bug in build, fmt, clippy and test
# at once — and it fails *green*: a worktree that reuses another worktree's
# artifacts gets a passing gate for code that was never compiled. Nothing else
# here can see that, so it is pinned directly. Pure shell against a stub
# docker: no daemon, no image, runs in every profile.
step "dev-env" bash tests/dev-env/dev-sh.test.sh

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
  # Routed through scripts/dev.sh: the host has no Rust toolchain and its PHP
  # is neither ZTS nor built with the embed SAPI this port links against, so
  # `cargo build` cannot work outside the frankenrust-dev container. dev.sh
  # itself fails loudly (not skips) if Docker is unavailable or the image
  # cannot be built, and step() below turns that failure into a gate FAIL.
  step "build" bash scripts/dev.sh cargo build --workspace --all-targets
fi

if [ "$PROFILE" != "bootstrap" ]; then
  step "fmt"     bash scripts/dev.sh cargo fmt --all -- --check
  step "clippy"  bash scripts/dev.sh cargo clippy --workspace --all-targets -- -D warnings
  step "test"    bash scripts/dev.sh cargo test --workspace

  # The one step that can see undefined behaviour. #79 shipped a
  # RequestArena::alloc that handed C a pointer carrying SharedReadOnly
  # provenance and let C write through it; build, clippy, test and conformance
  # were all green on it, because rustc happily executes that UB. Two reviewers
  # caught it and zero gate steps could have. Only an interpreter that tracks
  # provenance can, which also means the probe test #79 shipped
  # (context::tests::arena_pointers_are_writable_not_just_readable) is
  # decorative without this step: under rustc it passes whether alloc is right
  # or wrong. See issue #84.
  #
  # Why a filter and not `--lib`: Miri interprets MIR and cannot call a real
  # foreign function, so any test that reaches the C shim or libc is
  # permanently out — thread::tests re-execs the test binary via
  # std::process::Command and calls libc::sysinfo, and callbacks::* call into
  # frankenphp.c (frankenphp_init_persistent_string,
  # frankenrust_collect_server_vars). The frankenrust-sys *dependency* is not
  # the obstacle, which is the question #84 asked to settle: its build.rs
  # (bindgen + cc) runs natively under `cargo miri`, and an extern declaration
  # that is never called costs nothing, so `cargo miri test -p frankenrust-core`
  # builds and runs fine. Only the calls are impossible.
  #
  # Within context.rs the exclusions are a budget decision, measured, not a
  # correctness one — the whole module is Miri-clean. All 38 tests below take
  # 135s wall (209s of test time) on an M-series host. The four excluded
  # go_quote/go_is_print tests sweep the entire Unicode scalar range against a
  # Go oracle; under the interpreter they had not finished after 13 minutes.
  # They are pure byte/char logic with no raw pointer in sight, and `test`
  # above already runs them, so Miri learns nothing from them it does not
  # learn from the rest.
  #
  # Everything else in context.rs is included rather than allow-listed on
  # purpose: a new test in this module gets Miri coverage by default, which is
  # the behaviour worth having when #11's $_SERVER import, #12's go_read_post
  # buffers and #13's response writer all hand raw pointers to C. Extending
  # this to callbacks::* is issue #203.
  step "miri" bash scripts/dev.sh bash -c '
    set -uo pipefail
    export MIRIFLAGS=-Zmiri-strict-provenance
    log=$(mktemp)
    # The toolchain name comes from the image (docker/frankenrust-dev.Dockerfile
    # sets MIRI_TOOLCHAIN from its MIRI_NIGHTLY ARG) so the pin lives in exactly
    # one place; :? makes an image that predates it fail loudly here.
    cargo "+${MIRI_TOOLCHAIN:?image has no Miri toolchain -- rebuild frankenrust-dev}" \
      miri test -p frankenrust-core --lib -- \
      context:: --skip go_quote --skip go_is_print 2>&1 | tee "$log" || exit 1
    # Fail closed. A libtest filter that matches nothing runs zero tests and
    # exits 0, so a renamed test or a widened --skip would turn this step into
    # a 12-second no-op that reports PASS. Name the probe the whole step exists
    # for and require it to have actually run.
    grep -q "arena_pointers_are_writable_not_just_readable \.\.\. ok" "$log" || {
      echo "FAIL: miri never ran the arena provenance probe -- filter or test name changed?" >&2
      exit 1
    }
  '

  step "conformance" bash tests/conformance/run.sh
fi

if [ "$PROFILE" = "bench" ]; then
  step "bench-smoke" bash bench/harness/run.sh --smoke
fi

step "bench-report-selftest" python3 bench/harness/report.py --selftest

echo
if [ ${#FAILED[@]} -eq 0 ]; then
  echo "GATE PASS ($PROFILE)"
  exit 0
fi
echo "GATE FAIL ($PROFILE): ${FAILED[*]}"
exit 1
