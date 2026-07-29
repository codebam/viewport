# Rotating an output is broken

`wlr-randr --output DP-3 --transform 90` leaves the panel mostly black, with a
stretched piece of the desktop in one corner. Four attempts have each fixed
something real without fixing this. What follows is what is known, what is
ruled out and with what evidence, so none of it has to be rediscovered.

The hardware is two 2560x1440 displays side by side, DP-1 at +0+0 and DP-3 at
+2560+0, on an RX 7900 XTX, Vulkan renderer, DRM backend.

## The symptom, precisely

After the transform is applied:

- The panel is black apart from a region in its bottom-left — which, given the
  rotation, is the *top-left* of the composite.
- What is drawn there is **stretched**, not merely cropped or offset.
- It is live: holding Mod4 makes the bar appear and disappear in that region,
  so frames are reaching the screen. It is not frozen.
- The rest of the panel stays black.

A stretched sub-region is a scale mismatch, not a placement one. That points at
the primary plane's `src` and `dst` disagreeing — a framebuffer of one shape
being scanned out into a rectangle of another — rather than at anything in the
scene graph.

## Ruled out, with evidence

- **The renderer's transforms.** All eight are implemented in
  `crates/viewport-vulkan/src/transform.rs` and covered by GPU tests, including
  `a_rotated_output_puts_pixels_where_the_rotation_says`, which pins where a
  90-degree draw lands. `VIEWPORT_REQUIRE_GPU=1 cargo test -p viewport-vulkan
  rotated` passes.
- **DrmOutput not hearing about the change.** `initialize_output` is given the
  `Output` itself (`crates/viewport/src/udev.rs`), so the mode source is
  `OutputModeSource::Auto` and size, scale and transform are re-read every
  frame.
- **The layer map.** It held the pre-rotation shape, which made the usable area
  landscape on a portrait screen. Fixed in `ViewportState::output_reshaped`.
  The shell now lays a rotated DP-3 out correctly — the log shows columns of
  703x2540 inside 2560..4000, which is right for a 1440-wide portrait output.
- **The shell not repainting.** It painted 1418 frames in the minute after one
  rotation.
- **An empty frame.** A capture of DP-3 reports `6 element(s), 3 window(s),
  shell yes`.
- **The page being the wrong size.** `the shell is 4000x2560 now, for DP-1
  2560x1440+0+0 Normal, DP-3 1440x2560+2560+0 _90` — the layout bounding box
  grew and WebKit was resized to it.

## Fixed along the way, none of them the cause

- `OutputInfo.transform` was `Normal` unconditionally, so the shell was told a
  rotated monitor was not rotated.
- The capture path composited in the output's logical space into a mode-sized
  buffer with `Transform::Normal`, so a screenshot of a rotated screen came
  back lying on its side. This actively misled the investigation: the first
  `grim` looked like a double rotation. It takes size, scale and transform from
  the output now (`read_output_pixels`, `render_output_into`), while a *window*
  capture still does not — a window is not rotated by the screen it is on.
- `output_reshaped` cleared `surface.pending`, copied from the VT-switch path
  where every flip had died with the session. On a live output a flip is
  usually in flight, and forgetting it made the compositor stop waiting for
  vblank: it re-rendered at once, found no damage, committed nothing, and span.
  Half a million lines of `nothing to draw`.
- The first two attempts went into `Request::OutputConfigure`, which is the
  *shell's* path. `wlr-randr` speaks wlr-output-management, which is
  `ViewportState::apply_output_configuration`. Neither fix was reached. The
  absence of the log line said so and was not checked. Both paths now call
  `output_reshaped`, as does a mode change.

## Where to look next

The lead is the primary plane. In `smithay/src/backend/drm/compositor/mod.rs`
the primary plane is configured with `src: Rectangle::from_size(dmabuf.size())`
and `dst: Rectangle::from_size(current_size)`, with a comment that the transform
is deliberately *not* applied to that plane because "this is handled by the
dtr/renderer". So two things must agree: the buffer the renderer produced, and
`current_size`. If the renderer draws a 1440x2560 image (the logical, portrait
size) while `dst` is the 2560x1440 mode — or the reverse — the result is exactly
a stretched image in one corner.

Concretely:

1. Turn on `smithay::backend::drm` at debug and read the `PlaneConfig` line for
   DP-3 after a rotation. Before rotation it reads `src: 2560x1440, dst:
   2560x1440`. Whatever it reads after is the answer. Note that a run captured
   for this document showed *no* `PlaneConfig` lines after the rotation at all,
   which contradicts the screen visibly updating — so check the log level and
   the window being grepped before trusting that.
2. Check what `current_size` resolves to for `OutputModeSource::Auto` under a
   transform: the mode size, or the transformed logical size.
3. Check which size the swapchain allocates after `reset_buffers` on a rotated
   output, since the framebuffer must stay the mode's shape while the render
   target is described in logical terms.
4. `crates/viewport/src/state.rs:frame_for` positions elements against
   `output_geometry`, which is the *transformed* rectangle (1440x2560). If
   smithay's own damage tracker also applies the transform, one of the two is
   doing it twice — though note that would rotate the content rather than
   stretch it, so this is second on the list.

## Testing it

`./scripts/run-drm.sh` on a real session, then `wlr-randr --output DP-3
--transform 90`. The log goes to `/tmp/viewport-drm.log` and carries
`DP-3: reshaped to _90` when the reshape path runs — if that line is absent the
change never reached the compositor and nothing downstream matters.

`grim -o DP-3 out.png` composites through the same `frame_for` the scanout uses
and is trustworthy again, so a capture and the panel should now agree; if they
disagree, the difference is itself the clue, because the only thing between them
is the plane configuration.
