#!/usr/bin/env bash
# Head-to-head benchmark: official FrankenPHP vs FrankenRust.
#
#   bench/harness/run.sh --smoke            fast sanity run (used by the gate)
#   bench/harness/run.sh                    full run, writes bench/results/<ts>/
#   bench/harness/run.sh --server frankenphp --app noop
#
# Methodology notes (read these before trusting a number):
#
#  * Both servers run as linux/arm64 containers with identical --cpus and
#    --memory. The load generator runs in a THIRD container on the same docker
#    network. This matters: on Docker Desktop for Mac, host->container traffic
#    traverses the VM's userspace network proxy, which injects more latency
#    variance than the effect we are trying to measure. Keeping traffic inside
#    the VM removes that.
#  * The generator is CPU-limited too, and to a disjoint budget from the server.
#    An unpinned generator competing for cores measures the generator.
#  * Throughput and latency are measured in SEPARATE runs. A max-throughput run
#    saturates the server, so its latency percentiles are meaningless
#    (coordinated omission). The latency run uses a fixed request rate below
#    saturation, which is the only way p99 means anything.
#  * Every configuration runs $REPS times; we report the MEDIAN, plus the
#    spread. A single run on a laptop under thermal load is noise.
set -uo pipefail

cd "$(dirname "$0")/../.." || exit 1
HARNESS="bench/harness"
NET="frankenbench"
SERVER_CPUS="${SERVER_CPUS:-4}"
SERVER_MEM="${SERVER_MEM:-2g}"
LOADGEN_CPUS="${LOADGEN_CPUS:-2}"
LOADGEN_IMAGE="frankenbench/loadgen:oha"
REPS="${REPS:-5}"
DURATION="${DURATION:-30s}"
CONNECTIONS="${CONNECTIONS:-64}"
WARMUP="${WARMUP:-10s}"

APPS=(noop hello json compute)
SERVERS=(frankenphp frankenrust pasir)
SMOKE=0
SELFTEST=0

while [ $# -gt 0 ]; do
  case "$1" in
    --smoke)    SMOKE=1; REPS=1; DURATION=3s; WARMUP=2s; APPS=(hello); shift ;;
    --selftest) SELFTEST=1; shift ;;
    --app)      APPS=("$2"); shift 2 ;;
    --server)   SERVERS=("$2"); shift 2 ;;
    --reps)     REPS="$2"; shift 2 ;;
    *) echo "unknown arg $1"; exit 2 ;;
  esac
done

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUT="bench/results/$STAMP"
# --selftest exercises bake_tag only; it must not create an empty timestamped
# directory under the committed bench/results/ on every invocation (that
# litter would then get swept into a commit by merge_worktree's `git add -A`).
[ "$SELFTEST" = 1 ] || mkdir -p "$OUT"

die() { echo "FATAL: $*" >&2; exit 1; }
say() { echo "[$(date -u +%H:%M:%S)] $*"; }

# --- content-addressed image tags --------------------------------------------
# A tag keyed only on (server, app, routing) never changes, so bake() below
# reuses the same image forever even after the Dockerfile, the fixture, the
# Caddyfile, or the base image it was baked onto has changed. sha256_file/
# sha256_stdin fold that content into the tag so a changed input is a changed
# tag is a cache miss -- and BuildKit's layer cache keeps a no-op rebuild
# cheap, which is the point of doing this with a tag instead of `--no-cache`.
SHA256_BIN=""
if command -v sha256sum >/dev/null 2>&1; then
  SHA256_BIN="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  SHA256_BIN="shasum -a 256"
fi

sha256_file() {
  [ -n "$SHA256_BIN" ] || die "need sha256sum or shasum to compute a bake image tag"
  $SHA256_BIN "$1" | awk '{print $1}'
}

sha256_stdin() {
  [ -n "$SHA256_BIN" ] || die "need sha256sum or shasum to compute a bake image tag"
  $SHA256_BIN | awk '{print $1}'
}

