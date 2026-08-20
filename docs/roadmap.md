# Roadmap

What is missing, and why each one is worth doing here rather than by
installing another daemon beside this. The Wayland protocol surface is close to
complete — what is left is desktop integration and shell UX.

Nothing here is a commitment to an order. The list exists so that a gap found
once is written down rather than rediscovered. What lands comes off the list
and is documented where the rest of that subject is.

Global shortcuts were the last entry taken off it —
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

**There is no launcher of its own.** `Mod4+d` is `exec wmenu-run -i`
(`crates/viewport/src/binding.rs`, the `menu` config key), and what that opens
is a layer-shell client with its own theme, its own configuration file and no
idea what the layout is — it cannot open on the monitor that asked for it, or
know which workspace what it launches is going to land on. The shell already
draws pickers steered from the compositor for the clipboard, the two radios
and the screen-share chooser, and `crate::icon::lookup` already walks the
installed icon themes for the tray, so the drawing and the icons are both
solved. What is missing is the `.desktop` scan, which has to be the
compositor's — the page cannot read `XDG_DATA_DIRS` any more than it can read
`/proc` — and an activation token handed to what is started, so a launcher
that knows where the window is going can say so.

**There is no way to turn the machine off.** logind is already on the bus and
already being called: `crates/viewport/src/power.rs` invokes `Suspend` for the
lid policy. `PowerOff`, `Reboot` and suspend-because-somebody-asked have no way
in at all, so leaving this session means `exit` and a TTY. The picker to draw
is the one `data/shell/power.js` draws for power profiles, with three more
rows on it. What the entry has to settle before it is written is what "log
out" means when the compositor *is* the session — quitting is already `exit`,
and a menu offering both without a difference between them is a menu that
lies.

**A notification is a popup and then it is nothing.** The compositor owns
`org.freedesktop.Notifications` (`crates/viewport/src/notification.rs`) and the
shell draws each one as it arrives (`data/shell/session.js`); when it expires
there is no copy of it anywhere. One that came in over a fullscreen game, or
while the screens were blanked, was never seen and cannot be gone back to.
That is what every desktop's notification centre is for, and what a second
daemon is usually installed to keep — while the only copy that ever existed
was in this process.

**Nothing can say "not now".** Do-not-disturb is missing, and the two moments
that most want it are moments this compositor can already see without being
told: a window it made fullscreen, and a screencast session it is itself
serving (`crates/viewport/src/screencast/`). A notification popped over a
shared screen is the one failure mode with an audience.

**The clock is a line of text.** `clockText()` in `data/shell/bar.js` formats
it and nothing sits under it. This is the one entry on this list that needs no
compositor change whatever — a calendar is a grid and a stylesheet, in a shell
that already draws dropdowns — which is worth writing down precisely so that
it is not put off as though it needed one.

**Changing the volume shows nothing on screen.** `status.volume` re-samples
the bar, so the number moves wherever the bar happens to be, on whichever
monitor that is, and a bar toggled off says nothing at all. Brightness is
further away than that: the keys `exec brightnessctl`, so the shell never
learns the value and could not draw it if it wanted to. A backlight is read
from sysfs or over logind, which is this side of the line, and both would feed
the same transient indicator.

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
