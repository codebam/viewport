# Hyprland parity roadmap

What Hyprland has that this tree does not, as of the 2026-09 comparison
against `docs/roadmap.md`, the CHANGELOG and the keybinding/rules sections of
`docs/configuration.md`. Everything else Hyprland offers — submaps (here
`mode`), scroll and mouse binds, tabbed and stacked containers, pseudotile,
swallow rules, per-output VRR, tearing, HDR, fractional scale, the on-screen
keyboard over input-method-v2, screencopy, the lock, inhibit and shortcuts
portals — is already present, so it is not repeated here.

| # | Gap | Hyprland does | Here | Status |
| --- | --- | --- | --- | --- |
| 1 | Remote-desktop clipboard | — | `Start` answers `clipboard_enabled` false; `org.freedesktop.portal.Clipboard` is the interface that would make it true (roadmap §Remote desktop) | open |
| 2 | `wp-color-representation-v1` | — | was absent, so the YUV matrix of a DMA-BUF is guessed from picture height and the range taken narrow (roadmap §A protocol found by the same sweep) | done — server in `color_representation.rs`, consumed by the Vulkan renderer at import |
| 3 | Shell blur via its own DMA-BUF | blur lives in the compositor | `ext-background-effect-v1` works for ordinary surfaces; the shell is `render::Shell` from an imported buffer and carries no blur-region metadata; Vulkan captures nothing yet (roadmap §What the machine underneath does not do yet) | open |
| 4 | Cross-GPU fallback copy | — | a buffer the scanout card cannot import drops the surface from that screen; the copy through the primary renderer is unwritten, and none of the multi-GPU path has run on two cards (same section) | open |
| 5 | Plugin system | `hyprctl plugins` + a whole ecosystem | there is no plugin loader; a new layout model is one file and three name lists (CHANGELOG [Unreleased]) | unlikely, by design |
| 6 | `env` / `exec-env` config keys | user-set environment for the session and children | session variables (`XDG_CURRENT_DESKTOP`, portal env) are set internally; no user-facing key | open |

Item 2 landed on both sides of the pin: `crates/viewport/src/color_representation.rs`
is the server — advertisement, validation, double-buffered state and the
commit-time format check — and the renderer takes the declaration from the
surface's data map when it imports the buffer, in place of the height-based
matrix guess (`viewport-vulkan` `a5c9807` and `1e752e1`; the `rev` pin needs
the co-bump described in the root `Cargo.toml`). `docs/protocols.md`,
`docs/roadmap.md` and the CHANGELOG moved with it, and
`tests/color-representation-client.c` runs the whole conversation over a real
socket.