# bake_tag <server> <routing> <base_id> <app_dir> <dockerfile> [caddyfile]
#
# Pure function of its arguments and the files they name: no docker, no
# globals, so --selftest can exercise it against mktemp fixtures without a
# daemon. base_id is an opaque string (a docker image ID, or a sentinel) --
# this only cares that a changed base produces a changed tag, not what
# produced it. Files under app_dir are hashed by path RELATIVE to app_dir, so
# the tag depends on content and layout, never on where the repo (or a
# --selftest fixture under mktemp -d) happens to be checked out.
bake_tag() {
  local server="$1" routing="$2" base_id="$3" app_dir="${4%/}" dockerfile="$5" caddyfile="${6:-}"
  local app digest nfiles

  # Validate up front, and loudly. A digest is only trustworthy if every input
  # it claims to cover was actually read: hashing zero files under a mistyped
  # app_dir would otherwise yield a perfectly stable, perfectly wrong tag, and
  # the whole point of this function is that the tag cannot lie about content.
  # Diagnostics go to stderr -- bake() captures our stdout as the tag.
  [ -d "$app_dir" ]    || { echo "bake_tag: not a directory: $app_dir" >&2; return 1; }
  [ -f "$dockerfile" ] || { echo "bake_tag: not a file: $dockerfile" >&2; return 1; }
  [ -z "$caddyfile" ] || [ -f "$caddyfile" ] ||
    { echo "bake_tag: not a file: $caddyfile" >&2; return 1; }
  nfiles="$(LC_ALL=C find "$app_dir" -type f | wc -l | tr -d '[:space:]')"
  [ "${nfiles:-0}" -gt 0 ] || { echo "bake_tag: no files under $app_dir" >&2; return 1; }

  app="$(basename "$app_dir")"
  digest="$(
    {
      printf 'server=%s\n' "$server"
      printf 'routing=%s\n' "$routing"
      printf 'base_id=%s\n' "$base_id"
      local f rel sha
      while IFS= read -r f; do
        # `exit` and not `return`: the pipe puts this group in a subshell, so a
        # nonzero exit here becomes the pipeline's status under `pipefail` and
        # trips the `|| return 1` below. Never let an unhashed file pass.
        sha="$(sha256_file "$f")" || exit 1
        rel="${f#"$app_dir"/}"
        printf 'file=%s sha=%s\n' "$rel" "$sha"
      done < <(LC_ALL=C find "$app_dir" -type f | LC_ALL=C sort)
      sha="$(sha256_file "$dockerfile")" || exit 1
      printf 'dockerfile=%s\n' "$sha"
      # `if`, not `[ -n ... ] &&`: as the group's last command a false test
      # would make the group exit 1, and `pipefail` would turn that into a
      # failed pipeline -- so bake_tag would fail for exactly the callers that
      # pass no caddyfile (frankenrust and pasir). An `if` with no else is 0.
      if [ -n "$caddyfile" ]; then
        sha="$(sha256_file "$caddyfile")" || exit 1
        printf 'caddyfile=%s\n' "$sha"
      fi
    } | sha256_stdin
  )" || return 1
  [ -n "$digest" ] || { echo "bake_tag: empty digest for $app_dir" >&2; return 1; }
  printf 'benchimg-%s-%s-%s-%s\n' "$server" "$app" "$routing" "$digest"
}

# --- load generator image ----------------------------------------------------
ensure_loadgen() {
  local digest
  digest="$(sha256_file "$HARNESS/Dockerfile.loadgen")" || return 1
  LOADGEN_IMAGE="frankenbench/loadgen:oha-$digest"
  docker image inspect "$LOADGEN_IMAGE" >/dev/null 2>&1 && return 0
  say "building load generator image (one time, ~5min)"
  docker build --platform linux/arm64 -t "$LOADGEN_IMAGE" -f "$HARNESS/Dockerfile.loadgen" "$HARNESS" \
    || die "loadgen image build failed"
}

