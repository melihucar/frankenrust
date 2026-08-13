#!/usr/bin/env bash
# Keep the loop alive for a fixed total budget, and no longer.
#
# `python3 orchestrator/loop.py run` stops for two very different reasons: it
# finished its wallclock, or it died. Only the second one should be restarted,
# and neither is distinguishable from the outside -- both are just an exit.
#
# So this owns the budget instead of the loop. It computes one absolute
# deadline up front and hands each successor only the time that is actually
# left. That is the whole point: a `while true` wrapper restarting a dead loop
# would hand it a fresh FR_WALLCLOCK every time, so an 8h run that crashes
# hourly runs forever. #89 recorded the manual version of that mistake -- a
# cold restart at 08:41 gave itself another full 8h on top of the 6h already
# spent -- and a supervisor is only worth having if it does not automate it.
#
#   scripts/supervise.sh            # 8h total, restarts across crashes
#   FR_TOTAL=3600 scripts/supervise.sh
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
SUPLOG="$LOGDIR/supervisor-$(date -u +%Y%m%dT%H%M%SZ).log"

say() { echo "[supervisor $(date -u +%H:%M:%S)] $*" | tee -a "$SUPLOG"; }

say "budget ${TOTAL}s, deadline $(date -u -r "$DEADLINE" +%H:%M:%SZ 2>/dev/null || date -u -d "@$DEADLINE" +%H:%M:%SZ)"

restarts=0
while :; do
  left=$(( DEADLINE - $(date +%s) ))
  if (( left < MIN_SLICE )); then
    say "${left}s left, under the ${MIN_SLICE}s floor -- stopping"
    break
  fi

  say "starting loop with ${left}s remaining (restart #$restarts)"
  began=$(date +%s)
  # stderr into the same stream as stdout: #89 lost the traceback because the
  # loop's crash output went somewhere nobody was reading.
  FR_WALLCLOCK="$left" python3 orchestrator/loop.py run >>"$SUPLOG" 2>&1
  rc=$?
  ran=$(( $(date +%s) - began ))
  say "loop exited rc=$rc after ${ran}s"

  # A clean exit having used most of its slice is the loop finishing, not
  # failing. Anything else is a crash, and a crash that happens instantly will
  # happen instantly again -- back off so a broken tree cannot spin.
  if (( rc == 0 && ran > left - 120 )); then
    say "loop completed its budget -- done"
    break
  fi
  restarts=$(( restarts + 1 ))
  if (( ran < 60 )); then
    say "died in ${ran}s -- this is a crash loop, not a run; stopping for a human"
    break
  fi
  sleep 30
done

say "supervisor done after $restarts restart(s); log: $SUPLOG"
