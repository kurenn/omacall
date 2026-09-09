#!/bin/bash
#
# PLAN.md §0 — the netem rows of the spike go/no-go matrix.
#
#   ./scripts/spike-netem.sh bottleneck   rate-limit below offered load  (the real R1 test)
#   ./scripts/spike-netem.sh loss2        2% loss, 30ms +/-10ms jitter
#   ./scripts/spike-netem.sh loss5        5% loss
#   ./scripts/spike-netem.sh clean        remove any qdisc this left behind
#
# Runs entirely inside an unprivileged user+network namespace, so it needs no
# sudo at all: CAP_NET_ADMIN inside the namespace is all tc requires, and the
# namespace's own loopback is the only interface it can touch. That also means
# shaping cannot leak onto enp2s0 and throttle ssh, and nothing is left behind
# if the script dies -- the namespace evaporates with the process.
#
# Consequence worth knowing: there is no external network inside, so iroh
# cannot reach a relay. Connections here are direct over loopback, which is
# what these rows want. The relay row cannot be run this way.

set -uo pipefail

# Re-exec into a user+network namespace unless we are already in one.
if [[ -z ${OMACALL_NETNS:-} ]]; then
  exec unshare -Urn env OMACALL_NETNS=1 "$0" "$@"
fi

MODE=${1:-}
IFACE=lo
OFFERED_BITRATE=1500000        # what the encoder is told to produce
BOTTLENECK=1mbit               # deliberately below OFFERED_BITRATE
DURATION=${DURATION:-45}
SPIKE=./target/debug/spike
LOGDIR=$(mktemp -d)

cleanup() {
  pkill -x gst-launch-1.0 2>/dev/null
  pkill -x spike 2>/dev/null
  echo
  echo "cleaned up (namespace discarded with its qdisc)"
}
trap cleanup EXIT INT TERM

[[ -x $SPIKE ]] || { echo "build first: cargo build --bin spike"; exit 1; }

if [[ $MODE == clean ]]; then
  echo "nothing to clean: shaping lives in a throwaway namespace"
  trap - EXIT
  exit 0
fi

case $MODE in
bottleneck) NETEM=(rate "$BOTTLENECK") ;;
loss2)      NETEM=(loss 2% delay 30ms 10ms) ;;
loss5)      NETEM=(loss 5%) ;;
*) echo "usage: $0 bottleneck|loss2|loss5|clean"; exit 2 ;;
esac

echo "=== $MODE: netem ${NETEM[*]} on $IFACE, offering $((OFFERED_BITRATE / 1000))kbps ==="
ip link set "$IFACE" up || { echo "cannot bring $IFACE up"; exit 1; }
tc qdisc add dev "$IFACE" root netem "${NETEM[@]}" || { echo "tc failed"; exit 1; }
tc qdisc show dev "$IFACE" | sed 's/^/  /'

OMACALL_PORT_BASE=5100 "$SPIKE" listen > "$LOGDIR/listen.log" 2>&1 &
for _ in $(seq 1 30); do
  TICKET=$(grep -o '^TICKET .*' "$LOGDIR/listen.log" 2>/dev/null | cut -d' ' -f2-)
  [[ -n $TICKET ]] && break
  sleep 1
done
[[ -z ${TICKET:-} ]] && { echo "listener never produced a ticket"; cat "$LOGDIR/listen.log"; exit 1; }

OMACALL_PORT_BASE=5000 "$SPIKE" dial "$TICKET" > "$LOGDIR/dial.log" 2>&1 &
sleep 6
grep -q 'connected to' "$LOGDIR/dial.log" || { echo "never connected"; cat "$LOGDIR/dial.log"; exit 1; }

# Offered load: constant-rate VP8, the shape real media has.
gst-launch-1.0 -q videotestsrc is-live=true pattern=smpte \
  ! videoconvert ! video/x-raw,width=1280,height=720,framerate=30/1 \
  ! vp8enc deadline=1 target-bitrate=$OFFERED_BITRATE keyframe-max-dist=15 error-resilient=1 \
  ! rtpvp8pay pt=96 mtu=1120 ! udpsink host=127.0.0.1 port=5000 sync=false \
  > /dev/null 2>&1 &

# Does anything still decode on the far side under these conditions?
gst-launch-1.0 -q udpsrc port=5104 \
    caps="application/x-rtp,media=(string)video,encoding-name=(string)VP8,payload=(int)96" \
  ! rtpjitterbuffer latency=120 do-lost=true ! rtpvp8depay ! vp8dec \
  ! fpsdisplaysink video-sink=fakesink text-overlay=false sync=false \
  > "$LOGDIR/recv.log" 2>&1 &

echo
echo "running ${DURATION}s..."
sleep "$DURATION"

echo
echo "=== sender instrument (last 10s) ==="
grep -E '^out ' "$LOGDIR/dial.log" | tail -10

echo
echo "=== what to read ==="
echo "  kbps_out       offered load actually admitted to the wire"
echo "  send_buf_free  R1's observable. Sustained near 0 means quinn is"
echo "                 silently discarding oldest datagrams -- the failure"
echo "                 mode that returns no error and logs nothing."
echo "  paths          ip:N relay:N"
echo
echo "  PASS (bottleneck): send_buf_free dips but recovers, decoding continues,"
echo "                     and latency does not grow without bound."
echo "  FAIL (bottleneck): send_buf_free pinned at 0 with multi-second stale"
echo "                     video -- that is the R1 fail condition, and the"
echo "                     point at which webrtcbin gets reconsidered."
echo
echo "  PASS (loss2): freezes recover within a keyframe interval (~0.5s at"
echo "                keyframe-max-dist=15), no permanent freeze."
echo
echo "=== receiver fps ==="
grep -oE 'current: [0-9.]+' "$LOGDIR/recv.log" | tail -3 || echo "  (no fps lines -- receiver decoded nothing)"
echo
echo "logs: $LOGDIR"