# --- server lifecycle --------------------------------------------------------
# Bake the fixture into the image and echo the tag.
#
# Previously the apps were bind-mounted from macOS. Docker Desktop serves those
# over VirtioFS, and any server that stat()s the document root per request pays
# a large tax that a server which does not stat never sees. Measured on the same
# FrankenPHP build and fixture: 3,248 rps bind-mounted vs 21,317 rps baked --
# a 6.6x artifact, larger than any effect this project is trying to detect.
ROUTING="${ROUTING:-default}"        # default (php_server/try_files) | matched
# bake()'s builds use `bench` as the context (see the .dockerignore comment
# and Dockerfile.frankenphp's CADDYFILE arg), so HARNESS-relative paths need
# rewriting relative to that context too. HARNESS is "bench/harness"; strip
# the leading "bench/" once here rather than in every --build-arg below.
HARNESS_CTX="${HARNESS#bench/}"
bake() {
  local server="$1" app="$2"
  local app_dir="bench/apps/$app" app_ctx="apps/$app"
  local base_id tag dockerfile caddyfile=""
  case "$server" in
    frankenphp)
      # dunglas/frankenphp:latest may legitimately not be pulled yet on a
      # fresh host -- unlike the frankenrust/pasir bases below, which this
      # harness itself is responsible for building, that is not a reason to
      # skip. The sentinel keeps bake_tag callable regardless; the tag still
      # changes (and one rebuild happens) the moment the real ID is known,
      # which is the whole point of folding the base image into the tag.
      base_id="$(docker image inspect -f '{{.Id}}' dunglas/frankenphp:latest 2>/dev/null)"
      [ -n "$base_id" ] || base_id="unpulled"
      local cf_name="Caddyfile.bench"
      [ "$ROUTING" = "matched" ] && cf_name="Caddyfile.matched"
      dockerfile="$HARNESS/Dockerfile.frankenphp"
      caddyfile="$HARNESS/config/$cf_name"
      tag="$(bake_tag "$server" "$ROUTING" "$base_id" "$app_dir" "$dockerfile" "$caddyfile")" || return 1
      docker image inspect "$tag" >/dev/null 2>&1 && { echo "$tag"; return 0; }
      docker build --platform linux/arm64 -t "$tag" \
        --build-arg BASE=dunglas/frankenphp:latest \
        --build-arg APP="$app_ctx" --build-arg CADDYFILE="$HARNESS_CTX/config/$cf_name" \
        -f "$dockerfile" bench >/dev/null 2>&1 || return 1 ;;
    pasir|frankenrust)
      docker image inspect "$server:bench" >/dev/null 2>&1 || return 1
      base_id="$(docker image inspect -f '{{.Id}}' "$server:bench" 2>/dev/null)"
      dockerfile="$HARNESS/Dockerfile.app"
      tag="$(bake_tag "$server" "$ROUTING" "$base_id" "$app_dir" "$dockerfile")" || return 1
      docker image inspect "$tag" >/dev/null 2>&1 && { echo "$tag"; return 0; }
      docker build --platform linux/arm64 -t "$tag" \
        --build-arg BASE="$server:bench" --build-arg APP="$app_ctx" \
        -f "$dockerfile" bench >/dev/null 2>&1 || return 1 ;;
    *) return 1 ;;
  esac
  echo "$tag"
}

start_server() {
  local server="$1" app="$2" tag
  docker rm -f "bench-$server" >/dev/null 2>&1
  tag=$(bake "$server" "$app") || { say "SKIP $server: image not available"; return 1; }
  # FR_THREADS equalises the PHP concurrency budget across servers. Without it
  # FrankenPHP runs a fixed 8 interpreters while a spawn_blocking-based server
  # can have 64+ in flight at -c 64, which measures pool sizing, not runtime.
  docker run -d --name "bench-$server" --network "$NET" \
    --platform linux/arm64 --cpus "$SERVER_CPUS" --memory "$SERVER_MEM" \
    -e FR_THREADS="${FR_THREADS:-8}" \
    "$tag" >/dev/null || return 1
  for _ in $(seq 1 60); do
    if docker run --rm --network "$NET" "$LOADGEN_IMAGE" \
         oha -n 1 --no-tui "http://bench-$server/" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  say "!! $server failed health check on $app"
  docker logs "bench-$server" 2>&1 | tail -30
  return 1
}

stop_server() { docker rm -f "bench-$1" >/dev/null 2>&1; }

