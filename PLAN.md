# omacall v2 — implementation plan

Peer-to-peer video calls between any two machines on any network. This plan replaces the
LAN-only bash `omacall` with a Rust binary over [iroh](https://github.com/n0-computer/iroh),
in four stages, each of which leaves something shippable.

This document is the merge of an implementation plan and an adversarial review of it. Where
the review overturned the original, the reasoning is kept inline — the wrong version is
usually the one you'd reach for again otherwise.

---

## Settled architecture

Decided before this plan; not re-opened here.

- **Transport: iroh.** QUIC with ed25519 `NodeId` identity, hole punching, relay fallback.
  Media on QUIC unreliable datagrams with a 1-byte channel tag; control on reliable streams.
  `webrtcbin` was rejected because it solves only media transport, leaving signaling,
  identity and TURN unsolved and two services to host. libp2p rejected on leanness. An SFU
  is excluded because it puts a server back in the media path.
- **Rust**, with `gstreamer-rs` linked against system GStreamer, never bundled.
- **A resident daemon** holds the identity key and one iroh endpoint, keeping a relay
  connection so the machine is always ringable with no port forwarding and no sshd.
- **One window** compositing every participant plus local picture-in-picture.
- **Full mesh, roster capped at 4**, encode-once-and-fan-out. The binding limit is upstream
  bandwidth, not CPU.
- **Relay** defaults to n0's public infrastructure, overridable to a self-hosted `iroh-relay`.
- **Ringing UI** reuses v1's approach: `notify-send`, a ringtone, omarchy's floating terminal
  plus `gum confirm`.

## Environment: verified facts

Checked on the target machine, not assumed.

| Fact | State |
|---|---|
| Rust stable 1.97.1 via rustup | working |
| `gst-plugins-bad` / `-libs` 1.28.6 | installed |
| `gst-plugin-gtk` 1.28.6 | installed — owns `gtkwaylandsink` (GTK3) |
| `gst-plugin-va` (`vah264enc`) | **NOT installed** — it is its OWN package on Arch, not part of `gst-plugins-bad` |
| VCN H.264 **encode** capability | **confirmed** — `vainfo` lists ConstrainedBaseline/Main/High with `VAEntrypointEncSlice` |
| `gst-plugin-gtk4` (`gtk4paintablesink`) | in `extra`, not installed |
| `compositor`, `glvideomixer`, `waylandsink`, `glimagesink`, `videobox` | present |
| Logitech C930e | **720p30 only as MJPG**; YUYV 720p caps at 10fps |
| AMD Barcelo iGPU, `/dev/dri/renderD128`, `radeonsi_drv_video.so` | present; hardware encode confirmed, GStreamer negotiation still unproven |
| PipeWire 1.6.8, `gum`, `notify-send`, `pw-play`, floating-terminal helper, `phone-incoming-call.oga` | present |
| ufw, sshd | both active |
| iroh | **1.2.0** — ordinary semver, not pre-1.0 |
| `rtpvp8pay` default mtu | **1400** |
| quinn initial MTU | **1200** |
| `gtkwaylandsink` | links `libgtk-3.so.0`; gtk3-rs is archived |
| `pw-play --loop` | **does not exist** |

**Unverified, resolved by a named task:** whether iroh's `QuicTransportConfig` passes a
congestion-controller factory through (spike); whether `vp8enc`'s bitrate is mutable while
playing (Stage 2 gate 2); whether `vah264enc` negotiates on this VCN (Stage 2 gate 1).

## Traps already paid for in v1 — do not rediscover

1. **ufw silently drops inbound UDP.** The receiver binds, reports no error, burns zero CPU
   and never sees a frame. v2 is outbound-only, which deletes this rather than handling it.
2. **Backgrounding a shell function orphans its child.** `send &` captured the subshell PID;
   killing it left `gst-launch` transmitting forever. Several orphans then fed one `udpsrc`
   and produced garbled video that looked exactly like a codec bug.
3. **macOS TCC grants camera to the terminal, not the process.** An ssh-launched process gets
   zero frames, no error, ever.
4. **V4L2 permits one opener** — self-view must branch off a single `tee`.
5. **Many UVC cameras reach 720p30 only as MJPEG** (confirmed on this C930e).
6. **`pkill -f <pattern>` matches your own command line.** No `pkill` anywhere in this
   codebase, ever.

---

## 0. Spike: `rtp-over-iroh` — before any production code

One throwaway binary, `src/bin/spike.rs`, ~250 lines, deleted at the end of Stage 1. It is
deliberately a rough draft of Stage 1's tunnel, so little is wasted.

- `spike listen` — iroh endpoint (ALPN `omacall/0`), prints its node ticket, accepts one
  connection. Datagrams tagged `0`/`1` forward to `127.0.0.1:5004`/`5006`; UDP arriving on
  `127.0.0.1:5000`/`5002` goes back out as tagged datagrams.
- `spike dial TICKET` — the same bridge, other direction.

Media is v1's own pipelines, hand-launched. No control protocol, no UI, one hardcoded call.

**Set a small `datagram_send_buffer_size` (~32KB) in the transport config from the first
commit.** With the default-sized buffer, congestion first manifests as seconds of buffered
stale video — a latency balloon — and only then as silent drops. A small buffer converts that
into the drop-oldest behavior the tunnel's own channels already have. This is the single most
important R1 mitigation.

Log at 1Hz: datagrams in/out, `Connection::stats()` (path RTT, lost packets, cwnd),
`datagram_send_buffer_space()`, `max_datagram_size()`, and the active path from
`paths_stream()`.

### Pass/fail matrix

| Question | Test | Pass | Fail → reconsider webrtcbin |
|---|---|---|---|
| Hole punching between real ISPs | A at home, B on a different ISP. A phone hotspot is a valid and *harder* CGNAT stand-in. Read the reported path type. | Direct path within ~5s on the LAN pair and at least one WAN pair; when direct fails, relay still carries the call | No direct path on any real WAN pair **and** relay can't sustain media |
| **Bandwidth bottleneck** (the real R1 test) | `tc qdisc ... netem rate 1mbit` with 1.5Mbps offered | Added glass-to-glass latency bounded (< ~500ms), drops observable via `datagram_send_buffer_space()` | Multi-second stale video with no observable signal |
| Loss tolerance | `netem loss 2% delay 30ms 10ms`, 10 min at 1.5Mbps VP8. Repeat at 5%; also 5% audio-only | 2%: video recovers within a keyframe interval, jitterbuffer holds. 5%: Opus with `inband-fec=true` stays intelligible | Permanent freezes at 2% |
| Datagram size, **including across a path switch** | Log `max_datagram_size()` on LAN and WAN; force a relay→direct transition mid-flow and log it again. Count size errors at payloader mtu 1400, 1150, 1120 | An mtu ≥1120 exists with zero size errors on the *worst* path, not just the current one | Only tiny datagrams fit even after MTU discovery |
| Relay as a media path | Force relay, 1.5Mbps for 10 min | Sustained rate, added RTT < 150ms, no growing drop rate | Relay throttles. Test self-hosted `relay.kurice.fyi` before concluding — n0's relays are donated infrastructure and may rate-limit |
| ufw claim | Both machines keep ufw active with **zero** omacall rules; capture `ufw status` | Calls connect anyway: every flow is initiated outbound, so conntrack's ESTABLISHED handling admits the returns | A demonstration, recorded once |

**Why the loss row is not enough.** Random 2% loss barely moves BBR and only moderately
shrinks CUBIC's window. What kills datagram media is a bottleneck *below offered load*.
Without the rate-limited row, every spike row can pass and R1 still ambushes Stage 2 in the
field.

**On the congestion controller.** Quinn's BBR is labelled experimental by its own authors. It
is a lever worth trying, not the prepared answer. The prepared answer is offered-load control
(AIMD) plus the small send buffer. Confirm during the spike that iroh's `QuicTransportConfig`
actually exposes `congestion_controller_factory` before planning around it.

Also captured, because it is free: glass-to-glass latency, by pointing the camera at an
on-screen millisecond timer and photographing both.

**Budget: 2–3 days including the second-ISP field test. Do not start Stage 1 until every row
is green.**

---

## 1. Repo and module structure

Single crate, no workspace. The bash script stays at the repo root through Stage 1 and dies at
the end of Stage 2.

```
omacall/
├── omacall                  # bash v1 — Stage 1 patch, deleted end of Stage 2
├── PLAN.md
├── README.md
├── Cargo.toml               # bin "omacall"; iroh pinned =1.2.0
├── src/
│   ├── main.rs              # clap: (no args)=picker, call, id, add, daemon, doctor
│   ├── ipc.rs               # control socket — every CLI command routes through the daemon
│   ├── config.rs            # config.toml; keeps v1's OMACALL_* env overrides
│   ├── identity.rs          # load_or_create_key() → ~/.config/omacall/key, mode 0600
│   ├── proto.rs             # Msg, framing, datagram tags; CallState as a PURE struct
│   ├── daemon.rs            # endpoint, accept loop, owns CallState, owns every dial
│   ├── call.rs              # per-call actor: control streams, roster, media lifetime
│   ├── tunnel.rs            # Stage 1 only: UDP ↔ tagged datagrams
│   ├── ring.rs              # notify-send, ringtone respawn loop, gum confirm, cancel path
│   ├── contacts.rs          # minimal in Stage 1 (read + add); picker/TOFU in Stage 3
│   ├── doctor.rs            # Stage 3
│   └── media/               # Stage 2
│       ├── mod.rs           # per-call Pipeline owner; all pad ops on one thread
│       ├── capture.rs       # MJPG caps are mandatory on this camera, not a fallback
│       ├── codec.rs         # probe_codecs(); encoder/decoder bin builders
│       ├── window.rs        # GTK window, compositor, audiomixer, pad churn, layout
│       └── adapt.rs         # 1Hz AIMD
├── src/bin/spike.rs         # deleted end of Stage 1
├── packaging/{PKGBUILD,omacall.service,Omacall.desktop}
└── scripts/smoke.sh         # regression gate
```

### The control socket (`ipc.rs`) — the plan's largest structural fix

The original plan had no daemon↔CLI IPC and was **unimplementable without it**. `omacall dial`
is a separate process: it either builds its own iroh endpoint — a *different ephemeral NodeId*,
so the callee's `ring_unknown` gating sees a stranger, and the tunnel can't live there anyway
because the daemon owns the media ports — or it asks the daemon. Nothing else works. Busy
semantics have the same dependency: "one active call per daemon" is only enforceable if every
dial routes through the daemon, otherwise a second caller gets ringing instead of `Busy`.

`$XDG_RUNTIME_DIR/omacall.sock`, newline-delimited JSON: `dial`, `status`, `hangup`, later
`invite`. `smoke.sh` asserts against `status`. The picker and in-call invite ride the same
socket in Stage 3.

### Logging

`tracing` with a file layer in `$XDG_STATE_HOME/omacall/`, **from the first commit**. A
peer-to-peer app debugged across two houses without a log file is undebuggable, and this is
how a user reports a bug. `doctor` prints the tail of the last call's errors.

### Identity key handling

`load_or_create_key()` must **fail loudly** on an existing-but-unparseable file. Falling
through to "create" silently mints a new NodeId and destroys the user's identity with every
contact they have.

---

## 2. Control protocol (`proto.rs`)

**Framing.** Control: one bidirectional QUIC stream per peer, opened by the dialer; each
message is a `u16` LE length followed by postcard bytes. Media and feedback: unreliable
datagrams whose first byte is the channel tag — `0` video RTP, `1` audio RTP, `2` feedback
(postcard `Msg`; only `KeyframeReq` and `Stats` are legal there).

```rust
enum Msg {
    Ring        { call_id: u64, name: String, video: Vec<Codec>, roster: Vec<Peer> }, // human invite ONLY
    Join        { call_id: u64, name: String, video: Codec },   // silent mesh join
    PeerJoining { node_id: NodeId, name: String },              // inviter → every member
    Accept      { name: String, video: Codec },
    Decline, Busy, Cancel, Bye,
    Ping        { t_us: u64 },
    Pong        { t_us: u64 },
    KeyframeReq,
    Stats       { pkts: u32, lost: u32, jitter_ms: u16 },
}
enum Codec { H264, Vp8 }
struct Peer { name: String, node_id: NodeId }
```

### Why `Join` and `PeerJoining` exist

The original design made `call_id` a bearer token: any peer presenting it was auto-accepted
silently. But `Ring` delivers that token *before* the invitee's user decides. **Someone you
declined — or who let the ring time out — kept a credential that every member would silently
auto-accept, camera on, no ring, no notification, for the rest of the call.** The decline path
defeated its own trust model. The same leak reached a 5th participant who was `Busy`'d after
being invited.

Now a member auto-accepts `Join{call_id}` **only if** the joiner's NodeId is in its
`pending_joiners` set, which is populated solely by a `PeerJoining` from an existing member and
expires after 30s. `Ring` while `InCall` is *always* `Busy`. The token alone admits nobody, and
`PeerJoining` doubles as the "B is adding C" notification.

### State machine

`CallState` is pure — `fn handle(&mut self, Event) -> Vec<Action>` — so the whole table is
unit-testable with no network. That is the point of the struct, and it is why this protocol
rework costs days rather than weeks.

```
Idle --dial--------------> RingingOut(peer)   45s -> Cancel -> Idle
Idle --Ring--------------> RingingIn(peer)    45s -> Decline -> Idle
                                              ring_unknown=false + unknown NodeId -> auto-Decline
RingingOut --Accept--> InCall
RingingOut --Decline/Busy/timeout/conn-lost--> Idle
RingingIn  --user yes--> send Accept --> InCall
RingingIn  --user no ---> send Decline --> Idle
RingingIn  --Cancel-----> tear down ring UI --> Idle
InCall:
  Join{call_id matches AND node in pending_joiners} -> auto-Accept, add peer
  Join{otherwise} or Ring{any}                      -> Busy + missed-call notification
  peer Bye or QUIC connection closed                -> remove peer; roster empty -> Idle
  local window closed / SIGINT                      -> Bye to every peer -> Idle
```

**Glare.** A dials B while B dials A: in `RingingOut`, a `Ring` *from the peer being dialed* is
mutual intent — the lower NodeId's `call_id` wins, both sides go to `InCall` with no prompt,
because both users already expressed intent. A `Ring` from anyone else while `RingingOut` gets
`Busy`. The original had no transition here at all, which is the oldest race in telephony.

**Second `Ring` while `RingingIn`, same call_id** (two members invite the same person at once):
coalesce the inviters; one Answer sends `Accept` to each. A different call_id gets `Busy`.

**Anything except `Ring` arriving in `Idle`** → reply `Bye` and ignore. This mops up the late
`Accept` that arrives after a caller gave up.

**Join completeness.** The joiner must hold live connections to the full roster within 15s of
its `Accept`, or it sends `Bye` to all and reports "couldn't reach A" locally. Members that saw
`PeerJoining` but no `Join` within 30s drop the pending entry and notify.

This closes a failure the original called an accepted imperfection but wasn't: if C can reach B
but not A, B renders three tiles while A and C each render two — a **stable, silent, divergent
call** with no repair, no timeout and no error. Nobody forwards media in a mesh, so A and C
never see each other while both believe the call is fine. Now it aborts cleanly and visibly.

**Missed calls.** Every `Busy` also fires a `notify-send`. Otherwise the callee never learns
anyone called.

**Timeouts.** Ring 45s (v1's value). Liveness comes from QUIC's 20s idle timeout with 5s
keep-alives — the transport detects dead peers, so there is no protocol-level liveness logic.
`Ping`/`Pong` exists only to display RTT.

### Codec: call-wide, fixed at creation

Because we encode once and fan out, the codec cannot be per-pair without forcing double
encoding. `Ring.video` lists the dialer's codecs in preference order; `Accept.video` picks the
first common one; a joiner whose codecs lack the call's active one is declined. No mid-call
renegotiation, ever. Every client always offers VP8 and offers H.264 when `probe_codecs()`
found working VA-API, so this only bites mixed hardware fleets that prefer H.264; `prefer =
"vp8"` in config is the escape hatch.

---

## 3. Stages

### Stage 1 — transport tunnel, bash media untouched

0. **Control socket** (`ipc.rs`). Everything else depends on it.
1. `identity.rs`: `load_or_create_key()` (parent dirs, 0600, refuse group/world-readable, fail
   loudly on corruption), `omacall id` printing the node ticket. Endpoint in `daemon.rs`: iroh
   pinned `=1.2.0`, ALPN `omacall/0`, n0 relays with `relay =` override, local-network and DNS
   discovery, small `datagram_send_buffer_size`.
2. `proto.rs`: `Msg`, framing, and `CallState` with a unit test per transition above —
   including glare, coalesced invites, join gating and the `Idle` catch-all. Pure code, no
   network, written in parallel with everything else.
3. `tunnel.rs`: promote the spike bridge. Two tokio tasks per direction, 64-packet bounded
   channels, drop-oldest. Never buffer unreliable media.
4. `ring.rs`: `notify-send -u critical`; ringtone via a **respawn loop** around `pw-play`
   (there is no `--loop` flag); floating terminal plus `gum confirm` writing a verdict file.
   A `CancelRing` path must kill both the ringtone loop and the floating terminal, or a
   cancelled call leaves a live dialog whose Answer arrives at a peer already in `Idle`.
   Honor `OMACALL_AUTOACCEPT=1` for tests.
5. `daemon.rs` accept loop and `call.rs`: on accept, start the tunnel and spawn
   `omacall --talk 127.0.0.1 <name>` through `tokio::process::Command` with
   `kill_on_drop(true)` in its own process group, killed by pgid on hangup. **This is trap 2's
   fix: the daemon holds the `Child`, never pattern-matches process names, never orphans.**
6. Minimal `contacts.rs`: read `contacts.toml`, `omacall add NAME TICKET`. Stage 1's `dial`
   takes a name, so name→NodeId resolution cannot wait for Stage 3.
7. Patch bash — the honest list is **eight lines, not six**:
   - `recv()`: `udpsrc port=$((VPORT+4))` and the audio twin (5004/5006), because `send`
     targets `127.0.0.1:5000` and `recv` cannot bind the port the tunnel owns.
   - `send()`: **`mtu=1120` on `rtpvp8pay` and `rtpopuspay`.** v1 runs the default 1400, and
     1400+1 exceeds quinn's 1200-byte initial MTU, so every keyframe fragment is dropped at
     call start. The original patch list forgot to apply the spike's own conclusion, and the
     symptom would have looked exactly like a codec bug.
   - `call()`: the `ssh ... --answer` block becomes a `dial` over the control socket, then
     `talk 127.0.0.1 "$name"`.
   - `answer()` is no longer invoked remotely.
8. `OMACALL_PORT_BASE` so two daemons can run on one machine — the tunnel's fixed 5000/5002
   otherwise makes single-machine testing impossible.
9. `smoke.sh` v0: signaling only, driven through the control socket.

**Known warts, written down rather than discovered:** if the daemon dies mid-call, the
caller's bash `talk` keeps looping on live gst children and sits in a frozen call with no
signal; the callee self-heals via QUIC idle timeout plus `kill_on_drop`. The daemon must bind
tunnel ports *before* acking a dial, or the first packets vanish.

**Definition of done.** Two machines on different home ISPs. **sshd stopped on both.** ufw
active with no omacall rules on either (`ufw status` shows none). Caller runs bash `omacall
<name>`; callee hears the ringtone, sees the notification and prompt; Answer gives two-way
audio and video through v1's pipelines. Decline and 45s timeout both leave the caller a clean
message and **zero** surviving gst processes. Killing the daemon mid-call kills its media
children. This ships on its own.

### Stage 2 — media in-process, one window

Gates first, before any media code:

1. `sudo pacman -S gst-plugin-va gst-plugin-gtk4`. Note `gst-plugin-va` is a **separate Arch
   package**, not part of `gst-plugins-bad` — installing the latter alone leaves `vah264enc`
   missing. The VCN's H.264 encode capability is already confirmed via `vainfo`; what remains
   is whether GStreamer negotiates with it:
   `gst-launch-1.0 videotestsrc num-buffers=100 ! vah264enc ! vah264dec ! fakesink` and a live
   camera loopback. Resolves the plan's biggest stated unknown in ten minutes. If VCN encode
   is broken: VP8-only, `openh264enc` as a middle option, nothing downstream changes.
2. Runtime bitrate mutability: set `bitrate` on a playing `vah264enc` and `target-bitrate` on
   `vp8enc`. `vp8enc`'s property lacks the mutable-in-playing annotation, so this gate is
   well-placed — keep it.
3. **GTK: `gtk4paintablesink` is primary.** The original planned to try `gtkwaylandsink` first
   and fall back. It links `libgtk-3.so.0` and gtk3-rs is archived; spending half a day wiring
   an EOL binding stack into a new codebase in order to probably rip it out is waste. Inverted:
   gtk4 primary, `gtkwaylandsink` a fallback footnote.
4. **New gate — aggregator dynamic latency.** Compositor plus audiomixer, one live branch at
   start, a second with 120ms latency added at t+5s; assert no dropped buffers once
   `min-upstream-latency` (~250ms) is set on both. Ten minutes, and it prevents stuttering in
   *every* call — see §4.

Then:

5. `capture.rs`: `v4l2src ! image/jpeg,width=1280,height=720,framerate=30/1 ! jpegdec !
   videoconvert ! tee name=cam`. **MJPG caps are mandatory on this camera**, not a fallback:
   YUYV 720p is 10fps. Keep v1's `OMACALL_VIDEO_SRC`/`CAM`/`RES` knobs;
   `videotestsrc is-live=true` through that knob is how single-machine tests dodge V4L2's
   single-opener rule. Self-view is a second `tee` branch.
6. `codec.rs`: `probe_codecs()` at daemon start. Encoder bin
   `tee. ! queue leaky=downstream ! {vah264enc|vp8enc} ! {rtph264pay|rtpvp8pay} mtu=1120 pt=96
   ! appsink`, mtu per the spike's measured worst path. Audio:
   `pipewiresrc ! audioconvert ! audioresample ! opusenc frame-size=20 inband-fec=true !
   rtpopuspay pt=97 mtu=1120 ! appsink`.
7. Fanout: one task pulls the encoder appsink and `send_datagram(tag + rtp)` per roster peer.
   Per-peer send errors are counted, never fatal. Adding a peer fires `ForceKeyUnit` so a late
   joiner decodes within its first second.
8. Per-peer receive bins: `appsrc is-live=true do-timestamp=true ! application/x-rtp,... !
   rtpjitterbuffer latency=120 do-lost=true ! depay ! {vah264dec|vp8dec} ! videoconvert !
   queue ! compositor.sink_N`. Audio:
   `... ! opusdec ! audioconvert ! audioresample ! capsfilter(48k stereo) ! amix.` —
   **`audiomixer` does not convert**, and without an explicit chain the second joiner with a
   different channel layout simply refuses to link.
9. `window.rs` — see §4.
10. `KeyframeReq` both ways, **with a global 2s debounce**. Without it, one starved peer
    requesting at 1Hz forces keyframes for *everyone*, and a keyframe is 5–10× a delta frame —
    one bad link then degrades every healthy one. That is the steady state of any call with a
    weak participant, not a corner case.
11. `adapt.rs`: 1Hz per peer. Loss comes from `Connection::stats().path.lost_packets` plus the
    peer's `Stats`, **and from `datagram_send_buffer_space()`** — quinn discards queued
    datagrams silently, oldest first, rather than returning an error, so send-buffer pressure
    is a first-class input. Worst peer governs. Loss >2% on two consecutive ticks halves the
    target (floor 300kbps); a clean tick adds 100kbps (ceiling 2.5Mbps). **Bitrate only**; the
    540p drop stays behind a flag until live caps renegotiation is proven.
12. Mesh: `PeerJoining`/`Join` end to end, plus a 50× join/leave churn stress script.
13. Cutover: delete the bash script, `tunnel.rs` and `spike.rs`; install as `omacall`; rewrite
    the README.

**Definition of done.** (a) 1:1 between the two real machines on the hardware H.264 path,
single window, remote full-frame with self PiP, closing the window hangs up both ends cleanly.
(b) Under `netem loss 2%`, freezes recover in under 2s and the adapt log shows a halving then
recovery; **plus a wifi-flap row** — the call survives an outage shorter than the idle timeout
and video recovers via a path-change-triggered `ForceKeyUnit` rather than waiting on
`KeyframeReq`. (c) A 3-way call where the third participant is invited **by the callee** (which
proves the join path, not just caller-adds), tiles re-lay live, one participant leaves without
disturbing the others — **and a joiner who cannot reach one member aborts cleanly with no ghost
tiles anywhere**. (d) `pgrep gst-launch` finds nothing ever again.

### Stage 3 — ergonomics

1. `contacts.toml` grows TOFU: after a completed call with an unsaved peer,
   `gum confirm "Save <name>?"`. `ring_unknown` defaults on for locally-discovered peers, off
   from the internet.
2. Picker: bare `omacall` merges contacts and iroh local discovery, pings each contact
   concurrently with a 2s timeout for presence, then `gum choose`. **First-run empty state:**
   with zero contacts, print your own ticket and "send this to a friend, then run `omacall add
   NAME TICKET`" — the populated case is not the only one.
3. `doctor.rs`: key exists and is 0600; relay reachable and which; camera present **and
   advertising MJPG 720p30**; `probe_codecs()` result; plugin inventory; daemon running and in
   a graphical session; a loopback media selftest; the tail of the last call's log.
4. Packaging: `PKGBUILD` — `depends` is gstreamer, gst-plugins-base, gst-plugins-good,
   gst-plugin-gtk4, gum, libnotify. `optdepends` is `gst-plugin-va` for hardware H.264 on
   AMD/Intel; a machine without it negotiates VP8 and works, which is why VP8 is a mandatory
   fallback rather than a legacy option — hardware encode is machine-specific (NVIDIA needs
   `nvenc` from another plugin entirely, a VM has neither). `libva-utils` is a diagnostic and
   must **not** be a dependency; `doctor` probes through GStreamer's factory lookup.
   **No firewall hooks of any kind.** `omacall.service` with
   `After=graphical-session.target`, `PartOf=graphical-session.target`,
   `WantedBy=graphical-session.target` — **not** `default.target`, or the daemon starts before
   Wayland exists and the ring UI is dead. `Omacall.desktop`. AUR publish.
5. One README line: v1's contacts file mapped names to IP addresses and is useless to v2.

**Definition of done.** On a freshly logged-in machine that installed only the AUR package:
`omacall` shows contacts with live presence, one keystroke places a call; `omacall add` of a
ticket pasted from another continent works; `doctor` on a machine with a broken camera says so
in one line; the daemon survives logout and login.

### Stage 4 — polish

1. Echo cancellation: `doctor` detects PipeWire's `module-echo-cancel` and prints the one-liner
   to enable it; `pipewiresrc` targets its virtual source when present.
2. Self-hosted relay: compose file and docs for `iroh-relay` behind Nginx Proxy Manager at
   `relay.kurice.fyi`. `relay =` has worked since Stage 1, so this is documentation plus a
   latency comparison.
3. In-call path indicator from `path_events()` — window title suffix `· LAN` / `· direct` /
   `· relay`. iroh 1.x is multipath, so this can change mid-call.
4. **Per-peer quality without per-peer encoding:** VP8 temporal scalability
   (`temporal-scalability-number-layers`, verified present) with fanout-side filtering by the
   payload descriptor's TID field gives 30/15/7.5fps tiers from a single encode. This is the
   real answer to worst-peer-governs, and it is explicitly not v2.0 scope.
5. macOS, best-effort and a gate for nothing: transport, control and CLI compile; the window
   needs a different sink; TCC means the daemon must be launched once from a granted terminal
   or shipped as a signed app.

---

## 4. The call window (`media/window.rs`)

**Elements.** `compositor` (software) feeding `gtk4paintablesink`. `glvideomixer` outputs
GLMemory and would need `gldownload` before a non-GL sink, forfeiting the GPU win; blending
four opaque 720p tiles is memcpy-grade work this iGPU will not notice. `glvideomixer` stays a
named upgrade if profiling disagrees.

**One pipeline per call**, video and audio together, one clock and one bus:

```
v4l2src ! image/jpeg,1280x720,30/1 ! jpegdec ! videoconvert ! tee name=cam
cam. ! queue ! encoder-bin ! appsink                  (fanout)
cam. ! queue ! videoscale ! compositor.sink_0         (self view)
compositor name=mix min-upstream-latency=250000000 ! video/x-raw,width=1280,height=720 ! gtk4paintablesink
pipewiresrc ! ... ! opusenc ! ... ! appsink           (audio out)
audiomixer name=amix min-upstream-latency=250000000 ! audioconvert ! pipewiresink
```

**`min-upstream-latency` on both aggregators is not optional.** Remote receive bins carry a
120ms jitterbuffer and are *always* added after the pipeline has started with only the
low-latency camera branch. An aggregator computes its latency from the sources present at
start, so later higher-latency branches deliver late and get dropped. The property's own
description names this exact scenario. Omitting it produces stuttering video and crackling
audio in **every** call — and it would be misdiagnosed as a network problem for days.

**GTK lifecycle.** One permanent GTK thread, created lazily at the first call and never torn
down; windows are created and destroyed per call and the main loop is re-run per call. GTK
cannot be de-initialized and re-initialized in-process, so "the daemon never runs GTK outside a
call" cannot mean init/teardown per call. The sink's widget must only be touched from the GTK
main thread, which the one-thread rule already guarantees.

**Pad add (peer joins)** — all pad surgery happens via messages to the single GLib thread,
never from a streaming thread and never concurrently:

1. Build the peer's receive bin, `pipeline.add()`.
2. `mix.request_pad_simple("sink_%u")`, link, same for audio into `amix`.
3. `bin.sync_state_with_parent()`.
4. `relayout()`.

A fresh appsrc means no pad blocking is needed on add.

**Pad remove (peer leaves)** — the original recipe deadlocks and had to be replaced. It EOSed
the appsrcs and *then* added a `BLOCK_DOWNSTREAM` probe; block probes fire on the next passing
buffer or event, and after EOS nothing else ever traverses that pad, so the callback never
fires. The tile freezes forever, the bin is never removed, and the churn test hangs on
iteration one. Corrected order:

1. Stop the peer's datagram-reader task.
2. Add an **EOS event probe** on the compositor and audiomixer sink pads.
3. `appsrc.end_of_stream()` on both appsrcs — drain rather than truncate.
4. When the EOS probe fires, on the GLib thread: unlink, `release_request_pad`, bin to Null,
   `pipeline.remove`.
5. `relayout()`.
6. A 1s timeout fallback forces the bin to Null first, then releases.

**`relayout()`** is a pure `fn layout(n_remote: usize) -> Vec<Tile>`, unit-tested, applied as
`xpos`/`ypos`/`width`/`height`/`zorder` on live compositor pads. Output frame fixed at 1280×720;
the sink scales to the window.

- 1 remote: remote full frame; self 320×180 bottom-right, z1.
- 2 remotes: two 640×360 centered; self PiP, z1.
- 3 remotes: 2×2 grid; self promoted to the fourth cell as an equal tile.

**Late joiners.** Video is solved sender-side: adding a fanout recipient fires `ForceKeyUnit`,
so the joiner's first datagrams carry a keyframe, with the receiver's `KeyframeReq` as backstop.
Each appsrc uses `do-timestamp=true` and the jitterbuffer rebuilds timing from RTP timestamps,
so every peer's branch builds its own timeline from arrival and a joiner needs no history.
Cross-stream lip-sync within one peer is deliberately not engineered — both jitterbuffers run
at 120ms and sinks run low-latency, matching v1's behavior, which nobody complained about. The
hook, if it ever matters, is a sender report on the feedback channel mapping RTP timestamps to
the sender's clock.

---

## 5. Testing

**Unit, runs anywhere, the regression floor.** Postcard round-trips and framing (truncated and
oversized frames); the full `CallState` table including glare, coalesced invites, join gating,
join-completeness abort and the `Idle` catch-all; `layout()` geometry; AIMD arithmetic against
synthetic loss series.

**One machine, no hardware.** Two daemons with `OMACALL_CONFIG_DIR` and `OMACALL_PORT_BASE`
pointing at separate scratch dirs and ports, `OMACALL_VIDEO_SRC="videotestsrc is-live=true"`,
`OMACALL_AUTOACCEPT=1`. Exercises signaling, media end to end, mesh join with three daemons,
pad churn and hangup cleanliness.

**Loopback's blind spot, which must be asserted around.** The loopback MTU is 65536, so
`max_datagram_size` there is enormous and a 1400-byte payload that would die on every real path
passes happily. Assert `max_datagram_size` against the payloader mtu at call start, and clamp
`lo` in the sudo `--loss` mode.

**`scripts/smoke.sh` — the merge gate.** v0 in Stage 1 is signaling only: dial, auto-accept,
hangup, state assertions over the control socket. Stage 2 adds media assertions. Those must
count **decoded frames per peer**, not datagrams — "≥200 datagrams flowed" passes with a caps
typo, a depay mismatch or a dead decoder. Under 30 seconds, run before every merge.
`smoke.sh --loss` wraps it in `netem` and additionally asserts a bitrate halving appears.

**Genuinely needs two machines:** hole punching across real ISPs and CGNAT; relay-forced media
quality; the hardware H.264 path on a second, probably non-AMD GPU; **network flap mid-call**;
macOS TCC.

---

## 6. Risk register

| # | Risk | Earliest detection | Mitigation |
|---|---|---|---|
| R1 | **QUIC congestion control silently drops queued datagrams, oldest first** — it does not return errors, so naive error counting observes nothing while video dies; with a default-sized buffer it first shows up as seconds of stale video | Spike, bottleneck row | Small `datagram_send_buffer_size`; `datagram_send_buffer_space()` as a first-class AIMD input; AIMD keeps offered load under the estimate. BBR is an experimental lever, not the answer. Fail here → webrtcbin exit at ~3 days' cost |
| R2 | Payloader mtu exceeds `max_datagram_size`, **including after a mid-call path switch** | Spike, size row | `mtu=1120`; fit the worst path, not the current one; log at call start and warn |
| R3 | `vah264enc` does not negotiate on this VCN | Stage 2 gate 1, ten minutes | VP8 path is default-on and fully specified; `openh264enc` middle option |
| R4 | ~~gtkwaylandsink drags in EOL gtk3-rs~~ **Closed by decision** — gtk4paintablesink is primary | — | — |
| R5 | Live resolution drop breaks encoder renegotiation | Stage 2 task 11 | Bitrate-only AIMD ships; resolution switching behind a flag |
| R6 | Hole punching fails for a real ISP pair | Spike WAN row | Relay fallback *is* the product answer; the real risk is relay media quality |
| R7 | Public relays throttle under 4.5Mbps mesh upstream | Spike relay row; re-measured at the 3-way DoD | `relay =` override from day one; consider defaulting the AUR example config to the self-hosted relay |
| R8 | iroh API churn — **downgraded**, iroh is 1.2.0, ordinary semver | `cargo update` | Pin `=1.2.0`; upgrade deliberately at stage boundaries |
| R9 | Compositor pad churn deadlocks or leaks | Stage 2 50× stress script | Corrected EOS-probe recipe; all pad ops on the GLib thread; the stress script becomes a permanent test |
| R10 | Daemon starts before the Wayland session exists, so the ring UI is dead | Stage 3 clean reboot and login | `WantedBy=graphical-session.target`; `doctor` checks the daemon sees a display |
| R11 | Orphaned media processes | Stage 1 DoD hangup checks | pgid + `kill_on_drop`; no `pkill` anywhere; Stage 2 dissolves it entirely |
| R12 | Camera negotiates 10fps YUYV — **confirmed live on this C930e** | Already detected | Hard MJPG caps; `doctor` verifies the camera advertises MJPG 720p30 |
| R13 | ufw drops media | Spike ufw row | Structurally eliminated: every flow is outbound, so conntrack admits the returns |
| R14 | **Aggregator latency versus dynamically added live branches** — stuttering in every call, easily misread as a network fault | Stage 2 gate 4 | `min-upstream-latency=250ms` on compositor and audiomixer |
| R15 | **Declined invitee retains a join credential** | Protocol review | `PeerJoining` gating; `Ring` while `InCall` is always `Busy` |
| R16 | **Keyframe-request amplification** — one lossy peer degrades every healthy link | Stage 2 3-way testing | Global 2s `ForceKeyUnit` debounce |

---

## 7. Sequencing

**Serial spine:** spike → control socket → identity/endpoint → tunnel → ring → daemon/call →
bash patch → field test → Stage 2 gates 1–4 → capture → codec → fanout → receive → window →
mesh → cutover.

Media work cannot start before the spike verdict. The window cannot start before the GTK and
aggregator gates. Mesh cannot start before 1:1 in-process media works.

**Parallel from the start:** `proto.rs` and the whole `CallState` test table; `ring.rs`;
`layout()` and the AIMD arithmetic; packaging files; the self-hosted relay container — worth
standing up *during* the spike so the relay row can measure it too.

**Cut points, each shippable:**

1. **After Stage 1** — strictly better than v1: encrypted, any-network, no sshd, no firewall
   rules, with v1's proven media. A legitimate long pause.
2. **After Stage 2 task 11** — polished 1:1, single window, hardware codec, loss recovery,
   bash deleted.
3. **After Stage 2** — full 4-way, rough CLI ergonomics.
4. **After Stage 3** — the real release. Stage 4 is garnish, cuttable line by line.
