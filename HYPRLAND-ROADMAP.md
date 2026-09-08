# Hyprland parity roadmap

What Hyprland has that this tree does not, as of the 2026-09 comparison —
re-run against `v0.2.0` (`88e543d`) and Hyprland `0.56` — over `docs/roadmap.md`,
the CHANGELOG and the keybinding and rules sections of `docs/configuration.md`.
Everything else Hyprland offers is already present, so it is not repeated here:
submaps (here `mode`), scroll and mouse binds, tabbed and stacked containers,
pseudotile, swallow rules, pin, a scratchpad, per-output VRR including
`game-or-video` off the committed `wp_content_type_v1`, tearing, HDR, fractional
scale, the on-screen keyboard over input-method-v2, screencopy and
image-copy-capture, the lock, inhibit and shortcuts portals, `ext-workspace`,
`wlr-output-management`, DRM lease, pointer constraints and relative pointer,
layer-surface rules, window opacity policy, live and discrete touchpad gestures,
the magnifier, session restore, and a `theme` block the shell paints.

| # | Gap | Hyprland does | Here | Status |
| --- | --- | --- | --- | --- |
| 1 | Remote-desktop clipboard | — | `Start` answers `clipboard_enabled` false, in `screencast/remote.rs` and in `input_capture.rs`; `org.freedesktop.portal.Clipboard` is the interface that would make it true (roadmap §Remote desktop) | open |
| 2 | `wp-color-representation-v1` | — | was absent, so the YUV matrix of a DMA-BUF is guessed from picture height and the range taken narrow (roadmap §A protocol found by the same sweep) | done — server in `color_representation.rs`, consumed by the Vulkan renderer at import |
| 3 | Shell blur via its own DMA-BUF | blur lives in the compositor | `ext-background-effect-v1` works for ordinary surfaces; the shell is `render::Shell` from an imported buffer and carries no blur-region metadata (roadmap §What the machine underneath does not do yet) | open, and gated on 7 |
| 4 | Cross-GPU fallback copy | copies a foreign buffer to the scanout card | a buffer the scanout card cannot import drops the surface from that screen; `multigpu.rs` now argues the copy out loud instead of leaving it unwritten — per-surface feedback, then `cross_gpu = "portable"`, then the hole and one log line (same section) | open, argued against |
| 5 | Plugin system | `hyprctl plugins` + a whole ecosystem | there is no plugin loader; a new layout model is one file and three name lists (CHANGELOG [Unreleased]) | unlikely, by design |
| 6 | `env` / `exec-env` config keys | `hl.env(NAME, VALUE)` before the display server starts | session variables (`XDG_CURRENT_DESKTOP`, portal env) are set internally; `VIEWPORT_*` is read, never written; no user-facing key | open |
| 7 | Blur on the card that is driving the screen | blur is drawn by whatever renderer the monitor is using | `docs/protocols.md` §Background effects: the global is created only where GLES proved it can blit and compile the shader — nested and headless. The DRM backend gets no global at all, *including* on a card that fell back to GLES, because `VulkanFrame` offers neither `FrameContext` nor `BlitFrame` and has no blur pipeline. So on the hardware this is shipped for, no client gets blur, and 3 cannot land before it does | open — the largest one here |
| 8 | Focus follows the pointer | `input:follow_mouse`, with `follow_mouse_threshold` and `follow_mouse_shrink` | keyboard focus moves on a button press, on a touch, on `view.focus` and on activation — never on motion. `refresh_pointer_focus` retargets the *pointer*, which is a different thing and is not this. No config key exists | open |
| 9 | What a bind can be | flags: `locked`, `release`, `click`/`drag` with `binds:drag_threshold`, `long_press`, `repeating`, `non_consuming`, `auto_consuming`, `ignore_mods`, `transparent`, `description`, per-device `device`, `submap_universal`, `allow_input_capture`, `dont_inhibit` | `Binding` is modifiers, keysym *or* button *or* wheel, an action and a mode. Nothing a config file binds fires while locked — `input.rs`, "No binding fires while locked — one that spawns a terminal would put it on top of the lock screen" — and the only keys that still act there are the built-in VT switches. Nothing fires on release, held keys do not repeat outside the typed volume and brightness actions, and no bind can pass its chord through to the client as well as acting | open |
| 10 | Window-rule vocabulary | matches `class`/`initial_class`/`initial_title`/`modal`/`float`/`fullscreen(_state_*)`/`group`/`pin`/`xwayland`/`content`/`focus`, and ~50 effects, last match wins, with named rules that can be enabled and disabled at runtime | matches `app_id`, `title`, `tag` and the active `workspace`, in first-match order; effects are `workspace`, `floating`, `pinned`, `pseudotile`, `swallow`, `capture`, `opacity` and a rect. No rule is named, so none can be turned off; no effect reaches decoration, animation, aspect ratio, min/max size, focus, idle inhibit, blur, dimming, tearing or pointer confinement | open, and the cheapest one here |
| 11 | Named special workspaces, and what a workspace can be given | up to 97 `special:NAME` spaces toggled by name, and workspace rules carrying `on-created-empty`, `persistent`, `default_name`, `no_border`, `no_rounding`, `decorate`, `float_gaps`, `animation`, per-workspace `layout_opts` | one scratchpad and one pinned state. `workspaces` per number takes `output`, `layout`, `tiling_mode` and `gaps` — that half is done; nothing starts an application when a space is created, and a name is a label on a number rather than an identity | open |
| 12 | Groups that behave | `group:auto_group`, `drag_into_group`, `merge_groups_on_drag`, `group_on_movetoworkspace`, lock and `deny_from_group`, and a groupbar with its own geometry and title rendering | tabbed and stacked containers exist and are the same idea; the parts missing are the automatic ones — a window never joins the focused container by itself, dragging one onto a tab never makes a group, and nothing can lock a container against new arrivals | open |
| 13 | Motion as configuration | `hl.animation{leaf, speed, bezier}` / `{spring}` over an inherited tree — `windows`/`windowsIn`/`windowsOut`/`windowsMove`, `layers`, `border`, `borderangle`, `shadowangle`, `fadeDim`, `fadePopups`, `workspaces` with styles (`slide`, `popin`, `gnomed`, `slidefade`, `fade`, `zoom`) — and a per-window `animation` rule effect on top | the shell's motion is `--anim`, `--anim-slow` and `--ease` in `data/shell/shell.css`, switched off whole under `prefers-reduced-motion`. Right answer for a shell that is a web page — but there is no key that reaches it, so tuning a duration means shipping a stylesheet | open |
| 14 | Colour past the parametric path | ICC profiles and their VCGT (`render:icc_vcgt_enabled`), automatic HDR from SDR (`render:cm_auto_hdr`), `cm_sdr_eotf`, per-window `tonemap` and `no_auto_hdr` | `color_management.rs` says it plainly: "The parametric path is implemented. ICC profiles are not." There is no tone-mapping step anywhere in the tree, and HDR is a per-output switch rather than a conversion | open |
| 15 | A config that computes | the config is a Lua program: `hl.on(event, fn)`, `hl.timer`, `hl.dsp.*` from code, `hl.get_*`, multiple `exec-once`, conditionals over the whole keymap | the bootstrap tier is inert JSON by argument, and the argument is in `docs/configuration.md`: it has to start on a broken display, so nothing it reads may execute. The scripting that exists is the shell — IPC events, `subscribe`, `layout_extensions` — and one `startup` command | unlikely, by design |
| 16 | `hyprcursor` themes | hyprcursor themes — their own format, per-size and per-monitor scale shapes — behind `cursor:enable_hyprcursor` | `XCURSOR_THEME` and `XCURSOR_SIZE` only, which is the answer every other compositor gives and the one XCursor-shaped themes understand | open, small |

