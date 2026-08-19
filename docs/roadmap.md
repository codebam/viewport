# Roadmap

What is missing, and why each one is worth doing here rather than by
installing another daemon beside this. The Wayland protocol surface is close to
complete — what is left is desktop integration and shell UX.

Nothing here is a commitment to an order. The list exists so that a gap found
once is written down rather than rediscovered. What lands comes off the list
and is documented where the rest of that subject is.

The on-screen keyboard was the last entry taken off it — see
`data/shell/osk.js`, `docs/ipc.md`'s `osk.key`/`osk.wanted` and
`docs/configuration.md`'s default bindings. What has replaced it came off the
EI server (`crates/viewport/src/libei.rs`), which is done and works; these are
the pieces around it that are not.

**Modifier state is not sent back to a libei client.** `ei_keyboard.modifiers`
is how a client learns that the compositor's keyboard state changed — the
person at the machine pressed Shift, or a key the client itself sent latched
Caps Lock — and smithay's wrapper exposes it as
`EiInputSeat::keyboard_modifiers`. Nothing calls it. A client that composes
keystrokes from its own idea of the modifier state therefore drifts from the
seat's, which shows up as a remote session typing capitals nobody asked for
after somebody at the desk touches Shift. Closing it means noticing every
modifier change and telling each connected seat, which is a hook in the
keyboard path rather than anything in the EI code.

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

A gap found now goes back on this list rather than being fixed quietly, so the
next one is written down here rather than rediscovered.
