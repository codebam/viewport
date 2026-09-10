# FIXES

Every defect found in the code review of this tree, with the fix. One commit
per entry, in the order below unless a later one is a prerequisite. `[x]` means
fixed and committed; the commit subject is named where it is not obvious.

Verification for the whole set: `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`,
and the JavaScript shell suites under `tests/`.

## High — crashes reachable from a client or the shell

- [x] **1. `CaptureOutputRegion` on a removed output panics the compositor.**
  `crates/viewport/src/screencopy.rs:173` returns without initialising the
  `new_id` frame, which wayland-backend treats as a fatal bug. The sibling
  `CaptureOutput` arm at `:124` gets it right. Fix: init a placeholder
  `FrameState` (`copied` set) and `frame.failed()` before returning.

- [x] **2. DRM lease request after GPU removal panics.**
  `crates/viewport/src/handlers/mod.rs:1423` panics when
  `DrmLeaseState::new_with_filter` fails, which is exactly the removed-GPU case
  (`on_gpu_removed` clears `lease_state`). Fix: answer with an empty state
  instead of panicking; never abort a dispatch.

- [x] **3. Dead lock surfaces are walked on every vblank.**
  `state/frame_barriers.rs:185` and `state/frame_clock.rs:90` walk
  `lock_surfaces` without an `alive()` check. Smithay's session-lock
  `destroyed` sets `Defunct` without calling `unlock()`, so a disconnected
  locker leaves dead surfaces until the ~1 s housekeeping sweep — and the next
  vblank panics in `lock_user_data(...).unwrap()`. `render_frame.rs:605`
  already guards. Fix: filter to alive surfaces in both walks.

- [x] **4. xdg resize grab `unset` touches a destroyed toplevel.**
  `handlers/xdg_shell.rs:916` calls `with_pending_state` on `resize_surface`
  when Smithay discards a grab whose focus died. `toplevel_destroyed` never
  clears the grab. Fix: finish/clear the drag and `resize_surface` in
  `toplevel_destroyed`, and guard `unset` with `alive()`.

- [x] **5. `clip_covers` adds untrusted `i32`s unchecked.**
  `views.rs:735` (`clip.x + clip.width`) overflows on a shell-supplied
  `view.layout` clip: debug panic, release misroutes the pointer. Fix: compute
  the bounds with `saturating_add`/`i64`.

- [x] **6. `layout_size` overflows and ignores negative outputs.**
  `state.rs:2418-2427` adds `loc.x + size.w` unchecked and floors at `(0,0)`,
  so `output.configure` with a huge `x` overflows and a monitor at `x < 0`
  falls outside the shell region. Fix: track min and max in `i64`
  (`saturating`), and return an origin-aware extent.

## High — broken behaviour

- [x] **7. The settings overlay is not a patch; it wipes config-file keys.**
  `settings.json` is applied as a second `config::File`
  (`main.rs:377`, `shell_watch.rs`) and `apply_config` resets `tray`,
  `terminal`, `binds`, notification/clipboard history, icon theme, `lid` and
  the cursor keys with `unwrap_or(default)`. One settings-panel save silently
  discards a hand-written `binds` keymap. Fix: merge the overlay into the
  loaded `File` before the single `apply_config`, so absence means "leave it".

- [x] **8. `Stream::node_id` is write-once.**
  `screencast/stream.rs:141,856` never stores the real PipeWire node, so
  `stop_cast` (`state/screencast.rs:127`) never removes a cast (share leaks,
  DMA-BUFs stay pinned) and `remote_point` (`:341`) always fails, so remote
  absolute pointer/touch input is dead. Fix: store the announced node.

- [x] **9. Chromium backend strands events queued during a reload.**
  `viewport-shell-chromium/src/main.rs:205` re-enables `ready` on
  `Page.loadEventFired` without draining `queued`. Fix: drain there.

- [x] **10. CEF backend strands events queued during a reload.**
  `viewport-shell-cef/src/main.rs:523` has the same fault with `QUEUE`. Fix:
  drain and evaluate on `Page.loadEventFired`.

