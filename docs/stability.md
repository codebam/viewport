# Stability

## On this page

- [The covered surfaces](#the-covered-surfaces)
- [What is not covered](#what-is-not-covered)
- [Backend tiers](#backend-tiers)
- [The security exception](#the-security-exception)
- [Verification behind the promise](#verification-behind-the-promise)
- [Changing this document](#changing-this-document)

What a Viewport version number promises, and what it does not. The
[CHANGELOG](../CHANGELOG.md) records the project's adherence to semantic
versioning; this file defines the surfaces that promise covers. It is
written for the person deciding whether to build a shell, a widget, a
script or a distribution package against Viewport and be able to upgrade
without reading the diff.

While the version is `0.x`, the surfaces below are still settling: a minor
release may change them, and the CHANGELOG says so when it does. The
direction is additive — the protocol already tolerates a client from the
previous release, and the config already tolerates a file from the next
one — but `0.x` makes no promise yet. **`1.0.0` is the release where this
document becomes a commitment**: breaking a covered surface costs a major
version, and the rules below are how additions and changes are made
without breaking one.

## The covered surfaces

### The IPC protocol

The message vocabulary in `crates/viewport-ipc` — as of 0.11.0, 110
`Request` variants and 43 `Event` variants, 150 distinct wire names
(`session.lock`, `session.unlock` and `shell.command` travel both ways) — on both transports: the page bridge
and the control socket, which speak the same set. The wire names, the
field names, the field meanings and the documented defaults are the
contract. `docs/ipc.md` is its reference; the MIT licence on
`viewport-ipc` exists precisely so an alternative shell can be written
against it, and that shell is who this promise is for.

The tolerance rules are part of the contract, not implementation detail:

- An unknown request type is refused with an `error` event reported
  against the type itself — a client can detect a compositor older than
  its vocabulary without guessing.
- A member present with the wrong type refuses the whole message, with an
  `error` naming the type and the member; it is not treated as absent and no
  part of the message is applied. Absent optional fields take the documented
  default — serde defaults here are protocol, not convenience.
- Unknown fields are ignored by the compositor. A shell forwarding events
  ignores unknown types. Both directions tolerate a peer one release
  ahead.

There is no version handshake, deliberately. The `viewport msg` client is
the compositor binary itself, so client and protocol are never two
versions; a shell ships with the compositor or names the release it was
written against. The additive rules above are the mechanism that makes
that work — a handshake would itself be a protocol change, and the
question is revisited only at a major version, if ever. The C-client
tests (`a_request_added_after_the_c_build_is_still_a_known_type`) pin the
tolerance so it cannot quietly regress, and
`view_capture_requires_an_explicit_boolean` pins the wrong-type refusal.

The trust classes — the read-only queries any same-user client may ask
(`view.query`, `clipboard.query`, `notification.list`, `output.query`),
the privileged requests held to the supervised shell and the compositor
binary, the private events — are covered in the direction of looseness:
a query may gain the right to be asked by anyone (as `output.query` did
in 0.10.0), and taking one away is a breaking change unless it is a
security fix (see below).

### The config schema

The three tiers — the bootstrap `config.json`, the runtime `config.*`
setters, the saved overlay — with their documented precedence (flags over
file over defaults; the overlay over the file; on reload, the file over
the flags). Key names, value shapes, defaults and precedence are covered.

Unknown keys are ignored rather than refused, and that is already law:
"a config written for a later version has to keep working"
(`crates/viewport/src/config.rs`). Within a major version, a config that
loads keeps loading with the same meaning. Validation may be stricter
over the socket than in the file — that asymmetry is documented and
covered as it stands — but a value that worked does not stop working
inside a major.

### The extension contracts

`docs/layout-extension.md` and `docs/custom-widgets.md` are contract
documents, and the clauses they state are covered: what a layout must
plan, render and clear; `markRendered` for every placed window; the
idempotent `clear`; the context surface a layout or widget receives; the
widget `options` schema and its validation at registration; the name
rules; the fallback to `tiling` when an extension fails. The shell may
add helpers and context fields; it may not remove or rename what an
extension already receives. The session-restore blob is opaque and
shell-owned — extensions rely on restore working, not on its format.

### The CLI and the environment

The compositor's flags and the `viewport msg` client surface
(`docs/configuration.md`, `docs/ipc.md`) are covered, including the
tolerance that an unknown flag is logged and ignored. The documented
`VIEWPORT_*` overrides — `VIEWPORT_SHELL_BACKEND`, `VIEWPORT_SHELL_BIN`,
`VIEWPORT_CHROMIUM_BIN`, `VIEWPORT_SERVOSHELL_BIN`, the `*_ARGS` levers,
`VIEWPORT_ALLOW_REMOTE_SHELL`, `VIEWPORT_SOCKET`, and the rest named in
`docs/configuration.md` — are covered. The diagnostic and test variables
(`VIEWPORT_DUMP_*`, `VIEWPORT_FRAME_LOG`, `VIEWPORT_POINTER_DEBUG`,
`VIEWPORT_COMMIT_TIMING`, `VIEWPORT_IPC_TRUST_EXE`,
`VIEWPORT_UNSAFE_NO_CONSENT`, and friends) are explicitly not: they exist
to be changed when debugging needs them to change.

### The NixOS module

Every `programs.viewport.*` option in `flake.nix`'s modules — `enable`,
`package`, `portals.enable`, `shellBackend`, `chromiumBinary`,
`userConfig`, `url`, `urlSpan`, `timeoutMs`, `terminal`, `menu`,
`startup`, `bindsOverride`, `vtSwitching`, `binds`, `settings` — with its
type and default. Additions are free; a removal, rename or type change is
a major. The `viewport-smithay` package alias follows the default backend
by design and is not a pin — name a backend to be held to one.

### Package identity

The `viewport` binary and its `msg` subcommand; the shell helper binaries;
the flake attributes (`.#chromium`, `.#webkitgtk`, `.#wpe`, `.#cef`,
`.#servoshell`, `.#wpewebkit`); the AUR packages and their
`provides`/`conflicts`; the `viewport` wayland-session entry; the portal's
`org.freedesktop.impl.portal.desktop.viewport` name; and
`XDG_CURRENT_DESKTOP=viewport:wlroots`. A distribution or a script may
hard-code these.

### The Wayland protocol surface

The globals `docs/protocols.md` lists as advertised are additive within a
major: a new protocol or a new version of one is free, withdrawing a
global a client could bind is a major. The deliberately-not-advised list
in that document is a decision record, not a promise of future
advertisement.

## What is not covered

- **The compositor's Rust internals.** `crates/viewport` is a GPL binary,
  not a library; nothing in it is an API. `viewport-ipc`'s public Rust
  types are covered insofar as they mirror the wire — the crate exists to
  be reused — but its internal helpers are not.
- **The reference shell's internals.** `data/shell/` is a client of the
  protocol like any other. Its DOM, CSS and JS may be rewritten in any
  release; the protocol it speaks is the covered part.
- **Log text, debug output, temporary paths, benchmark numbers.** These
  are observations, not contracts.
- **Engine-specific behaviour of the experimental backends** (below).
  The shared protocol is identical on every backend; what an engine does
  with it is the engine's own.

## Backend tiers

The six backends are all implemented, but they are not all held to the
same promise:

- **Supported** — `chromium` (the default), `webkitgtk`, `wpe`,
  `servoshell`. Packaged, built by CI or the commit hook, and soaked on
  real hardware. The full promise above applies.
- **Experimental** — `cef` and the embedded `servo`. `cef` is packaged
  and the excluded-crates CI job builds its crate (it sits outside the
  workspace) and runs its unit tests, which cover the shared-bridge
  adapter; its engine paths are not exercised. `servo` is built by hand
  and has not been measured.
  Either may change engine-specific behaviour in any release; a change
  of tier is announced in the CHANGELOG.

## The security exception

A security fix may change behaviour in any release, including a patch.
The cef load-gate fixes and the `file://host` origin refusal in 0.10.0
are the pattern: the wire vocabulary stayed additive, but what the
compositor *does* with a message changed, because what it did before was
wrong. Such changes are recorded under `Fixed` in the CHANGELOG with the
behaviour they replace. This exception is narrow: it covers closing a
hole, not redesigning a surface.

## Verification behind the promise

A promise is worth what stands behind it. `docs/protocols.md`'s Status
section lists what is confirmed on real hardware (DRM/KMS on an RX 7900
XTX, dual 2560x1440) and what is reasoned or unit-tested only. The
headless integration suites and the commit hook cover the protocol and
the policy; the hardware list covers the scanout path. Known gaps are
written down where they live: the multi-GPU copy has not run on two cards
(`docs/roadmap.md` carries the first-boot checklist for whoever does),
DRM lease is untested against a headset, and the auto-HDR transition
paths — fixed in 0.10.0 to re-evaluate on every path that moves their
inputs — await a hardware verification pass, since CI has no HDR screen;
`docs/protocols.md` carries the procedure. The fork's atomic exit restore
(`docs/RUST-REWRITE.md` § Known rough edges) had the same kind of claim
until a Steam Deck run on 2026-11-25 supplied the missing half: the
restore ran from the compositor's own teardown, the kernel accepted its
commit (no `EINVAL`), and a 3000 K night-light LUT was live at quit. What
that run found first is worth keeping in mind for the next claim of this
kind: the restore had only ever been reachable from the DRM device's
`Drop`, which a real exit does not reach, so the unit tests were green
over a path no session took. The one thing a machine cannot assert is the
console's colour on the returning VT; that is a person's check.

## Changing this document

This file is itself a covered surface. Narrowing the promise — removing
a surface, tightening a rule — is a major-version change. Clarifying it,
or adding a surface to it, is free. When the two disagree, the CHANGELOG
entry for the release says which happened.