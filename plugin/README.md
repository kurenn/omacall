# omacall — Omarchy plugin

Call state in the bar, and one click to place a call.

## Install

The plugin is the frontend; the daemon is a separate package, because an Omarchy
manifest cannot declare dependencies.

    sudo pacman -S omacall
    systemctl --user enable --now omacall.service
    omarchy plugin add https://github.com/kurenn/omacall.git --enable

If the daemon is missing the widget dims, and clicking it offers to install. That
indirection exists because an Omarchy manifest cannot declare dependencies and has
no install hooks, so the plugin cannot pull the daemon in by itself.

## What the bar shows

- grey dot — idle
- ringing phone, pulsing — a call is ringing
- blue dot and a count — in a call, direct
- amber and `· relay` — in a call, relayed

That last one matters: a relayed call adds real latency, and without saying so
it just reads as "omacall is laggy" when it is actually carrier NAT.

Left click opens the picker, right click hangs up.

## Remove

    omarchy plugin remove io.github.kurenn.omacall