# --- measurement -------------------------------------------------------------
# Derive the latency run's fixed rate from THIS server+app's measured throughput.
#
# A fixed rate above saturation is not a latency measurement, it is a queueing
# measurement: the generator keeps offering load the server cannot retire, the
# backlog grows without bound, and every percentile just reports how long the
# run lasted. The first version of this harness hardcoded 1500 req/s, and the
# CPU-bound app -- which saturates near 566 rps -- duly reported a p50 of 4.6
# SECONDS. That number described the queue, not the server.
#
# 50% of measured max keeps the server below the knee, which is the regime where
# p99 reflects the request path. Throughput therefore MUST run before latency.
LATENCY_LOAD_FACTOR="${LATENCY_LOAD_FACTOR:-0.5}"
latency_rate() {
  python3 - "$OUT" "$1" "$2" "$LATENCY_LOAD_FACTOR" <<'PY'
import glob, json, statistics, sys
out, server, app, factor = sys.argv[1], sys.argv[2], sys.argv[3], float(sys.argv[4])
vals = []
for f in glob.glob(f"{out}/{server}.{app}.throughput.*.json"):
    try:
        vals.append(json.load(open(f))["summary"]["requestsPerSec"])
    except Exception:
        pass
# No throughput data (ordering bug) -> fall back rather than crash the whole run.
print(max(1, int(statistics.median(vals) * factor)) if vals else 500)
PY
}

# $1 server  $2 app  $3 mode(throughput|latency)  $4 rep -> writes json
measure() {
  local server="$1" app="$2" mode="$3" rep="$4"
  local url="http://bench-$server/" extra=()
  [ "$mode" = "latency" ] && extra=(-q "$(latency_rate "$server" "$app")" --latency-correction)
  docker run --rm --network "$NET" --cpus "$LOADGEN_CPUS" "$LOADGEN_IMAGE" \
    oha -z "$DURATION" -c "$CONNECTIONS" --no-tui --output-format json "${extra[@]}" "$url" \
    > "$OUT/$server.$app.$mode.$rep.json" 2>"$OUT/$server.$app.$mode.$rep.err"
}

# --- main --------------------------------------------------------------------
# --- preflight: a benchmark run on a busy machine measures the machine --------
# This box is a working laptop with other project stacks (databases, queue
# workers) running in Docker. Those compete for the same VM CPU allocation and
# will quietly skew results. Warn loudly and record what was running, so a
# surprising number can be explained later instead of believed.
preflight() {
  local noisy
  noisy=$(docker ps --format '{{.Names}}' | grep -v '^bench-' || true)
  if [ -n "$noisy" ]; then
    echo "!! WARNING: other containers are running and will contend for CPU:"
    echo "$noisy" | sed 's/^/     /'
    echo "!! Results will be noisier than they should be. Stop them for a clean run."
    echo "$noisy" > "$OUT/CONTAMINATION.txt"
    [ "${BENCH_ALLOW_NOISY:-0}" = "1" ] || {
      echo "!! Set BENCH_ALLOW_NOISY=1 to proceed anyway."; exit 3; }
  fi
  uname -a > "$OUT/environment.txt"
  sysctl -n machdep.cpu.brand_string hw.ncpu hw.memsize >> "$OUT/environment.txt" 2>/dev/null
  docker version --format '{{.Server.Version}}' >> "$OUT/environment.txt" 2>/dev/null
  {
    echo "server_cpus=$SERVER_CPUS loadgen_cpus=$LOADGEN_CPUS mem=$SERVER_MEM"
    echo "duration=$DURATION connections=$CONNECTIONS reps=$REPS latency_load_factor=$LATENCY_LOAD_FACTOR"
  } >> "$OUT/environment.txt"
}

# --- --selftest ---------------------------------------------------------------
# Nothing in any gate profile reads bench/harness/, and bench-smoke couldn't
# catch a stale-tag regression even if it did: a stale image starts fine,
# passes the health check, and serves requests. A wrong-but-working image IS
# the failure mode. This exercises bake_tag against mktemp fixtures with
# synthetic base IDs, entirely without Docker.
SELFTEST_OK=1
st() {
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then
    echo "ok - $desc"
  else
    echo "FAIL - $desc"
    SELFTEST_OK=0
  fi
}

# st's inverse: assert a command FAILS. bake_tag returning a plausible tag for
# inputs it could not read is as bad as returning none for inputs it could.
st_fails() {
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then
    echo "FAIL - $desc"
    SELFTEST_OK=0
  else
    echo "ok - $desc"
  fi
}

