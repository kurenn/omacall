# Installing

    omacall doctor          # what this machine is missing
    systemctl --user enable --now omacall.service

No firewall rules. Every flow omacall makes is outbound — hole-punching probes,
the punched path, and the relay connection — so conntrack admits the replies and
an inbound default-deny policy never sees a new flow. v1 needed a ufw rule and
failed silently without it; that failure mode is gone rather than handled.

Suggested keybinding, in `~/.config/hypr/bindings.conf`:

    bindd = SUPER SHIFT, C, Call someone, exec, omarchy-launch-floating-terminal-with-presentation omacall
