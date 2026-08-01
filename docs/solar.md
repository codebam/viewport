# The solar layout

One window in the middle, the rest in orbit around it. A third layout model
beside `tiling` (i3-style splits) and `scrolling` (niri's strip), selected with
`"layout": "solar"` in the config file or `shell layout.model solar` at
runtime.

## Where it lives, and why

The compositor has no layout policy at all. A window is mapped only once the
shell has sent a `view.layout` for it (`crates/viewport/src/views.rs:9`), so
every rectangle on this desktop is decided in `data/shell/`. There is no
compositor-side `recalculate_solar_layout()` to write: it would never be
called. The engine is `data/shell/solar.js`, and the compositor's share is the
config key, the keybindings, and the three wire fields the model needs —
`scale`, `opacity` and `floating` — all of which already exist.

It is also the first mode that computes rectangles. `tiling` and `scrolling`
build nested flexboxes and let the browser do the arithmetic; an orbit is not
expressible in flexbox, so solar positions absolutely. That is not a new
mechanism either — floating windows have always been positioned rather than
laid out (`geometry.js`) — but it does mean `shell.md`'s "layout is CSS, not
arithmetic" has one documented exception, and this file is it.

What has *not* changed: geometry is still measured, never assumed. Solar
computes where it wants each window, writes it as inline style, and
`reportGeometry()` measures the result and reports that. A transition mid-flight
is reported at wherever it actually got to.

## The three tiers

| Tier | Which windows | Size | Drawn at | Opacity |
| --- | --- | --- | --- | --- |
| Sun | the focused window | `√α` of each output dimension | scale 1 | 1.0 |
| Inner orbit | the next 8 | `ρ` of the sun | scale 1 | 0.78 |
| Outer orbit | everything after | `ρ` of the sun | scale `σ` | 0.40 |

The sun is the focused window, always. That is the one hard rule the model
keeps: **the window you are typing into is never shrunk, never translucent and
never occluded.** Everything else is decoration.

An outer-orbit window keeps its real size and is *drawn* at 40% —
`view.layout`'s `scale` field, the same mechanism the overview uses. This is not
a detail. Focus moves constantly, and every focus change reshuffles the orbits;
resizing cold clients each time would ask every application on the workspace to
relayout itself several times a second. Drawn scale costs the compositor a
surface-tree walk and costs the client nothing.

## The model

Let the output's usable area — what `.windows` measures, so panels and the bar
are already subtracted — be

```
A = (Ax, Ay, Aw, Ah)          C = (Ax + Aw/2, Ay + Ah/2)
```

### The sun

`α` is the fraction of the area the sun takes (`sunArea`, default 0.60).
Keeping the output's aspect ratio:

```
Sw = round(Aw · √α)           Sx = Ax + (Aw − Sw)/2
Sh = round(Ah · √α)           Sy = Ay + (Ah − Sh)/2
```

so `Sw · Sh = α · Aw · Ah`. At α = 0.60 the sun is 77.46% of each dimension and
the margin around it is 11.27% of each — narrower than a satellite, which is
why satellites are partly eclipsed by it rather than ringed neatly around it.
Lowering α with `solar.mass` widens the ring; that is the knob.

### Slot projection

Both orbits place a window's *centre* at an offset from `C` given by an angle.
θ is measured clockwise from screen-right with y increasing downward, so θ=0 is
right, 90 is down, 180 is left, 270 is up.

```
project(θ, Rx, Ry):
    c = cos θ,  s = sin θ
    t = 1 / max(|c|, |s|)
    return (Rx · c · t,  Ry · s · t)
```

The Chebyshev normalisation `t` traces the boundary of the **rectangle**
`[−Rx, Rx] × [−Ry, Ry]` rather than the ellipse inscribed in it. Three reasons:

- θ = 45° lands on the actual corner, not at `0.707R` inward. Corners are where
  a centred sun leaves the most free space, so that is where a satellite should
  go.
- `|dx| ≤ Rx` and `|dy| ≤ Ry` hold for every θ. Choose `Rx = (Aw − w)/2` and the
  window is inside the output *by construction* — no clamping, and no window
  ever half off the screen at some awkward angle.
- "Sit near edges and corners", which is what the outer orbit is for, is
  literally what a rectangle boundary is.

### Inner orbit

Satellites are sized against the sun and orbit inside an inset rectangle:

```
Iw = round(ρ · Sw)                    ρ  = innerRatio      (0.42)
Ih = round(ρ · Sh)
Rx = κ · (Aw − Iw)/2                  κ  = innerClearance  (0.72)
Ry = κ · (Ah − Ih)/2
(dx, dy) = project(θ_j, Rx, Ry)
x = round(Cx + dx − Iw/2)             y = round(Cy + dy − Ih/2)
```

`κ` is the whole visual character of the layout. At κ = 1 satellites hug the
screen edge and the ring is wide open; at κ = 0.72 they sit inside the outer
band, tucked under the sun's edge, and the outer band is left free for cold
windows. Below about 0.5 a satellite disappears behind the sun entirely.

### Outer orbit

Same nominal size, drawn small, hard against the edge:

```
Ew = round(σ · ρ · Sw)                σ = outerScale       (0.40)
Eh = round(σ · ρ · Sh)
lap = min(⌊j / slots⌋, maxLaps)       λ = outerLapInset    (0.86)
Rx = λ^lap · (Aw − Ew)/2
Ry = λ^lap · (Ah − Eh)/2
```

`Ew` is the size on screen; the client is configured at `Ew / σ` and the
compositor shrinks it. Windows past one lap of the slot table step inward by
`λ` per lap so the twenty-fourth window is not exactly underneath the ninth.

### Slots, and why they are fixed

Each orbit has a fixed table of angles, filled in order:

```
inner  45, 315, 135, 225, 0, 180, 90, 270     corners first, top last
outer  90,   0, 180,  45, 135, 315, 225, 270  bottom first, top last
```

Fixed slots rather than `θ_j = 2π·j/n` redistribution, deliberately. Opening a
third window must not move the first two — a layout where every window shifts
whenever any window opens is one you cannot build muscle memory for. Corners
come first because they have the most clearance from a centred sun; the top
comes last because the bar is there.

The consequence is that a small workspace reads as a partial arc and a full one
closes into a ring, without either being a special case.

### Stacking

The compositor gives the shell exactly two z-bands. `restack()`
(`crates/viewport/src/state.rs:5162`) raises every window the shell marked
`floating` above every window it did not, and that is the only stacking rule the
compositor keeps for itself.

So: **the sun is sent `floating: true`, the orbits are not.** The sun is above
everything, which is the rule the model exists to enforce. Relative order
*within* the orbits is not expressible and is not needed — inner and outer never
overlap, because `κ = 0.72` puts the inner ring inside the band the outer ring
occupies.

### Two monitors

`solarField` picks between them; `solar.field` toggles.

**`binary` (default).** Each output runs its own independent system for
whichever workspace it is showing. Both have a sun; only the one holding
keyboard focus is at full opacity, the other — the companion star — rests at
0.90. Which window is a workspace's sun is remembered per workspace, so a
workspace nobody is looking at still has a centre and does not reshuffle when
you come back to it.

**`lagrange`.** The focused output keeps the sun and the inner orbit; its outer
orbit is parked on the other monitor, spread over that whole screen as a field
of background applications at `lagrangeScale` (0.55) — bigger than an outer
orbit because it has a screen to itself rather than a margin.

Spillover happens only when the companion's own workspace is empty. A monitor
with windows on it is showing something someone chose to put there, and burying
that under another workspace's cold windows is worse than a crowded outer orbit.
When the companion is occupied, `lagrange` behaves as `binary`.

A parked window is clipped to the monitor it is parked on rather than to its
workspace's host — `reportGeometry()` takes the clip from `view.solar.cell` when
one is set, the same way an overview thumbnail does.

### Radial ray-casting focus

Directional focus is not a geometry question the compositor can answer here —
the same reason the scrolling layout takes it into the shell — so `Mod4+h/j/k/l`
arrive as `shell solar.ray <direction>`.

Cast a ray from the sun's centre at φ (right 0, down 90, left 180, up 270) and
score every other window on the workspace:

```
Δ(w)     = wrap(atan2(wy − Cy, wx − Cx) − φ)      into (−π, π]
r(w)     = hypot(wx − Cx, wy − Cy)
score(w) = |Δ(w)| + β · r(w) / rMax                β = rayBias (0.35)
```

Reject anything with `|Δ| > π/2` — it is behind you — and focus the lowest
score. `β` is what stops a distant outer-orbit window that happens to be dead
on the axis from beating an inner satellite ten degrees off it: without the
radial term, ray-casting on a two-ring layout always picks the far ring.

The focused window becomes the sun, so a ray-cast focus is also the promotion
gesture. There is no separate "promote" command because there is nothing for it
to do.

## IPC

No new messages. Solar uses what is already on the wire:

| Message | Direction | What solar does with it |
| --- | --- | --- |
| `view.layout` | shell → compositor | the rect, plus `scale` for the outer orbit, `floating: true` for the sun, and `clip` for a window parked on another monitor |
| `view.opacity` | shell → compositor | the resting opacity of each tier, re-sent only when it changes |
| `view.focus` | shell → compositor | what ray-cast focus and slingshot ask for |
| `view.focused` | compositor → shell | sets the sun for that workspace |
| `shell.command` | compositor → shell | the commands below |
| `config` | compositor → shell | `"layout": "solar"` |

`shell.command` verbs, all handled in `commands.js`:

| Command | Argument | Effect |
| --- | --- | --- |
| `solar.ray` | `left`/`right`/`up`/`down` | ray-cast focus, above |
| `solar.spin` | `1` / `-1` | rotate the satellites one slot around the orbit, sun unchanged |
| `solar.slingshot` | — | throw the focused window to the other monitor; it arrives as that monitor's sun |
| `solar.mass` | `1` / `-1` | α ± 0.05, clamped to 0.30…0.85 |
| `solar.field` | — | toggle `binary` ↔ `lagrange` |
| `layout.model` | a model name, or nothing | switch layout model; nothing cycles |

`layout.model` switches between the three models — a name picks one, no
argument cycles — and is not bound to anything by default. `layout.mode` still
selects the arrangement *within* tiling and does nothing here: solar has no
sub-arrangements, because where a window goes is decided by its position in the
order and nothing else. `layout.toggle` is unrelated to either; it flips a
tiling container's direction.

## Keymap

Bound in `crates/viewport/src/binding.rs` only when the configured layout is
`solar`, so nothing here shadows a chord in the other two models.

| Chord | Action |
| --- | --- |
| `Mod4+h` `Mod4+j` `Mod4+k` `Mod4+l` | `shell solar.ray left/down/up/right` |
| `Mod4+Left` … `Mod4+Right` | the same |
| `Mod4+bracketright` | `shell solar.spin 1` |
| `Mod4+bracketleft` | `shell solar.spin -1` |
| `Mod4+Shift+s` | `shell solar.slingshot` |
| `Mod4+equal` | `shell solar.mass 1` |
| `Mod4+minus` | `shell solar.mass -1` |
| `Mod4+Shift+g` | `shell solar.field` |

Everything else — workspaces, close, fullscreen, float, the launcher — is
unchanged, because none of it is layout-specific.

## Tunables

All of `SOLAR` in `data/shell/solar.js`, and all of it is read at use rather
than captured, so changing a value and reloading the shell takes effect without
a restart.

| Name | Default | What it does |
| --- | --- | --- |
| `sunArea` | 0.60 | fraction of the output the sun occupies |
| `innerRatio` | 0.42 | satellite size as a fraction of the sun |
| `innerClearance` | 0.72 | how far out the inner ring sits, 1 = against the edge |
| `outerScale` | 0.40 | drawn scale of a cold window |
| `outerLapInset` | 0.86 | inward step per lap of the outer slot table |
| `lagrangeScale` | 0.55 | drawn scale of a window parked on the companion |
| `opacity` | 1 / 0.9 / 0.78 / 0.4 | sun, companion star, inner, outer |
| `rayBias` | 0.35 | radial weight in the ray-cast score |

## What it does not do

- **No manual resize.** A satellite's size is a function of the sun's, so
  `layout.resize` has nothing to act on. `solar.mass` is the resize gesture.
- **No orbital drag.** Dragging a window does not move it to another slot;
  slots are assigned by order, and order is changed with `solar.spin` and
  `window.move`.
- **Dialogs stay floating.** A window `views.rs:209` calls floating is left to
  the existing floating path and is not given a slot. Tiling a modal into an
  orbit is the same mistake in every layout.
- **Sessions restore the order, not the slots.** Slot assignment is derived, so
  `session.js` needs to know nothing about solar.
- **The overview draws a solar workspace as a tree.** A thumbnail is the
  workspace shrunk to a cell an eighth of the size, and an orbit at that scale
  is a smudge with a smudge in the middle of it. The overview's question is
  which windows are where, and a grid answers it; the orbits come back when it
  closes.

## Testing

`tests/shell.test.js` runs the real shell against a stubbed DOM, so it can check
structure and the arithmetic — `recalculateSolarLayout()` is a pure function of
`(ids, sun, area)` and needs no browser. What it cannot check is whether the
rectangles land where the stylesheet says; that is `tests/layout.test.js`, which
needs a screen.

```
node tests/shell.test.js data/shell solar
```
