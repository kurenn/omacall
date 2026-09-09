# omarchy-call

Peer-to-peer video calls between machines. No account, no server, no third party.

About 200 lines of bash over gstreamer. Machines on the same LAN find each other
automatically; everything else is a line in a contacts file.

## Install

```bash
curl -o ~/.local/bin/omarchy-call https://raw.githubusercontent.com/kurenn/omarchy-call/main/omarchy-call
chmod +x ~/.local/bin/omarchy-call
omarchy-call --setup
```

`--setup` writes the launcher entry, enables LAN announcement, and opens the
firewall. Do this on **both** machines.

## Use

```bash
omarchy-call              # pick a machine from a list
omarchy-call somehost     # call it directly
omarchy-call --selftest   # check the camera and pipelines
```

The callee gets a notification and an answer/decline prompt. Your own camera
appears immediately; theirs appears when they connect. Close a video window to
hang up.

## How machines are found

Three sources, merged, preferring LAN addresses over VPN over anything else:

- **mDNS** on the local network, no configuration at all
- **Tailscale** peers, if it happens to be running
- `~/.config/omarchy-call/contacts`, one `name host-or-ip` per line

## Requirements

Linux: gstreamer (base + good), pipewire, avahi, `gum`, `jq`, and `sshd` running
on the callee. macOS: `brew install gstreamer coreutils`.

## Gotchas

**A firewall drops this silently.** The receiver binds fine, reports no error and
never sees a frame. `--setup` opens UDP 5000-5002; if you skip it and calls stay
black, that is why.

**macOS grants the camera to the terminal, not the script.** Run it from a
terminal that has Camera and Microphone access under System Settings > Privacy &
Security. A process started over ssh can never get that grant, so it produces
zero frames with no error at all.

## Limits

- LAN or VPN only. No NAT traversal, so two machines behind different home
  routers need a VPN, a port forward, or something like Jami instead.
- Media is not encrypted. Fine on a LAN or over Tailscale; do not port-forward
  it to the open internet as-is.
- No echo cancellation. Wear headphones, or load pipewire's `module-echo-cancel`.
- A Mac can place calls but cannot be rung: crossing macOS's session boundary
  from ssh needs root.
