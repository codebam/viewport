# Roadmap

What is missing, and why each one is worth doing here rather than by
installing another daemon beside this. The Wayland protocol surface is close to
complete — what is left is desktop integration, shell UX, and the two places
underneath where the hardware assumption is narrower than the desks people
have.

Nothing here is a commitment to an order. The list exists so that a gap found
once is written down rather than rediscovered. What lands comes off the list
and is documented where the rest of that subject is.

The power menu was the last entry taken off it —
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

## The desktop this does not draw yet

**Nothing can say "not now".** Do-not-disturb is missing, and the two moments
that most want it are moments this compositor can already see without being
told: a window it made fullscreen, and a screencast session it is itself
serving (`crates/viewport/src/screencast/`). A notification popped over a
shared screen is the one failure mode with an audience. What was in the way of
this is now built: silencing a popup is only acceptable if the notification is
still somewhere afterwards, and `crate::notification::History` is where it
would be — so this is a question of when to draw rather than of what to keep.

**The clock is a line of text, and it is one American's line of text.**
`clockText()` in `data/shell/bar.js` formats it and nothing sits under it. This
is the one entry on this list that needs no compositor change whatever — a
calendar is a grid and a stylesheet, in a shell that already draws dropdowns —
which is worth writing down precisely so that it is not put off as though it
needed one. What the calendar would inherit is the second half of this: the
locale is the literal `'en-US'` and the time is assembled by hand from
`getHours()`, so there is no twelve-hour desk, no other language's month, and
no key to ask for either. A calendar drawn under a clock that is wrong in both
respects is a bigger wrong thing, so the format belongs with the grid rather
than after it.

**Changing the volume shows nothing on screen.** `status.volume` re-samples
the bar, so the number moves wherever the bar happens to be, on whichever
monitor that is, and a bar toggled off says nothing at all. Brightness is
further away than that: the keys `exec brightnessctl`, so the shell never
learns the value and could not draw it if it wanted to. A backlight is read
from sysfs or over logind, which is this side of the line, and both would feed
the same transient indicator.

**Locking is still somebody else's program.** `lock_session()` runs
`idle.lock_command`, which is `swaylock` unless the config says otherwise, and
the compositor's whole part in it is being a correct ext-session-lock *server*
for whatever that program turns out to be — see
`crates/viewport/src/handlers/session_lock.rs`, down to telling the user when
a crashed locker has left the session locked with nothing drawing. Every
other modal surface on the desk is drawn here: the launcher, the power menu,
the notification centre, the on-screen keyboard. The power menu was taken off
this list on the argument that a desk with no keyboard — a touch screen, a
kiosk — could not leave; that same desk cannot get back *in*, because a
locker in another process cannot reach `data/shell/osk.js` and swaylock has
no keyboard of its own. The
surface with the strongest case for being drawn here is the one surface not
drawn here. What makes it harder than the others, and worth saying before
somebody starts: a lock screen that fails open is worse than no lock screen,
so the shell crashing while it holds the lock has to leave the session locked
rather than unlocked, which is the one place a web page drawing the desktop
has to answer for something the rest of the shell does not.

**The settings panel three paragraphs already assume.** `docs/configuration.md`
opens by justifying two tiers of configuration with "a settings UI cannot run
on a display that is not working", and names a settings panel twice more as
the thing the runtime setters are for. Nobody has written it. The runtime tier
it was designed around is three keys deep — `config.border`, `config.gaps`,
`config.wallpaper` — against a config file with dozens, so the argument for
the design and the extent of the design have drifted apart. Either the panel
is a thing to build, and the missing setters get written as it needs them, or
the runtime tier is a scripting surface for `viewport msg` and the
documentation should stop promising a window. The first is the better answer
— outputs, gaps, borders, the wallpaper and dark mode are exactly what
somebody wants to try rather than to edit and reload — but it is the answer
that has to be chosen out loud.

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

Both of these are written in one source comment and nowhere a reader of this
file would look, which is the case this list exists for.

**One GPU, and it has to be the right one.** `crates/viewport/src/udev.rs`
says it in its header — multi-GPU and hotplug of whole devices are not there —
and the desk that wants it is the ordinary one: a laptop with an Intel display
controller and a discrete card beside it. Today one node renders and scans out
everything, so a monitor wired to the other card's ports is a connector this
compositor never sees, and a client rendering on the node that is not the
primary one hands over a buffer that has to cross devices to be composited.
Neither half is exotic any more. Half the machinery for the hotplug side
already exists and was built for something else: `recovery.rs` reopens a card
that was unregistered by a bus reset, which from userspace is the same event
as one being unplugged — what is missing is the
case where the card that appears is a *new* one rather than the one that left.

**Xwayland is handed no scale.** Per-output `scale` is in the config and is
honoured for Wayland clients; nothing in the Xwayland handler, or in
`start_xwayland`, mentions scale or DPI at all. On a HiDPI desk every X11
window is therefore a 1x buffer stretched to a 2x screen, and
the applications most likely to still be X11 are the ones somebody stares at
for hours. The answer may well be that X11 clients stay at 1x deliberately —
several compositors have concluded exactly that, and the alternative is a
per-window scale nobody has made look good — but that answer belongs in
`docs/protocols.md` beside the other decisions rather than being the silence
it is now.

## Nobody who cannot see it can use it

**The shell's accessibility tree reaches nothing.** The desktop here is a web
page, which means it has a real accessibility tree already built by the
engine, and no backend hands it to AT-SPI. Orca finds a screen with a bar, a
launcher and a notification list on it and can read none of them. This is
worth more than it sounds precisely because of how the shell is drawn: an
accessible desktop is usually a large amount of bespoke work, and here most of
it exists and is not plugged in.

**Keyboard reach stops after two surfaces.** `data/shell/launcher.js` and
`data/shell/network.js` bind `keydown`; the tray, the notification centre and
the power menu bind none, so they are opened by a binding and then finished
with the pointer. A power menu that a keyboard cannot choose a row in is the
same gap as a power menu that a touch screen could not open, which is the one
that got it built.

**There is no magnifier.** Not `canvas.zoom` — that is a canvas feature whose
own comment records that input only lands where it is aimed at 1.0, which is
the opposite of what magnification is for. What is meant here is the pointer
dragging a magnified region of the real screen around, with clicks still
landing under the cursor, which is a compositing and input-transform question
and so is squarely this side of the line.

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