- [x] **11. `click+`/`drag+` leak `SUPPRESSED_BUTTONS`, sticking the next click.**
  `input.rs:1772` suppresses, but the release is consumed by the
  `pending_click` branch (`:1786`) before `release_suppressed`. Fix: release
  the suppression there.

- [x] **12. Input capture leaves held keys held in the seat.**
  `input_capture.rs:1137-1144,1238` moves held keys to `suppressed_keys` and
  then swallows their releases without feeding the seat, so modifiers stick and
  the key is dead afterwards. Fix: feed the release through
  `keyboard.input_intercept(..., |_,_,_| ())`.

- [x] **13. A key held across capture activation never gets its release locally.**
  `input_capture.rs:1241-1249` consumes the transition with
  `update_input_capture_key` and then forwards a second time; Smithay has no
  holder left and forwards nothing. Fix: do not consume it — return `false`.

## Medium

- [x] **14. Overlay `outputs` replaces the whole map.**
  `state/config_apply.rs:444` drops config-file outputs for monitors the panel
  never touched. Fixed together with (7) by a per-key merge.

- [x] **15. `view.close` is a no-op for X11 windows.**
  `apply.rs:130` only calls `toplevel()`. Fix: fall back to `x11_surface()`,
  as `input.rs:2843` already does.

- [x] **16. `initially_allows_capture` fails open on an unsupported regex.**
  `config.rs:1643` returns `false` when the Rust regex crate rejects a
  JS-valid pattern (lookbehind), so a `capture: false` rule stops matching and
  privacy fails open. Fix: treat a build failure as a possible match.

- [x] **17. Local paste after a remote copy serves nothing.**
  `state/screencast.rs:979` records then offers `Owner::History`, but
  `send_selection` serves `clipboard.current()` (`ours`), which `record` never
  sets. Fix: fall back to the newest entry when serving history.

- [x] **18. `Clipboard::ours` is never cleared.**
  `clipboard.rs:200` only clears it if the same text is recorded again, but
  Smithay never reads the compositor's own selection back, so the next
  identical copy is silently dropped. Fix: treat the first client selection
  after a paste as real (`ours.take()` at the top of `record`).

- [x] **19. Notification `owners` grows without bound.**
  `notification.rs:296` inserts per id and only removes on close; history
  eviction never prunes it and ids are monotonic. Fix: drop the owner when the
  history evicts its entry.

- [x] **20. The tray trusts a caller-supplied service name.**
  `tray.rs:373-397` registers arbitrary bus names without checking the sender
  owns them. Fix: for the name form, require `NameHasOwner`/owner == sender.

- [x] **21. Clipboard reads spawn unbounded threads/fds.**
  `clipboard.rs:145` starts a detached reader (and `serve` a writer) per
  request with no timeout or concurrency bound. Fix: drop/replace a previous
  outstanding capture and bound concurrent writers.

- [ ] **22. Unfiltered MPRIS signals feed an unbounded queue.**
  `mpris.rs:156` matches only interface/path, and the channel is unbounded.
  Fix: coalesce refreshes (`try_send` on a bounded channel / an atomic flag).

- [ ] **23. `Shell::wake_with` can lose the ping silently.**
  `shell.rs:631` uses `try_lock` once. Fix: blocking `lock()`, or retry.

- [ ] **24. `inject_pointer` bypasses pointer constraints.**
  `input.rs:751` skips `pointer_constraint`, clamping and drag tracking that
  `pointer_absolute_to` (`:2731`) does. Fix: route through
  `pointer_absolute_to`.

- [ ] **25. The shell pointer grab is cleared by any button release.**
  `input.rs:1997` clears one flag on every release. Fix: track the button that
  set the grab and clear only on its release.

- [ ] **26. Press/release pairing uses `modified_sym()`.**
  `input.rs:1294,1468` can miss when Shift is released first: stray key-up,
  uncancelled long-press, a `repeating+` timer that never stops. Fix: key the
  bookkeeping by the unmodified symbol.

- [ ] **27. Tearing is recorded even when the display refused it.**
  `udev.rs:3421` sets `surface.tearing` regardless of `honoured`, flipping
  frame flags to primary-only. Fix: only set it when honoured, and latch the
  refusal.

