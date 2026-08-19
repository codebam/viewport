# Roadmap

What is missing, and why each one is worth doing here rather than by
installing another daemon beside this. The Wayland protocol surface is close to
complete — what is left is desktop integration and shell UX.

Nothing here is a commitment to an order. The list exists so that a gap found
once is written down rather than rediscovered. What lands comes off the list
and is documented where the rest of that subject is.

## Network and Bluetooth applets

The bar reports link throughput but cannot join a network. NetworkManager and
BlueZ are D-Bus, the status worker is already a D-Bus and subprocess sampler,
and the picker is a shell overlay like the screencast chooser.

## RemoteDesktop portal

`org.freedesktop.impl.portal.ScreenCast` is implemented here because
xdg-desktop-portal-wlr could only offer whole outputs. RemoteDesktop is the
same interface plus input injection, and the virtual keyboard and pointer
protocols this already speaks are the injection half.

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
