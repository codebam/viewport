# Roadmap

What is missing, and why each one is worth doing here rather than by
installing another daemon beside this. The Wayland protocol surface is close to
complete — what is left is desktop integration and shell UX.

Nothing here is a commitment to an order. The list exists so that a gap found
once is written down rather than rediscovered. What lands comes off the list
and is documented where the rest of that subject is.

## Tray menus (`com.canonical.dbusmenu`)

The tray itself is done — see
[`docs/protocols.md`](protocols.md#the-system-tray). What is missing is the
menu. An item that answers `ContextMenu` draws its own window and works today;
one that publishes a `com.canonical.dbusmenu` object instead expects the host
to fetch its layout and draw it, and this does not yet. That is a recursive
`GetLayout` over a variant tree, a shell overlay to draw it in, and `Event`
calls back for clicks — and it is what stands between the current tray and the
menus GTK applications ship.

## MPRIS media widget

`mpris` appears in one place today — a keybinding that shells out to
`playerctl`. The bar already samples PipeWire for sink and source volume on a
timer, so a player's title, artist and play state can ride the same tick, with
previous, play/pause and next as buttons that call back over the bus.

## Clipboard history

The compositor brokers `wl_data_device`, primary selection and
`wlr-data-control`, so it already sees every selection offered on the session.
Keeping the last N text and image offers and drawing a picker in the shell
gives a clipboard manager with no second process, the same argument that put
notifications in here.

## Network and Bluetooth applets

The bar reports link throughput but cannot join a network. NetworkManager and
BlueZ are D-Bus, the status worker is already a D-Bus and subprocess sampler,
and the picker is a shell overlay like the screencast chooser.

## RemoteDesktop portal

`org.freedesktop.impl.portal.ScreenCast` is implemented here because
xdg-desktop-portal-wlr could only offer whole outputs. RemoteDesktop is the
same interface plus input injection, and the virtual keyboard and pointer
protocols this already speaks are the injection half.

## Screenshot portal

Still routed to xdg-desktop-portal-wlr in `data/portal-config`, which captures
outputs and nothing else — the exact limit that made ScreenCast worth
implementing here. Window and region screenshots need the window list this
compositor already holds.

## Battery and power

Nothing here talks to UPower. The idle configuration covers a machine left
alone; a laptop also has a battery to show, a lid to react to and a power
profile to switch.

## On-screen keyboard

`input-method` and `virtual-keyboard` are wired up and touch is complete, so
the missing piece is a keyboard to type on. The shell is a web page, which
means an on-screen keyboard is HTML and CSS rather than a second Wayland
client.

## Layouts as an extension point

Five layouts ship: tiling, scrolling, solar, matrix and canvas. Adding a sixth
means writing a file and editing `index.html`. Documenting the contract a
layout implements — what it receives, what it must measure, what it may not
transform — would turn the strongest thing about this design into something a
user can extend without patching the shell.

## Configuration reload

Saving a shell file reloads the page under `--watch-shell`, and `Mod4+Shift+c`
reloads it by hand. The configuration file should have the same property, so
that editing a binding or a gap does not cost a session.
