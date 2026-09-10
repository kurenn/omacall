# omacall

Peer-to-peer video calls between machines. No account, no server, no third party in
the media path.

An Omarchy plugin. Calls go directly between machines over QUIC, encrypted end to
end, using [iroh](https://github.com/n0-computer/iroh) for identity and NAT
traversal. When two machines are on the same network the media stays on the LAN;
when they are not, they hole-punch, and a relay carries the call only if that fails.

## Install

Needs `gstreamer`, `gst-plugins-base`, `gst-plugins-good`, `gum` and `libnotify`.
`gst-plugin-va` is optional and gives hardware H.264 on AMD and Intel; without it
calls run on software VP8.

```
cargo build --release
```

## Use

```
omacall daemon            # normally started by omacall.service
omacall id                # your identity
omacall add NAME TICKET   # save someone
omacall call NAME
omacall status            # what the daemon is doing, as JSON
omacall hangup
```

Send someone your ticket over any channel you already trust. They save it, and from
then on the name is enough — the identity is a key, not an address, so it survives
your machine moving networks.

## How it works

A resident daemon holds the identity key and one iroh endpoint, keeping a
connection to a relay so the machine is reachable without port forwarding and
without sshd. Everything else — the CLI, the shell plugin, the tests — talks to it
over a control socket.

Media is one gstreamer pipeline per call: your camera, the encoder, a compositor
mixing every participant into **one window**, and a branch per peer. RTP leaves
through an appsink straight onto QUIC datagrams and arrives at an appsrc, so there
is no loopback hop and no port to open. Full mesh, capped at four people, because
the ceiling is upstream bandwidth rather than CPU.

## What it does not do

- No NAT traversal without a rendezvous. Two machines behind different routers need
  a relay to introduce them; that is what NAT is, not a shortcoming to engineer
  around. The relay sees encrypted bytes and public keys, never content, and only
  when hole punching fails.
- No echo cancellation yet. Use headphones, or enable PipeWire's
  `module-echo-cancel`.
- Four participants maximum.

`PLAN.md` has the architecture, the measurements behind each decision, and the
things that are still open.
