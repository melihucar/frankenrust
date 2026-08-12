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
FIXED_RATE="${FIXED_RATE:-2000}"     # for the latency run; must be < saturation
WARMUP="${WARMUP:-10s}"

APPS=(noop hello json compute)
SERVERS=(frankenphp frankenrust)
SMOKE=0

while [ $# -gt 0 ]; do
  case "$1" in
    --smoke)   SMOKE=1; REPS=1; DURATION=3s; WARMUP=2s; APPS=(hello); shift ;;
    --app)     APPS=("$2"); shift 2 ;;
    --server)  SERVERS=("$2"); shift 2 ;;
    --reps)    REPS="$2"; shift 2 ;;
    *) echo "unknown arg $1"; exit 2 ;;
  esac
done

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUT="bench/results/$STAMP"
mkdir -p "$OUT"

die() { echo "FATAL: $*" >&2; exit 1; }
say() { echo "[$(date -u +%H:%M:%S)] $*"; }

# --- load generator image ----------------------------------------------------
ensure_loadgen() {
  docker image inspect "$LOADGEN_IMAGE" >/dev/null 2>&1 && return 0
  say "building load generator image (one time, ~5min)"
  docker build --platform linux/arm64 -t "$LOADGEN_IMAGE" -f "$HARNESS/Dockerfile.loadgen" "$HARNESS" \
    || die "loadgen image build failed"
}

# --- server lifecycle --------------------------------------------------------
start_server() {
  local server="$1" app="$2"
  docker rm -f "bench-$server" >/dev/null 2>&1
  case "$server" in
    frankenphp)
      docker run -d --name "bench-$server" --network "$NET" \
        --platform linux/arm64 --cpus "$SERVER_CPUS" --memory "$SERVER_MEM" \
        -v "$PWD/bench/apps/$app:/app/public:ro" \
        -v "$PWD/$HARNESS/config/Caddyfile.bench:/etc/frankenphp/Caddyfile:ro" \
        dunglas/frankenphp:latest >/dev/null ;;
    frankenrust)
      docker image inspect frankenrust:bench >/dev/null 2>&1 \
        || { say "SKIP frankenrust: image not built yet"; return 1; }
      docker run -d --name "bench-$server" --network "$NET" \
        --platform linux/arm64 --cpus "$SERVER_CPUS" --memory "$SERVER_MEM" \
        -v "$PWD/bench/apps/$app:/app/public:ro" \
        frankenrust:bench >/dev/null ;;
    *) die "unknown server $server" ;;
  esac
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
# $1 server  $2 app  $3 mode(throughput|latency)  $4 rep -> writes json
measure() {
  local server="$1" app="$2" mode="$3" rep="$4"
  local url="http://bench-$server/" extra=()
  [ "$mode" = "latency" ] && extra=(-q "$FIXED_RATE" --latency-correction)
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
    echo "duration=$DURATION connections=$CONNECTIONS reps=$REPS rate=$FIXED_RATE"
  } >> "$OUT/environment.txt"
}

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
