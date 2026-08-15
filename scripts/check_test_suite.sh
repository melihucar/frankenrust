#!/usr/bin/env bash
# Gate check: the Rust test suite is still there.
#
# An agent that deletes tests must not get a green gate. Nothing else in
# gate.sh can see that: `cargo test` on a tree with no tests passes, loudly and
# in green. So this counts the files that hold test functions and refuses to
# let that number fall below a floor, and rejects `#[ignore]` without a written
# justification.
#
#   bash scripts/check_test_suite.sh            check the real tree
#   bash scripts/check_test_suite.sh --selftest run the negative cases below
#
# The floor lives in .gate/min-test-files (an integer, alone on line 1 -- the
# file cannot hold a comment, which is why the rationale is here). It is set to
# the exact current count, not a slack value: slack is indistinguishable from
# permission to delete that many files. Of the 21 counted files, 15 are
# crates/*/src/*.rs carrying `#[cfg(test)] mod tests` -- those already have to
# exist for the workspace to compile, so the `build` step protects them and
# they contribute no real coverage to this floor. The other 6 are the
# workspace's only dedicated integration tests (frankenrust-sys tests/version,
# tests/zval_tags; frankenrust-core tests/abort_stub; frankenrust-server
# tests/hello_world, tests/keepalive_shutdown, tests/concurrency), and they are
# protected only by the exactness of this number: any floor below 21 is a floor
# that lets one of them be deleted. Raise it when tests are added. Lowering it
# is a visible line in a diff, which is the point -- a legitimate consolidation
# should have to say so out loud.
#
# Rust lives under crates/*/src and crates/*/tests; there is no top-level src/
# (top-level tests/ is the shell conformance and dev-env suites). Both scans
# below therefore cover `tests crates`.
set -uo pipefail

# Counting pattern. Anchored to the start of a line so that prose *about* a
# test attribute is not counted as a test: crates/frankenrust-sys/src/layout.rs
# explains at length that its ABI assertions are `const _: () = ...` blocks
# rather than `#[test]`, and an unanchored scan counted it as a test file that
# does not exist. A phantom is not harmless here -- it inflates the count the
# floor is set from, so the floor ends up protecting a file that cannot be
# deleted instead of one that can. `(\]|\()` after `test` is what admits
# `#[tokio::test(flavor = "multi_thread", worker_threads = 8)]`; requiring `]`
# matched the bare forms only and made tests/concurrency.rs invisible.
TEST_ATTR_RE='^[[:space:]]*#\[(tokio::)?test(\]|\()'

# Rejection pattern, deliberately NOT anchored. Opposite trade to the count:
# here a false positive is a gate failure someone reads and annotates, while a
# false negative silently disarms an assertion, so this scans whole lines.
# `(\]|=|[[:space:]])` is what admits `#[ignore = "flaky, will fix later"]`,
# which is the natural phrasing and matched nothing when the pattern was
# `#[ignore]`. It does not match `#[ignored_...]`.
IGNORE_RE='#\[ignore(\]|=|[[:space:]])'

# check_tree <dir> <min> -- the check itself, run against a tree rooted at
# <dir>. Parameterised only so --selftest can point it at fixtures; the real
# run passes the repo root. Diagnostics go to stdout, exit status is the
# verdict.
check_tree() {
  local dir="$1" min="$2"
  (
    cd "$dir" || exit 1
    [ -d tests ] || { echo "tests/ is missing"; exit 1; }
    n=$(grep -rlE "$TEST_ATTR_RE" tests crates 2>/dev/null | wc -l | tr -d " ")
    [ "$n" -ge "$min" ] || { echo "test files: $n < required $min"; exit 1; }
    ! grep -rnE "$IGNORE_RE" tests crates 2>/dev/null | grep -v "GATE-OK" || {
      echo "found #[ignore] without a GATE-OK justification"; exit 1; }
  )
}

# ---------------------------------------------------------------- self-test --
#
# These are the cases two reviewers found by hand on issue #58 after the check
# had been shipped and passing. The patterns above are the load-bearing part of
# this gate step and they are invisible from a normal run -- a pattern that
# matches nothing produces a *green* gate -- so they are pinned here, running
# against whatever grep the gate actually invokes (BSD on the macOS host, GNU
# in the container), which is also what keeps the bracket/alternation syntax
# honest across the two.

SELFTEST_PASSED=0
SELFTEST_FAILED=0

sok()  { SELFTEST_PASSED=$((SELFTEST_PASSED + 1)); [ -n "${SELFTEST_VERBOSE:-}" ] && echo "    ok   $1"; return 0; }
sbad() { SELFTEST_FAILED=$((SELFTEST_FAILED + 1)); echo "    FAIL selftest: $1"; }

# fixture <name> -- make an empty tests/ + crates/ tree under $TMP, echo path.
fixture() {
  local d="$TMP/$1"
  mkdir -p "$d/tests" "$d/crates/c/src" "$d/crates/c/tests"
  echo "$d"
}

assert_pass() { # <label> <dir> <min>
  local out
  if out=$(check_tree "$2" "$3" 2>&1); then sok "$1"
  else sbad "$1: expected pass, got fail: $out"; fi
}

assert_fail() { # <label> <dir> <min> <expected substring of message>
  local out
  if out=$(check_tree "$2" "$3" 2>&1); then sbad "$1: expected fail, but it passed"
  elif [ -n "${4:-}" ] && ! printf '%s' "$out" | grep -qF "$4"; then
    sbad "$1: failed, but message lacked '$4': $out"
  else sok "$1"; fi
}