run_selftest() {
  SELFTEST_OK=1
  local base1="sha256:1111111111111111111111111111111111111111111111111111111111111111"
  local base2="sha256:2222222222222222222222222222222222222222222222222222222222222222"
  local d1 d2
  d1="$(mktemp -d)" || die "mktemp failed"
  d2="$(mktemp -d)" || die "mktemp failed"

  mkdir -p "$d1/app/sub" "$d2/app/sub" "$d1/emptyapp"
  printf 'hello world\n'  > "$d1/app/index.php"
  printf 'hello world\n'  > "$d2/app/index.php"
  printf 'nested\n'       > "$d1/app/sub/inc.php"
  printf 'nested\n'       > "$d2/app/sub/inc.php"
  printf 'FROM scratch\nCOPY . /app\n' > "$d1/Dockerfile"
  printf 'FROM scratch\nCOPY . /app\n' > "$d2/Dockerfile"
  printf ':80 {\n\troot /app\n}\n'     > "$d1/Caddyfile"
  printf ':80 {\n\troot /app\n}\n'     > "$d2/Caddyfile"

  local t_d1 t_d2
  t_d1="$(bake_tag frankenphp default "$base1" "$d1/app" "$d1/Dockerfile" "$d1/Caddyfile")"
  t_d2="$(bake_tag frankenphp default "$base1" "$d2/app" "$d2/Dockerfile" "$d2/Caddyfile")"

  st "produces a non-empty tag" test -n "$t_d1"
  st "identical inputs in two different temp dirs produce an identical tag" \
    test "$t_d1" = "$t_d2"

  printf 'hello world!\n' > "$d1/app/index.php"
  st "a changed byte in a file under app_dir changes the tag" \
    test "$(bake_tag frankenphp default "$base1" "$d1/app" "$d1/Dockerfile" "$d1/Caddyfile")" != "$t_d1"
  printf 'hello world\n' > "$d1/app/index.php"

  printf 'extra\n' > "$d1/app/extra.php"
  st "an added file under app_dir changes the tag" \
    test "$(bake_tag frankenphp default "$base1" "$d1/app" "$d1/Dockerfile" "$d1/Caddyfile")" != "$t_d1"
  rm -f "$d1/app/extra.php"

  rm -f "$d1/app/sub/inc.php"
  st "a removed file under app_dir changes the tag" \
    test "$(bake_tag frankenphp default "$base1" "$d1/app" "$d1/Dockerfile" "$d1/Caddyfile")" != "$t_d1"
  printf 'nested\n' > "$d1/app/sub/inc.php"

  st "app_dir restored byte-for-byte reproduces the original tag" \
    test "$(bake_tag frankenphp default "$base1" "$d1/app" "$d1/Dockerfile" "$d1/Caddyfile")" = "$t_d1"

  printf 'FROM scratch\nCOPY . /app2\n' > "$d1/Dockerfile"
  st "a changed dockerfile changes the tag" \
    test "$(bake_tag frankenphp default "$base1" "$d1/app" "$d1/Dockerfile" "$d1/Caddyfile")" != "$t_d1"
  printf 'FROM scratch\nCOPY . /app\n' > "$d1/Dockerfile"

  printf ':80 {\n\troot /app2\n}\n' > "$d1/Caddyfile"
  st "a changed caddyfile changes the tag" \
    test "$(bake_tag frankenphp default "$base1" "$d1/app" "$d1/Dockerfile" "$d1/Caddyfile")" != "$t_d1"
  printf ':80 {\n\troot /app\n}\n' > "$d1/Caddyfile"

  st "a changed base_id changes the tag (the #15 axis)" \
    test "$(bake_tag frankenphp default "$base2" "$d1/app" "$d1/Dockerfile" "$d1/Caddyfile")" != "$t_d1"

  st "a different server produces a different tag" \
    test "$(bake_tag frankenrust default "$base1" "$d1/app" "$d1/Dockerfile" "$d1/Caddyfile")" != "$t_d1"

  st "a different routing produces a different tag" \
    test "$(bake_tag frankenphp matched "$base1" "$d1/app" "$d1/Dockerfile" "$d1/Caddyfile")" != "$t_d1"

  st "the tag is a legal docker tag (charset [a-z0-9._-])" \
    bash -c '[[ "$1" =~ ^[a-z0-9._-]+$ ]]' _ "$t_d1"
  st "the tag is under 128 chars" test "${#t_d1}" -lt 128

  # The five-argument form. bake() calls bake_tag WITHOUT a caddyfile for
  # frankenrust and pasir (Dockerfile.app COPYs no Caddyfile), so two of the
  # three servers ride this path exclusively -- and a bake_tag that fails here
  # is invisible: start_server downgrades it to "SKIP: image not available"
  # and the run reports a FrankenPHP-only comparison as a routine skip.
  local t_nocf
  st "bake_tag exits 0 with no caddyfile arg (the frankenrust/pasir form)" \
    bake_tag frankenrust default "$base1" "$d1/app" "$d1/Dockerfile"
  t_nocf="$(bake_tag frankenrust default "$base1" "$d1/app" "$d1/Dockerfile" 2>/dev/null)"
  st "the no-caddyfile form produces a non-empty tag" test -n "$t_nocf"
  st "the no-caddyfile form is byte-stable" \
    test "$(bake_tag frankenrust default "$base1" "$d1/app" "$d1/Dockerfile" 2>/dev/null)" = "$t_nocf"
  st "the no-caddyfile form still tracks base_id (the #15 axis)" \
    test "$(bake_tag frankenrust default "$base2" "$d1/app" "$d1/Dockerfile" 2>/dev/null)" != "$t_nocf"
  st "omitting the caddyfile produces a different tag than supplying one" \
    test "$t_nocf" != "$(bake_tag frankenrust default "$base1" "$d1/app" "$d1/Dockerfile" "$d1/Caddyfile" 2>/dev/null)"

  # Unreadable inputs must fail, not hash to a stable tag over nothing.
  st_fails "a missing app_dir fails rather than yielding a tag" \
    bake_tag frankenphp default "$base1" "$d1/nosuchapp" "$d1/Dockerfile" "$d1/Caddyfile"
  st_fails "an empty app_dir fails rather than yielding a tag" \
    bake_tag frankenphp default "$base1" "$d1/emptyapp" "$d1/Dockerfile" "$d1/Caddyfile"
  st_fails "a missing dockerfile fails rather than yielding a tag" \
    bake_tag frankenphp default "$base1" "$d1/app" "$d1/nosuch.Dockerfile" "$d1/Caddyfile"
  st_fails "a missing caddyfile fails rather than yielding a tag" \
    bake_tag frankenphp default "$base1" "$d1/app" "$d1/Dockerfile" "$d1/nosuch.Caddyfile"

  rm -rf "$d1" "$d2"

  echo
  if [ "$SELFTEST_OK" = 1 ]; then
    echo "SELFTEST PASS"
  else
    echo "SELFTEST FAIL"
  fi
  [ "$SELFTEST_OK" = 1 ]
}

