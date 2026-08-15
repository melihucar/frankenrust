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
# The floor is NOT a number an agent hand-types into a file. It was, and two
# rounds of review on issue #58 killed that design: a floor authored from one
# worktree's count is stale the moment another worktree lands a test file on
# `main`, and it is stale in the direction that matters -- a floor of 21
# shipped while `main` already held 23, so deleting two dedicated integration
# tests still read green. `derive_min()` below computes the floor instead: the
# count of test-bearing files at `git merge-base HEAD main`, i.e. exactly "how
# many existed before this branch's own changes". The orchestrator rebases
# every worktree onto current `main` before the gate run that actually decides
# anything, and after that rebase the merge-base IS `main`'s tip, so the floor
# tracks `main` with nobody hand-editing a number, ever. Lowering it is still a
# visible thing -- it now takes deleting a test file, in the diff, rather than
# editing an integer next to it.
#
# .gate/min-test-files (an integer, alone on line 1 -- the file cannot hold a
# comment, which is why the rationale is here) still exists, but only as a
# backstop for when git cannot answer the question at all (no `main` or
# `origin/main` ref reachable -- a shallow clone, a detached snapshot). It does
# not need to track `main`; it only needs to be a number that was true once.
# Whichever of the dynamic read and the backstop is available wins; if both
# are, the larger does, so neither a stale-low backstop nor a transient git
# hiccup can pull the floor down under a healthy reading. If neither source is
# available there is no basis for a floor, and this fails closed rather than
# defaulting to 0 -- silently defaulting to 0 on a missing file is the other
# bug round 2 found here.
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