# count_in <dir> -- what check_tree would count, for the counting assertions.
count_in() { (cd "$1" && grep -rlE "$TEST_ATTR_RE" tests crates 2>/dev/null | wc -l | tr -d " "); }

assert_count() { # <label> <dir> <expected>
  local n; n=$(count_in "$2")
  [ "$n" = "$3" ] && sok "$1" || sbad "$1: expected count $3, got $n"
}

run_selftest() {
  TMP="$(cd "$(mktemp -d)" && pwd -P)"
  trap 'rm -rf "$TMP"' EXIT

  # --- the floor actually holds ---------------------------------------------
  local d
  d=$(fixture floor)
  printf '#[test]\nfn a() {}\n' > "$d/crates/c/tests/a.rs"
  printf '#[test]\nfn b() {}\n' > "$d/crates/c/tests/b.rs"
  assert_count "two test files count as two" "$d" 2
  assert_pass  "count at the floor passes" "$d" 2
  assert_fail  "count below the floor fails" "$d" 3 "test files: 2 < required 3"
  rm "$d/crates/c/tests/b.rs"
  assert_fail  "deleting a test file trips the floor" "$d" 2 "test files: 1 < required 2"

  # --- parameterised #[tokio::test(...)] is counted (reviewer 2, finding 1) --
  # A file whose only test is `#[tokio::test(flavor = ..., worker_threads = N)]`
  # -- the shape of crates/frankenrust-server/tests/concurrency.rs -- was
  # invisible, so the floor could not protect it at any value.
  d=$(fixture tokio_args)
  printf '#[tokio::test(flavor = "multi_thread", worker_threads = 8)]\nasync fn a() {}\n' \
    > "$d/crates/c/tests/conc.rs"
  assert_count "parameterised #[tokio::test(..)] is counted" "$d" 1
  printf '#[tokio::test]\nasync fn b() {}\n' > "$d/crates/c/tests/plain.rs"
  assert_count "bare #[tokio::test] is counted" "$d" 2

  # --- prose about a test attribute is not counted --------------------------
  # crates/frankenrust-sys/src/layout.rs holds no test, only a doc comment
  # saying why; counting it inflated the number the floor is derived from.
  d=$(fixture prose)
  printf '//! Every assertion is a `const _: () = ...`, not a `#[test]`.\npub const X: u8 = 1;\n' \
    > "$d/crates/c/src/layout.rs"
  assert_count "a doc comment naming #[test] is not a test file" "$d" 0
  printf '#[cfg(test)]\nmod tests {}\n' > "$d/crates/c/src/other.rs"
  assert_count "#[cfg(test)] alone is not a test file" "$d" 0
  printf '#[cfg(test)]\nmod tests {\n    #[test]\n    fn a() {}\n}\n' > "$d/crates/c/src/real.rs"
  assert_count "an indented #[test] inside cfg(test) is counted" "$d" 1

  # --- #[ignore] rejection (reviewer 2, finding 2) --------------------------
  # `#[ignore = "reason"]` disables a test exactly as `#[ignore]` does and is
  # the phrasing an agent under gate pressure reaches for.
  d=$(fixture ign)
  printf '#[test]\nfn a() {}\n' > "$d/crates/c/tests/a.rs"
  assert_pass "clean tree has no #[ignore] complaint" "$d" 1
  printf '#[test]\n#[ignore]\nfn b() {}\n' > "$d/crates/c/tests/b.rs"
  assert_fail "bare #[ignore] is rejected" "$d" 1 "found #[ignore] without a GATE-OK"
  printf '#[test]\n#[ignore = "flaky under CI, will fix later"]\nfn b() {}\n' > "$d/crates/c/tests/b.rs"
  assert_fail 'literal #[ignore = "reason"] is rejected' "$d" 1 "found #[ignore] without a GATE-OK"
  printf '#[test]\n#[ignore="x"]\nfn b() {}\n' > "$d/crates/c/tests/b.rs"
  assert_fail "#[ignore=\"x\"] without spaces is rejected" "$d" 1 "found #[ignore] without a GATE-OK"
  printf '#[test]\n#[ignore] // GATE-OK: needs a live PHP build, see #99\nfn b() {}\n' \
    > "$d/crates/c/tests/b.rs"
  assert_pass "#[ignore] justified with GATE-OK on the same line passes" "$d" 1
  printf '#[test]\nfn b() { let ignored_marker = 1; }\n' > "$d/crates/c/tests/b.rs"
  assert_pass "an identifier containing 'ignore' is not an attribute" "$d" 2

  # --- the tree itself vanishing ---------------------------------------------
  d=$(fixture gone)
  printf '#[test]\nfn a() {}\n' > "$d/crates/c/tests/a.rs"
  rmdir "$d/tests"
  assert_fail "a missing tests/ dir fails" "$d" 1 "tests/ is missing"
  d=$(fixture nocrates)
  rm -rf "$d/crates"
  assert_fail "a missing crates/ dir cannot reach a nonzero floor" "$d" 1 "test files: 0"

  if [ "$SELFTEST_FAILED" -eq 0 ]; then
    echo "SELFTEST PASS ($SELFTEST_PASSED cases)"
    return 0
  fi
  echo "SELFTEST FAIL ($SELFTEST_FAILED of $((SELFTEST_PASSED + SELFTEST_FAILED)) cases)"
  return 1
}

# ---------------------------------------------------------------------- main --

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd -P)"

if [ "${1:-}" = "--selftest" ]; then
  run_selftest
  exit $?
fi

MIN="$(cat "$REPO_ROOT/.gate/min-test-files" 2>/dev/null || echo 0)"
check_tree "$REPO_ROOT" "$MIN"
