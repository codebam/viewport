# Roadmap

What is missing, and why each one is worth doing here rather than by
installing another daemon beside this. The Wayland protocol surface is close to
complete — what is left is desktop integration and shell UX.

Nothing here is a commitment to an order. The list exists so that a gap found
once is written down rather than rediscovered. What lands comes off the list
and is documented where the rest of that subject is.

Six entries came off it at once, which is not the usual way and is worth
naming: the lock screen the shell draws
(`crates/viewport/src/lock.rs`, `data/shell/lock.js`, `idle.lock_command` in
`docs/configuration.md`); the settings panel and the overlay file it writes
(`crates/viewport/src/settings.rs`, `data/shell/settings.js`,
`config.save`/`config.dark_mode`/`output.revert` in `docs/ipc.md`); a keyboard
for every surface that can be opened, and a screen magnifier
(`data/shell/keys.js`, `crates/viewport/src/magnify.rs`, and
`docs/shell-backends.md` for which engine can reach an accessibility bus and
which cannot); the calendar under the clock, and a clock that reads the
locale (`data/shell/calendar.js`, `clock` in `docs/configuration.md`); and
`xwayland.scale`, with the per-toolkit limits recorded in `docs/protocols.md`
rather than promised away.

Before them, the power menu —
`crates/viewport/src/power.rs` for `Suspend`, `Reboot` and `PowerOff` over
logind, `data/shell/power.js` for the rows, `power.action` in `docs/ipc.md`, and
the battery note in `docs/configuration.md`. Before it, the launcher —
`crates/viewport/src/launcher.rs` for the scan and the parse,
`data/shell/launcher.js` for the list and the filter, `launcher.query` and
`launcher.launch` in `docs/ipc.md`, and `menu` in `docs/configuration.md`
for the external menu that keeps `Mod4+d` when one is named. Before that, the
notification centre —
`crates/viewport/src/notification.rs`'s `History` for what is kept and what
leaves it, `data/shell/notifications.js` for the list, `notification.list` and
`notification.forget` in `docs/ipc.md`, and `notifications.history` in
`docs/configuration.md`. Before that, global shortcuts —
`crates/viewport/src/shortcuts.rs`, `docs/protocols.md`'s section of that name
for what is remembered and why, and `shortcuts.pick` in `docs/ipc.md` for the
dialogue. Before that, modifier feedback to a libei client
(`sync_eis_modifiers` in `crates/viewport/src/libei.rs`); before that, the
inhibit interfaces:
`org.freedesktop.ScreenSaver` and `org.freedesktop.impl.portal.Inhibit` in
`crates/viewport/src/inhibit.rs`; and before those, the on-screen keyboard:
`data/shell/osk.js`, `docs/ipc.md`'s `osk.key`/`osk.wanted` and
`docs/configuration.md`'s default bindings.

## Remote desktop

The EI server itself is done and works — `crates/viewport/src/libei.rs`, and
`docs/protocols.md` for how it sits beside the Notify calls. These are the
pieces around it that are not.

**No clipboard for a remote session.** `Start` answers `clipboard_enabled`
with a stated false, and `org.freedesktop.portal.Clipboard` is the interface
that would make it true. Somebody driving this machine from another one
therefore cannot paste into it, which for a remote-support tool is the second
thing tried after the pointer. It is its own portal interface and its own
consent question — reading the desk's clipboard is not the same permission as
typing into it — which is why it is a list entry and not a line in the
existing one.

**No receiver contexts, so no input capture.** The EI server here is a sender:
the client sends input and this compositor performs it. The other direction —
`org.freedesktop.portal.InputCapture`, where a client asks to be *given* the
pointer and keyboard as they leave the edge of a screen — is what a
multi-machine setup like input-leap wants, and it is a different libei context
type on a different portal interface. `EiInput` refuses a receiver context
outright today, which is the right refusal rather than a bug, but it is a
refusal.

## Two protocols, found by the same sweep

Short, because the protocol surface really is close to complete and neither of
these changes that.

**`ext-background-effect-v1` is never instantiated.** The pinned smithay fork
carries it. Every other compositor implementing blur-behind-a-surface is doing
it for clients; here the client that would use it is the shell, a web page
already drawing translucent chrome over whatever is under it, which makes this
the rare protocol whose consumer is in this repository.

**`wp-color-representation-v1` is absent, and so the matrix is guessed.**
`docs/protocols.md`'s hardware-video section says it plainly: a DMA-BUF cannot
carry the YUV matrix, so it is inferred from the picture's height and the
range is taken as narrow. This protocol is the client saying which it is
instead. The guess is right almost always and wrong exactly where nobody
notices immediately — a washed-out frame is easy to blame on the file.

## What the machine underneath does not do yet

This section used to carry two entries. One of them, `xwayland.scale`, has
landed. The other said multi-GPU and whole-device hotplug were missing, and it
was wrong — it was written from `udev.rs`'s header comment, which was stale,
rather than from the code under it, which has had `devices: Vec<Device>`, a
renderer per card, per-device dmabuf feedback and `on_gpu_added`/
`on_gpu_removed` for some time. The entry is gone and the header comment is
fixed. What the audit that found this out *did* find is below: the places
where one card is still assumed, all of which now have an answer, and the one
that does not.

**A buffer no scanout card can import is a hole in one screen.** Per-surface
feedback tells a client which card is displaying it, and `cross_gpu =
"portable"` narrows the default advertisement to what every card can import,
which between them cover every client that listens. A client that ignores
both hands over a buffer the scanout card cannot take, and the surface is
dropped from that screen with one line in the log. The fallback that would
close it — a copy through the primary renderer — is not written, and
`crates/viewport/src/multigpu.rs`'s header argues the shape of it:
`render_pass` is generic over one renderer and compiled twice, a blit needs
two live in one pass, and smithay's own multi-GPU renderer is on its GLES
`GraphicsApi` where this tree is Vulkan wherever Vulkan works.

**None of the multi-GPU path has run on two cards.** Everything above is
reasoned and unit-tested against a machine with one. `docs/debugging.md` has
the symptoms and the log lines to look for, and whoever first boots this on a
laptop with a discrete card beside an integrated one should expect to find
something; that is not pessimism, it is what an untested path is worth.

## What is deliberately not on this list

A file chooser stays with the GTK backend. `viewport-portals.conf` already
sends everything this compositor does not name there, and a file dialog is a
toolkit's job: drawing one here would mean a second file manager to maintain
and no application would be better off for it.

`xdg-toplevel-drag-v1`, `wlr-export-dmabuf-v1` and `ext-transient-seat-v1` are
absent by decision, and the decisions — with what would have to change for
each to come back — are recorded in `docs/protocols.md` rather than here.
Those are answers, not gaps.

A gap found now goes back on this list rather than being fixed quietly, so the
next one is written down here rather than rediscovered.