- [ ] **28. Removing the active headless output leaves `active_output` dangling.**
  `headless.rs:344` does not do the fallback the DRM paths do. Fix: apply it in
  `output_removed`.

- [ ] **29. CRTCs of just-unplugged outputs stay reserved for the scan.**
  `udev.rs:2316` builds `taken` from all surfaces before `gone` is removed at
  `:2661`. Fix: process `gone` before the connector loop.

- [ ] **30. The seat device is opened and never released.**
  `udev.rs:1897` leaks the libseat device on every open, including failures.
  Fix: keep the fd and `session.close` it when the slot is replaced or the open
  fails.

- [ ] **31. servoshell reads the request line unbounded before the token check.**
  `viewport-shell-servoshell/src/main.rs:557`; also its event queue is
  uncapped (`:388`). Fix: bound the request line and the queue.

- [ ] **32. Unchecked `stride * height` on a client-influenced stream size.**
  `screencast/stream.rs:82,355,943,1121`. Fix: checked `i64`/`u64` arithmetic
  with a size bound.

- [ ] **33. `SelectionTransfer` is never emitted.**
  `screencast/remote.rs:1094` — local→remote paste can never request data.
  Fix: emit it (and `SelectionOwnerChanged(false)`) from the local selection
  path.

- [ ] **34. Portal `Session.Close` skips the frontend check.**
  `screencast/portal.rs:931`. Fix: require `called_by_frontend` like the rest.

- [ ] **35. `ext-image-copy-capture` uses process uptime for `presentation_time`.**
  `state/capture.rs:744,787`. Fix: `clock_gettime(CLOCK_MONOTONIC)`.

- [ ] **36. A second output-management bind cancels the first client's config.**
  `output_management.rs:132` bumps the serial unconditionally. Fix: bump only
  when the head set changed.

## Low

- [ ] **37. `commands.js` builds a shell command with `JSON.stringify`.**
  `data/shell/commands.js:894`. Fix: pass the URL as an argv token instead of
  a shell line.

- [ ] **38. `inject_key` does `keycode + 8` on an unvalidated `u32`.**
  `input.rs:644`. Fix: `saturating_add`.

- [ ] **39. `arm_repeating` can compute a zero interval.**
  `input.rs:2974`. Fix: clamp the interval to at least 1 ms.

- [ ] **40. `msg.rs` discovery aborts on a non-UTF-8 directory entry.**
  `msg.rs:1239`. Fix: `continue` instead of `?`.

- [ ] **41. `bind.add` is uncapped and O(n²).**
  `apply.rs:693,1640`. Fix: cap the runtime-binding count.

- [ ] **42. `settings::save` never fsyncs and uses a fixed temp name.**
  `settings.rs:167`. Fix: write, `sync_all`, rename, fsync the directory; unique
  temp name.

- [ ] **43. Relative `wallpaper` resolves against cwd.**
  `config.rs:1195`. Fix: resolve relative to the config file, like `icc`.

- [ ] **44. `parse_mode` accepts `NaN`/`inf`.**
  `config.rs:996`. Fix: require finite and positive; use the same parser as
  `pick_mode`.

- [ ] **45. `SetScale` accepts `NaN`.**
  `output_management.rs:590`. Fix: `!scale.is_finite() || scale <= 0.0`.

- [ ] **46. MPRIS `trim_start_matches(PREFIX)` strips repeated prefixes.**
  `mpris.rs:402`. Fix: `strip_prefix`.

- [ ] **47. `wp_color_management_surface_v1` never posts `inert`.**
  `color_management.rs:548`. Fix: `is_alive()` guard before `with_states`.

- [ ] **48. A session file with a nested empty split crashes the scrolling layout.**
  `data/shell/session.js:846,918`. Fix: prune empty splits on revive.

- [ ] **49. Notification popups have no cap and can be permanent.**
  `data/shell/session.js:450`. Fix: cap the visible stack.

- [ ] **50. GTK/Servo buffer inbound events in unbounded channels.**
  `viewport-shell-gtk/src/main.rs:315`, `viewport-shell-servo/src/main.rs:120`.
  Fix: bounded channel.
