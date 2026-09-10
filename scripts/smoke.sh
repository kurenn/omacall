#!/bin/bash
#
# The merge gate. Boots two daemons on this machine, places a call, and asserts
# that video actually decoded on both sides -- not that datagrams moved.
#
# A datagram counter passes with a caps typo, a depayloader mismatch or a dead
# decoder. Frames decoded is the only number that means the call worked.
#
# Uses videotestsrc throughout: V4L2 allows one opener, so grabbing the real
# camera would fight any call in progress.

set -uo pipefail

BIN=${OMACALL_BIN:-./target/debug/omacall}
DIR=$(mktemp -d)
MIN_FRAMES=${MIN_FRAMES:-30}
WAIT=${WAIT:-15}

cleanup() {
  [[ -n ${A_PID:-} ]] && kill "$A_PID" 2>/dev/null
  [[ -n ${B_PID:-} ]] && kill "$B_PID" 2>/dev/null
  rm -rf "$DIR"
}
trap cleanup EXIT INT TERM

[[ -x $BIN ]] || { echo "build first: cargo build"; exit 1; }

start() { # name, pattern
  mkdir -p "$DIR/$1"
  OMACALL_RING_SILENT=1 OMACALL_AUTOACCEPT=1 \
  OMACALL_CONFIG_DIR="$DIR/$1" OMACALL_SOCKET_DIR="$DIR/$1" \
  OMACALL_VIDEO_SRC="videotestsrc is-live=true pattern=$2" \
  OMACALL_AUDIO_SRC="audiotestsrc is-live=true wave=silence" \
  OMACALL_SINK="fakesink sync=false" \
    "$BIN" daemon > "$DIR/$1.log" 2>&1 &
  echo $!
}

status() { OMACALL_SOCKET_DIR="$DIR/$1" "$BIN" status 2>/dev/null; }

A_PID=$(start a smpte)
B_PID=$(start b ball)

for _ in $(seq 1 30); do
  TICKET=$(sed -n 's/^  ticket: //p' "$DIR/b.log" 2>/dev/null)
  [[ -n $TICKET ]] && break
  sleep 1
done
[[ -z ${TICKET:-} ]] && { echo "FAIL: callee never started"; cat "$DIR/b.log"; exit 1; }

OMACALL_CONFIG_DIR="$DIR/a" "$BIN" add bee "$TICKET" > /dev/null || { echo "FAIL: could not save the contact"; exit 1; }
OMACALL_SOCKET_DIR="$DIR/a" "$BIN" call bee > /dev/null || { echo "FAIL: the call was refused"; exit 1; }

sleep "$WAIT"

fail=0
for side in a b; do
  json=$(status "$side")
  read -r state frames direct <<<"$(python3 -c "
import json,sys
d=json.loads('''$json''' or '{}')
f=max((p['frames_decoded'] for p in d.get('peers',[])), default=0)
print(d.get('state','?'), f, d.get('paths_direct',0))
" 2>/dev/null)"
  printf "  %s: state=%s frames_decoded=%s direct=%s\n" "$side" "$state" "$frames" "$direct"
  [[ $state == in_call ]] || { echo "    FAIL: expected in_call"; fail=1; }
  [[ ${frames:-0} -ge $MIN_FRAMES ]] || { echo "    FAIL: fewer than $MIN_FRAMES frames decoded"; fail=1; }
done

OMACALL_SOCKET_DIR="$DIR/a" "$BIN" hangup > /dev/null 2>&1
sleep 2
for side in a b; do
  s=$(status "$side" | python3 -c "import json,sys; print(json.load(sys.stdin)['state'])" 2>/dev/null)
  [[ $s == idle ]] || { echo "  FAIL: $side did not return to idle after hangup (got $s)"; fail=1; }
done

if [[ $fail -eq 0 ]]; then
  echo "smoke: OK"
else
  echo "smoke: FAILED"
fi
exit $fail