# derive_min <repo_dir> -- the floor to enforce for <repo_dir>. Prints the
# number and returns 0, or prints nothing and returns 1 if no floor can be
# established at all (caller must treat that as fail-closed, not as 0).
#
# "dynamic" is the test-file count at `git merge-base HEAD main` (falling
# back to `origin/main` for a checkout with no local `main` branch) -- the
# tree as it stood before this branch's own commits, so a branch adding tests
# is never penalised for not having predicted its own additions, but deleting
# one that already existed there always is. "static" is the
# .gate/min-test-files backstop. See the file header for why both exist and
# why the larger of the two wins.
derive_min() {
  local dir="$1" dynamic="" static_min="" base=""
  static_min="$(cat "$dir/.gate/min-test-files" 2>/dev/null)"
  base="$(git -C "$dir" merge-base HEAD main 2>/dev/null)"
  [ -n "$base" ] || base="$(git -C "$dir" merge-base HEAD origin/main 2>/dev/null)"
  if [ -n "$base" ]; then
    dynamic="$(git -C "$dir" grep -lE "$TEST_ATTR_RE" "$base" -- tests crates 2>/dev/null \
      | wc -l | tr -d " ")"
  fi
  if [ -n "$dynamic" ] && [ -n "$static_min" ]; then
    if [ "$dynamic" -ge "$static_min" ]; then echo "$dynamic"; else echo "$static_min"; fi
  elif [ -n "$dynamic" ]; then
    echo "$dynamic"
  elif [ -n "$static_min" ]; then
    echo "$static_min"
  else
    return 1
  fi
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

# git_fixture <name> -- an isolated git repo under $TMP, branch `main`, git
# identity set locally so the selftest doesn't depend on global git config
# existing (e.g. a bare CI container).
git_fixture() {
  local d="$TMP/$1"
  mkdir -p "$d/tests"
  git -C "$d" init -q -b main
  git -C "$d" config user.email "selftest@example.com"
  git -C "$d" config user.name "check_test_suite selftest"
  # git tracks no empty dirs, so top-level tests/ (required by check_tree)
  # would vanish across checkouts/rebases without a tracked file inside it.
  : > "$d/tests/.gitkeep"
  git -C "$d" add -A && git -C "$d" commit -q -m "scaffold: tests/" >/dev/null
  echo "$d"
}

# write_test_file <repo_dir> <relpath> -- a one-test file at <relpath>.
write_test_file() {
  mkdir -p "$(dirname "$1/$2")"
  printf '#[test]\nfn t() {}\n' > "$1/$2"
}

# git_commit_all <repo_dir> <message>
git_commit_all() { git -C "$1" add -A && git -C "$1" commit -q -m "$2" >/dev/null; }

assert_min() { # <label> <dir> <expected>
  local out
  if out=$(derive_min "$2" 2>&1); then
    [ "$out" = "$3" ] && sok "$1" || sbad "$1: expected $3, got $out"
  else
    sbad "$1: derive_min failed unexpectedly (wanted $3): $out"
  fi
}

assert_min_fails() { # <label> <dir>
  local out
  if out=$(derive_min "$2" 2>&1); then sbad "$1: expected derive_min to fail closed, got '$out'"
  else sok "$1"; fi
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

  # --- derive_min(): the dynamic floor (round-2 review fix) ------------------
  # These reproduce, permanently, the exact attack the round-2 reviewers found
  # by hand: a floor hand-typed from one worktree's count is already wrong the
  # moment another worktree lands a test file on `main` -- a floor of 21
  # shipped while `main` already held 23, so deleting two dedicated
  # integration tests still read green. derive_min() removes the hand-typed
  # number as the source of truth.

  d=$(git_fixture floor_dynamic)
  write_test_file "$d" crates/c/tests/a.rs
  git_commit_all "$d" "base: one test"
  assert_min "floor is the count at HEAD when there is no divergence from main" "$d" 1

  git -C "$d" checkout -q -b feature
  write_test_file "$d" crates/c/tests/b.rs
  git_commit_all "$d" "feature: add a second test"
  assert_min "a branch's own added tests don't inflate its own floor" "$d" 1

  # the reviewers' scenario: main gains tests a branch never saw, then the
  # orchestrator rebases the branch onto that main before the real gate run.
  d=$(git_fixture floor_rebase)
  write_test_file "$d" crates/c/tests/a.rs
  git_commit_all "$d" "base: one test"

  git -C "$d" checkout -q -b feature
  mkdir -p "$d/crates/c"
  printf 'unrelated feature work\n' > "$d/crates/c/NOTES.txt"
  git_commit_all "$d" "feature: unrelated change"

  git -C "$d" checkout -q main
  write_test_file "$d" crates/c/tests/added-by-other-agent-1.rs
  write_test_file "$d" crates/c/tests/added-by-other-agent-2.rs
  git_commit_all "$d" "main: two more tests land while feature is out"

  git -C "$d" checkout -q feature
  assert_min "before rebase, feature's own floor predates main's new tests" "$d" 1

  git -C "$d" rebase -q main >/dev/null
  assert_min "after the orchestrator's rebase, the floor absorbs main's new tests" "$d" 3

  rm "$d/crates/c/tests/added-by-other-agent-1.rs"
  assert_fail "post-rebase, deleting one of main's tests now trips the floor" "$d" 3 \
    "test files: 2 < required 3"

  # --- fallback: no main/origin-main ref reachable -> the static backstop ---
  d=$(git_fixture nomain)
  git -C "$d" branch -q -m main trunk
  write_test_file "$d" crates/c/tests/a.rs
  git_commit_all "$d" "base: one test, no main branch exists"
  mkdir -p "$d/.gate"
  printf '5\n' > "$d/.gate/min-test-files"
  assert_min "no main/origin-main ref: falls back to the static floor file" "$d" 5

  # --- fallback: origin/main remote-tracking ref, no local main -------------
  d=$(git_fixture originmain)
  git -C "$d" branch -q -m main trunk
  write_test_file "$d" crates/c/tests/a.rs
  git_commit_all "$d" "base: one test"
  git -C "$d" update-ref refs/remotes/origin/main "$(git -C "$d" rev-parse trunk)"
  git -C "$d" checkout -q -b feature
  write_test_file "$d" crates/c/tests/b.rs
  git_commit_all "$d" "feature: add a second test"
  assert_min "falls back to origin/main when local main is absent" "$d" 1

  # --- both sources available: the larger always wins -----------------------
  d=$(git_fixture bothsources)
  write_test_file "$d" crates/c/tests/a.rs
  write_test_file "$d" crates/c/tests/b.rs
  git_commit_all "$d" "base: two tests"
  mkdir -p "$d/.gate"
  printf '5\n' > "$d/.gate/min-test-files"
  assert_min "a higher static backstop is honoured over a lower dynamic read" "$d" 5
  printf '1\n' > "$d/.gate/min-test-files"
  assert_min "a healthy dynamic read is honoured over a lower static backstop" "$d" 2

  # --- neither source available: fail closed, not a silent 0 ----------------
  d=$(git_fixture nofloor)
  git -C "$d" branch -q -m main trunk
  write_test_file "$d" crates/c/tests/a.rs
  git_commit_all "$d" "base: one test, no main branch and no floor file"
  assert_min_fails "no derivable floor at all fails closed, not silently 0" "$d"

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

MIN="$(derive_min "$REPO_ROOT")" || {
  echo "cannot determine a test-file floor: no main/origin-main ref reachable" \
       "from HEAD and no .gate/min-test-files backstop"
  exit 1
}
check_tree "$REPO_ROOT" "$MIN"
