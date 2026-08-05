# Rotating an output

Fixed. `wlr-randr --output DP-3 --transform 90` used to leave the panel mostly
black with a stretched piece of the desktop in one corner; the cause is in
[What it was](#what-it-was), and the rest of this is kept because five attempts
went past it and the reasons they did are worth not repeating.

The hardware is two 2560x1440 displays side by side, DP-1 at +0+0 and DP-3 at
+2560+0, on an RX 7900 XTX, Vulkan renderer, DRM backend.

## The symptom, precisely

What it looked like, before the fix below:

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

- ~~**The renderer's transforms.**~~ This is where it was. The GPU tests passed
  because they asserted the renderer's own inverted convention back at it: the
  rotated test bound a transposed target, which is not the buffer a rotated
  output scans out. A test that agrees with the code says nothing about whether
  either agrees with Smithay.
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

## What it was

The Vulkan renderer had the two sizes the wrong way round.

The primary plane was never the problem: the swapchain is allocated at
`mode.size()` and the plane is configured `src: dmabuf.size(), dst:
current_size` — both the landscape mode, both agreeing. The stretch was in the
image drawn into that buffer.

Smithay's convention, which `GlesRenderer::render` is the statement of:

- `Renderer::render` is given the **framebuffer** size. GLES sets its viewport
  to it *before* it looks at the transform at all.
- Only then does it swap the axes for transposed transforms, and that swapped
  size is the space every `dst` rectangle, every damage rectangle and
  `Frame::output_size` are in. Smithay's damage tracker agrees: it lays elements
  out against `output_transform.transform_size(output_size)`.

So a 2560x1440 panel rotated 90 degrees scans out a 2560x1440 framebuffer
holding a 1440x2560 desktop. The renderer's `src/transform.rs` had it
the other way about — it normalised into clip space by the *transformed* size
and treated `dst` as being in the *untransformed* one — so the desktop was
squeezed into a fraction of the framebuffer and hung off the edge. The GPU tests
passed throughout because they encoded the same inversion: they bound a
transposed target and drew into it.

The second half of it is that Smithay has two functions that look like they say
the same thing and do not. `Transform::transform_point_in` maps `Flipped90` to a
bare transpose `(y, x)`; `Transform::matrix()` — what GLES, and so everything
downstream of a renderer, actually uses — is that transpose *and* a half turn.
The same disagreement is in `transform_rect_in`, which the scissor went through.
Deriving from `transform_point_in` left `flipped-90` and `flipped-270` upside
down while the four rotations were right. `position` and the new
`framebuffer_rect` both go through `matrix()` now, and one test pins `_90` and
`Flipped90` to different quadrants so they cannot quietly converge again.

Everything else here was real and none of it was the cause.

## Testing it

`./scripts/run-drm.sh` on a real session, then `wlr-randr --output DP-3
--transform 90`. The log goes to `/tmp/viewport-drm.log` and carries
`DP-3: reshaped to _90` when the reshape path runs — if that line is absent the
change never reached the compositor and nothing downstream matters.

`grim -o DP-3 out.png` composites through the same `frame_for` the scanout uses
and comes back at the transformed size — 1440x2560 for a rotated 2560x1440
panel — with the desktop upright in it.

A capture is still not proof about the panel. It goes through an offscreen
target of its own, sized and oriented for the client's buffer, so the one thing
it cannot exercise is the scanout framebuffer's shape — which is exactly what
was wrong. Both were checked here: `flipped-90` and `flipped-270` were upside
down on the panel while their captures looked correct. Look at the screen.