if [ "$SELFTEST" = 1 ]; then
  run_selftest
  exit $?
fi

docker network inspect "$NET" >/dev/null 2>&1 || docker network create "$NET" >/dev/null
preflight
ensure_loadgen

for app in "${APPS[@]}"; do
  for server in "${SERVERS[@]}"; do
    say "=== $server / $app"
    start_server "$server" "$app" || continue

    say "  warmup ($WARMUP) — lets the JIT/opcache and allocator settle"
    docker run --rm --network "$NET" --cpus "$LOADGEN_CPUS" "$LOADGEN_IMAGE" \
      oha -z "$WARMUP" -c "$CONNECTIONS" --no-tui "http://bench-$server/" >/dev/null 2>&1

    for mode in throughput latency; do
      [ "$SMOKE" = 1 ] && [ "$mode" = latency ] && continue
      for rep in $(seq 1 "$REPS"); do
        say "  $mode rep $rep/$REPS"
        measure "$server" "$app" "$mode" "$rep"
      done
    done

    docker stats --no-stream --format '{{.MemUsage}} {{.CPUPerc}}' "bench-$server" \
      > "$OUT/$server.$app.resources.txt" 2>/dev/null
    stop_server "$server"
  done
done

python3 "$HARNESS/report.py" "$OUT" | tee "$OUT/REPORT.md"
say "results -> $OUT/REPORT.md"