Item 2 landed on both sides of the pin: `crates/viewport/src/color_representation.rs`
is the server — advertisement, validation, double-buffered state and the
commit-time format check — and the renderer takes the declaration from the
surface's data map when it imports the buffer, in place of the height-based
matrix guess (`viewport-vulkan` `a5c9807` and `1e752e1`; the `rev` pin needs
the co-bump described in the root `Cargo.toml`). `docs/protocols.md`,
`docs/roadmap.md` and the CHANGELOG moved with it, and
`tests/color-representation-client.c` runs the whole conversation over a real
socket.

7 is the one to read twice, because it is not the entry the tree had before.
The old framing was that blur was missing for the shell only. What the sweep
found is that `ext-background-effect-v1` is never advertised on DRM at all —
correctly, since a compositor-wide global cannot promise an effect that only
some of the outputs would draw, and Vulkan is preferred per card — which leaves
blur running on nested and headless sessions and nowhere else. Item 3 is
downstream of that: the shell cannot get what no client on the machine gets.

The rest, named so that the next sweep does not have to rediscover them, and
not worth a row each: `input:scroll_factor` and `touchpad:scroll_factor`,
`emulate_discrete_scroll`, `scroll_button` with its lock, and `drag_3fg`;
`kb_model`, `kb_rules`, `numlock_by_default` and `kb_file` beside the layout,
variant and options this has; tablet region mapping and pressure ranges, where
this maps the pen to the whole desk and passes the pressure through as it is;
`cursor:hide_on_touch`, `hide_on_key_press`, `no_warps` and
`warp_on_change_workspace`, where this has a theme, a size and
`hide_after_ms`; a headless output created from a running session rather than
one only `output.test_add` may add; and `misc:enable_anr_dialog`, the
"window is not responding" check that has no equivalent here — the liveness
watchdog in `ipc.rs` is watching the shell.

Two things this audit asked and answered as present, recorded so they are not
re-listed: `pointer_constraints` covers what Hyprland's `confine_pointer` rule
is for on the client's side, though no rule can ask for it; and `opacity`
multipliers for active, inactive and fullscreen do the job `dim_inactive` is
used for, even though they multiply alpha rather than pushing the colour
toward black.
