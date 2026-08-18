#!/usr/bin/env bash
# Keep the loop lanes alive for a fixed total budget, and no longer.
#
# `python3 orchestrator/loop.py <lane>` stops for two very different reasons:
# it finished its wallclock, or it died. Only the second one should be
# restarted, and neither is distinguishable from the outside -- both are just
# an exit.
#
# So this owns the budget instead of the loops. It computes one absolute
# deadline up front and hands each successor only the time that is actually
# left. That is the whole point: a `while true` wrapper restarting a dead loop
# would hand it a fresh FR_WALLCLOCK every time, so an 8h run that crashes
# hourly runs forever. #89 recorded the manual version of that mistake -- a
# cold restart at 08:41 gave itself another full 8h on top of the 6h already
# spent -- and a supervisor is only worth having if it does not automate it.
#
# The lanes run concurrently, each with its own log: `run` drains the queue,
# `unblock` rescues fr:blocked issues (recovery used to run inline in the
# claim loop, walking the whole blocked queue in one call and suspending
# claims -- and the wallclock check -- for the duration). Shared state is
# GitHub labels alone, so concurrency is the point, not a hazard.
#
#   scripts/supervise.sh                       # 8h total, run lane only
#   scripts/supervise.sh run unblock           # both lanes, same budget
#   FR_TOTAL=3600 scripts/supervise.sh run unblock
#
# Note the loop re-execs itself into merged code at batch boundaries, which is
# invisible here: the pid persists, so a self-update is not a restart and does
# not consume a restart slot.
set -uo pipefail
cd "$(dirname "$0")/.."

TOTAL=${FR_TOTAL:-$((8 * 60 * 60))}
MIN_SLICE=${FR_MIN_SLICE:-300}   # below this, a restart cannot finish an issue
DEADLINE=$(( $(date +%s) + TOTAL ))
LOGDIR=orchestrator/logs
mkdir -p "$LOGDIR"

LANES=("$@")
[ ${#LANES[@]} -eq 0 ] && LANES=(run)

supervise_lane() {
  local lane=$1 suplog=$2
  local restarts=0 rc ran left
  say() { echo "[supervisor $(date -u +%H:%M:%S)] $*" | tee -a "$suplog"; }
  say "lane $lane: budget ${TOTAL}s, deadline $(date -u -r "$DEADLINE" +%H:%M:%SZ 2>/dev/null || date -u -d "@$DEADLINE" +%H:%M:%SZ)"
  while :; do
    left=$(( DEADLINE - $(date +%s) ))
    if (( left < MIN_SLICE )); then
      say "${left}s left, under the ${MIN_SLICE}s floor -- stopping"
      break
    fi

    say "starting $lane with ${left}s remaining (restart #$restarts)"
    began=$(date +%s)
    # stderr into the same stream as stdout: #89 lost the traceback because the
    # loop's crash output went somewhere nobody was reading.
    FR_WALLCLOCK="$left" python3 orchestrator/loop.py "$lane" >>"$suplog" 2>&1
    rc=$?
    ran=$(( $(date +%s) - began ))
    say "lane $lane exited rc=$rc after ${ran}s"

    # A clean exit having used most of its slice is the loop finishing, not
    # failing. Anything else is a crash, and a crash that happens instantly will
    # happen instantly again -- back off so a broken tree cannot spin.
    if (( rc == 0 && ran > left - 120 )); then
      say "lane $lane completed its budget -- done"
      break
    fi
    restarts=$(( restarts + 1 ))
    if (( ran < 60 )); then
      say "lane $lane died in ${ran}s -- this is a crash loop, not a run; stopping for a human"
      break
    fi
    sleep 30
  done
}

pids=()
for lane in "${LANES[@]}"; do
  suplog="$LOGDIR/supervisor-$lane-$(date -u +%Y%m%dT%H%M%SZ).log"
  supervise_lane "$lane" "$suplog" &
  pids+=("$!")
done

# Ctrl-C or TERM stops every lane, not whichever one the shell happened to be
# waiting on.
trap 'kill ${pids[*]} 2>/dev/null' INT TERM
for p in "${pids[@]}"; do
  wait "$p"
done
trap - INT TERM