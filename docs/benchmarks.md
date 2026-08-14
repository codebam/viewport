# Benchmarks

Viewport measured against [sway](https://swaywm.org/) and
[niri](https://github.com/YaLTeR/niri), on the same machine, the same
monitors, and the same client, by `scripts/bench-vkcube.sh`.

niri is in the comparison for a reason beyond wanting a third number:
Viewport's scrolling layout was written from it, so it is the compositor that
model should be held against.

Everything below came from `--drm` runs on a TTY. Nested runs are paced by the
host compositor and their frame rates compare backends rather than
compositing; only the CPU, memory and GPU columns survive nesting. If a number
here is to mean anything it has to have reached a panel.

## The machine

| | |
| --- | --- |
| CPU | AMD Ryzen 7 5700X3D, 8 cores / 16 threads |
| GPU | AMD Navi 21 (`1002:744c`), `amdgpu` |
| Kernel | 7.1.5-cachyos |
| Monitors | DP-1 and DP-3, both 2560x1440 at 120Hz |
| Client | `vkcube`, Vulkan, one window filling its output |

| compositor | version |
| --- | --- |
| Viewport | release build, `4c23f59`-era for the one-monitor table and `870cfe3`-era for the two-monitor one |
| sway | 1.13-dev |
| niri | 26.04 (nixpkgs) |

The two Viewport builds are recorded by their timestamp rather than their
commit — the harness writes the build time to `environment.txt`, not a hash. The
commits between them are the multi-GPU and unplug fixes, which touch nothing
either table measures.

Both compositors are configured so a lone window fills its output, and the
mode is left at whatever each output prefers, which on this desk is the same
120Hz on both panels.

## What the scenarios are

| scenario | what it runs |
| --- | --- |
| `fifo` | one client, `FIFO` present mode — every frame is presented, paced at the refresh rate |
| `fifo-x4` | four concurrent clients, all `FIFO` |
| `immediate` | one client, `IMMEDIATE` — no pacing, most frames discarded |
| `mailbox` | one client, `MAILBOX` — no pacing, latest frame wins |
| `mm-fifo` | two monitors: one client saturating DP-1, a `FIFO` client measured on DP-3 |
| `mm-stress` | two monitors: an `IMMEDIATE` client that never idles on DP-1, a `FIFO` client measured on DP-3 |

## One monitor

All three compositors in one sitting, 2026-08-01. `bench-results/` is not
committed, so the raw TSVs behind these tables live on the machine that ran
them; the section at the end says how to produce your own.

| scenario | compositor | fps | cpu ms/frame | comp cpu % | gpu % | rss MB |
| --- | --- | --- | --- | --- | --- | --- |
| fifo | viewport | 120.0 | 0.217 | 2.6 | 3.3 | 344 |
| fifo | sway | 120.4 | 0.217 | 2.6 | 3.9 | 222 |
| fifo | niri | 119.9 | 0.183 | 2.2 | 3.1 | 210 |
| fifo-x4 | viewport | 480.5 | 0.083 | 4.0 | 4.5 | 344 |
| fifo-x4 | sway | 476.3 | 0.092 | 4.5 | 5.1 | 222 |
| fifo-x4 | niri | 121.5 | 0.196 | 2.4 | 3.2 | 210 |
| immediate | viewport | 13922.8 | 0.022 | 30.2 | 50.5 | 344 |
| immediate | sway | 13559.4 | 0.013 | 18.1 | 41.7 | 222 |
| immediate | niri | 0.0 | — | — | 2.0 | 210 |
| mailbox | viewport | 13894.8 | 0.022 | 31.2 | 50.2 | 344 |
| mailbox | sway | 13223.4 | 0.014 | 18.7 | 41.4 | 222 |
| mailbox | niri | 7475.4 | 0.061 | 45.5 | 37.9 | 210 |

Viewport over the others. Above 1.00 is more of whatever the column counts,
which is better for fps and worse for everything else.

| scenario | fps vs sway | cpu ms/frame vs sway | fps vs niri | cpu ms/frame vs niri |
| --- | --- | --- | --- | --- |
| fifo | 1.00x | 1.00x | 1.00x | 1.18x |
| fifo-x4 | 1.01x | 0.91x | 3.95x | 0.43x |
| immediate | 1.03x | 1.63x | n/a | n/a |
| mailbox | 1.05x | 1.58x | 1.86x | 0.37x |

What is in there:

- **FIFO is a three-way tie at the panel rate.** None of the three misses
  vsync with one window on screen. This is the case that describes a desktop
  in use, and there is nothing between them in it.
- **`fifo-x4`: Viewport and sway reach ~480, niri stops at 121.** Four clients
  each asking for 120Hz should total ~480 presented frames a second. niri
  returns the rate of one.
- **niri does not survive `IMMEDIATE` here at all.** 0 fps with a negative
  wall time is a failed run, not a slow one — `vkcube --present_mode 0` does
  not produce frames under niri on this machine. Reproduced in a second niri
  run an hour later. Not counted against it in the ratios.
- **Unthrottled, sway is the cheapest by a wide margin.** 0.013–0.014 ms of
  CPU per frame against Viewport's 0.022, and 18% of a core against
  Viewport's 30%. Viewport is doing more work per discarded frame than it
  needs to.
- **Viewport's resident set is 344 MB against 210–222.** The largest standing
  loss in the table, and it is there whether or not anything is drawing.

`fps` is client frames. Only in the FIFO rows is every one of them a frame
that reached the screen — `IMMEDIATE` and `MAILBOX` discard most of what they
draw, so their fps says how fast the compositor returns buffers, not how fast
it presents. `cpu ms/frame` includes whatever the compositor burns while idle,
divided over the frames the client asked for; measured idle cost was 0.0–0.1%
for all three, so the net figure is the same to the digit shown.

## Two monitors

One client holding DP-1 at full rate, the other client measured on DP-3. This
is the question no single-output run can answer: what a terminal on your other
monitor gets while something runs flat out over here.

| scenario | viewport | sway | niri |
| --- | --- | --- | --- |
| mm-fifo, fps on DP-3 | 118.7 | 119.2 | 116.6 |
| mm-stress, fps on DP-3 | 118.5 | 119.0 | not run |

All three hold the screen that is not busy at its own refresh rate.

### A retraction

An earlier version of this table had Viewport at 79.0 and 87.6 against sway's
119, and read as Viewport starving the second monitor. That was the harness.

`verify_placement` — the check that both clients landed on the monitors they
were asked for — ran in the foreground between the measured client's start and
its wait, and the wall clock spans exactly that interval. So the time
verification spent polling was recorded as time the client took to draw. It
polls for up to five seconds; for Viewport each poll is a control-socket round
trip through Python with its own timeouts, while sway answers two quick
`swaymsg` calls. It inflated one compositor's wall time and not the other's.

The arithmetic was there to be read: 600 frames / 7.598s = 78.97, which is the
"79.0". And the giveaway with it — `wall_b` came back 7.599 and 7.598 for runs
with half the frame count between them. A number that does not move when the
work halves is not measuring the work.

Fixed in `7c999d3`: verification now runs in the background and is waited for
after the clocks have stopped. It still happens while both clients are alive,
which is the whole point of it, but it is no longer on the measurement path.

This retracts the second-monitor starvation entirely, including the claim that
the barrier-tick fix in `8eada16` was incomplete. Two independent compositors
appearing to agree that Viewport was slow were agreeing about this harness.

## Caveats

Read these before quoting anything above.

- **No single sitting has all three compositors post-fix.** The one-monitor
  table is one run of all three together. The two-monitor row for Viewport is
  a rerun after `7c999d3`; sway's is from before it but uncontaminated — its
  wall time was 5.034s for 600 frames, which is the honest figure; niri's is a
  separate rerun.
- **niri has no `mm-stress` number.** It was not run post-fix.
- **niri's `IMMEDIATE` failure is not investigated.** It may be a niri bug, a
  `vkcube` interaction, or something about this machine. It is reported as
  observed, not diagnosed.
- **`fifo-x4` at 480 fps is four clients' frames added up**, not one client at
  480Hz. Nothing here is running above the panel rate.
- **One machine, one GPU vendor, two identical panels.** A desk with monitors
  at different refresh rates asks the two-monitor question harder than this
  one does, because a compositor pacing off the device rather than off each
  screen cannot produce two different numbers. This desk cannot expose that.
- **CPU in the two-monitor rows is the whole compositor across both screens.**
  There is no per-output attribution to be had: one process, one render loop
  serving both.

## The shell backends against each other

Everything above compares Viewport with other compositors. The shell can be
drawn by six different engines — see
[`docs/shell-backends.md`](shell-backends.md) — and comparing those means
holding the compositor still and changing only the engine. The numbers below
are for the four that existed when the run was made; `servoshell` was measured
later, in a run of its own further down, and the embedded `servo` backend has
not been measured at all — it is not in the script's default list either,
because it has no package to build.

```sh
scripts/bench-backends.sh --runs 3    # every implemented backend
scripts/bench-backends.sh --only cef
```

Each backend is a package rather than a flag, because the packaged wrapper is
what names the engine and installs the shell program beside the compositor; the
script builds all four before measuring any, so a build failure in the last one
does not arrive after three runs have already had the machine.

All four come from this flake. Three of them are packaged for Arch as well —
`packaging/aur/viewport-{wpe,webkitgtk,chromium}` — and `cef` is not: CEF ships as a
prebuilt binary bundle, the only Arch package of it is `cef-minimal` in the AUR,
and that is CEF 121 against the 149 this tree is built against. So a comparison
run on Arch is three engines wide and one on nix is four.

### What one run said

`--drm` from a TTY, DP-1 at 2560x1440@120 with a 5120x1440 layout, `--runs 3`,
median of three. Same compositor binary in all four, same shell page, same
client.

| backend | fifo fps | cpu ms/frame | comp cpu % | session cpu % | compositor rss MB | start → shell talking |
| --- | --- | --- | --- | --- | --- | --- |
| wpe | 120.0 | 0.217 | 2.6 | 2.6 | 329 | 2484 ms |
| webkitgtk | 120.0 | 0.217 | 2.6 | 2.8 | 248 | 585 ms |
| chromium | 119.9 | 0.217 | 2.6 | 3.0 | 248 | 678 ms |
| cef | 119.6 | 0.233 | 2.8 | 2.8 | 248 | 308 ms |

**The compositing columns do not tell these engines apart, and that is the
finding.** Every backend presents at the panel's rate and spends the same fifth
of a millisecond of CPU on each frame, because the thing being measured is a
fullscreen `vkcube` and the shell behind it is idle. An engine that has painted
its desktop and been given no reason to repaint costs nothing to composite,
whichever engine it is. This benchmark was built to compare compositors and it
compares compositors; pointing it at four shells mostly re-measures the same
compositor four times.

**The memory column is not a memory comparison.** `rss_mb` is the compositor
process, and only `wpe` has an engine inside that process — the other three
keep theirs in a shell process the harness never samples. So the honest reading
of 329 against 248 is "this is what moving WebKit out of the compositor takes
out of the compositor", not "wpe uses more memory". Session CPU *does* cover
the shell process, and it is the column to read across that line.

**The startup column is a real difference** and the largest one here. CEF is up
and talking in 308 ms against WPE's 2.5 s — though wpe's figure is measured
from a different point, since there the compositor is starting an engine on a
thread of its own rather than a process, and the page load is inside it.

**What is not established.** Session CPU varies more between runs of one
backend than between backends: `cef` fifo came in at 2.2, 2.8 and 7.6 percent,
`chromium` at 2.4, 3.0 and 5.6. The two Blink backends produced the spikes and
the two WebKit ones did not, across all three runs — which is worth noticing
and is not worth concluding anything from at n=3.

**What would actually compare engines** is a scenario where the shell repaints,
a sampler that counts every process the desktop is made of, and the shell's own
paint rate. That is `scripts/bench-shell.sh`, below — and the rate is what turns
out to decide it. These tables say the compositor does not care
which engine draws its desktop, which is a real thing to know and a smaller
thing than it looks.

## The shell under load

`scripts/bench-shell.sh --seconds 25 --runs 3`, from a TTY, four `foot` windows
open, the shell driven over the control socket at four commands a second: the
overview on and off, a workspace switch, and a pair of resize deltas — which is
what dragging an edge does. Median of three runs.

CPU is the compositor **and every process it started**, because the engine is
inside the compositor for `wpe` and beside it for the other three; a
per-process number cannot compare across that line. Memory is **PSS** over the
same tree rather than RSS: CEF and Chromium run four or five processes sharing
most of an engine, and adding their RSS together reports a desktop about twice
the size of what the machine has actually committed. `shell fps` is the frames
the shell handed over, counted by the compositor — `VIEWPORT_SHELL_RATE=1`.

| backend | idle cpu % | load cpu % | of which the compositor | shell fps | cpu per painted frame | idle pss MB | load pss MB | processes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| wpe | 0.3 | 8.9 | 4.4 | 49 | 0.182 | 358 | 362 | 9 |
| webkitgtk | 0.3 | 10.9 | 2.9 | 40 | 0.273 | 391 | 395 | 10 |
| chromium | 0.7 | 13.3 | 3.1 | 43 | 0.309 | 582 | 595 | 12 |
| cef | 1.0 | 9.5 | 3.0 | 38 | 0.250 | 529 | 551 | 9 |

**These are the second set of numbers for this table, and the first set were
wrong.** The Blink backends measured 12 frames a second against WebKit's 52,
which read as an engine painting a fifth as often — and it was this
compositor's fault. Chromium paces on presentation feedback; the shell surface
was in neither the space nor a layer map, so nothing ever took its feedback,
and 723 requests in twelve seconds came back `discarded`. With nothing to pace
against it fell back to its own 60Hz clock. WebKitGTK paces on frame callbacks,
which the shell surface always received, so it tracked the panel and hid the
bug. Fixed; re-measured; the row that said 12 now says 38.

**Nothing separates the engines by much.** All four paint between 38 and 49
frames a second under the same load and spend between 8.9 and 13.3 percent of a
core doing it. The interesting column is the last derived one: cost per frame
the shell actually produced.

**`wpe` is the cheapest, and on nix it is the one nobody waits for.** 0.182% of
a core per frame and the smallest resident set, which is what an engine inside
the compositor buys — no second process, no buffer handed across one. Under
this flake it is also the only backend that compiles WebKit, so a machine
switching to it waits hours for a desktop. On Arch it is the opposite: the
repositories carry `wpewebkit` built with the WPE platform API, nothing
compiles an engine, and `packaging/aur/viewport-wpe` builds in about a minute.

**Of the three that build no engine, `cef` is cheapest per frame and
`webkitgtk` is lightest.** 0.250% against 0.273%, and 551 MB against 395 — a
156 MB difference, which is the whole of the argument for either. Embedding
beats driving: `chromium` runs the same engine through a browser process and a
DevTools round trip per message and costs 0.309% per frame for it, the most of
the four.

**Idle went up for the Blink backends** — 0.7% and 1.0% against 0.3% before the
fix, when they were being told nothing. A client that is now receiving
presentation feedback keeps a compositor loop alive to consume it. That is a
real cost of the fix and it is small, but it is not nothing on a laptop.

**The spread**: CPU is tight (8.9/8.9/8.9, 11.4/10.9/10.9, 13.3/13.3/13.3,
10.9/9.5/9.3) and the rate is not (49/46/53, 40/43/39, 44/43/41, 35/38/43). The
load is scripted but the work it makes is not identical run to run — an
overview with four windows is not the same repaint every time.

### servoshell, measured against the other three

A later run, on the same machine and the same load, with `servoshell` in it —
`scripts/bench-shell.sh --runs 3 --seconds 20`, four `foot` windows, from a
TTY. `wpe` was not run. These four were measured in one sitting, so read this
table across itself rather than against the one above.

| backend | idle cpu % | load cpu % | of which the compositor | shell fps | cpu per painted frame | idle pss MB | load pss MB | processes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| webkitgtk | 0.0 | 11.5 | 2.9 | 48 | 0.240 | 434 | 449 | 10 |
| chromium | 1.2 | 11.5 | 3.2 | 44 | 0.261 | 629 | 639 | 12 |
| cef | 0.9 | 9.9 | 2.9 | 43 | 0.230 | 576 | 594 | 9 |
| servoshell | 0.0 | 8.5 | 2.0 | 14 | 0.607 | 346 | 357 | 4 |

**`servoshell` is the cheapest desktop here and the most expensive engine.**
Every absolute column is the lowest of the four — 8.5% of a core under load,
357 MB, four processes against nine to twelve, and an idle cost that did not
register at all. The derived column says why: it painted 14 frames a second
against 43 to 48, so it costs 0.607% of a core per frame the shell actually
produced, about two and a half times what the other three cost. A desktop that
repaints a third as often is cheaper the way a slower car uses less fuel.

**This is the second set of numbers for this table, and the first set were
measured against an overview Servo could not draw.** The shell used CSS grid,
which Servo does not have, so its thumbnails stacked in one column running off
the screen — the load was real work on the other three and a broken layout on
the fourth. Fixing it in the obvious direction, by making the overview flexbox
for everyone, then cost the three engines that *do* have grid about twice the
compositor CPU each. So the grid stayed and the flex version became a
fallback behind `@supports not (display: grid)`; these numbers are that
arrangement, with every backend drawing the same overview it was designed to
draw. The per-frame ordering did not move.

**The process count is the architecture, not a fault.** Servo runs
single-process by default, so the whole desktop is the compositor, the shell
process and the browser. Blink's four-to-five-process model is most of the
memory difference against `cef` and `chromium`, and it is what PSS exists to
report honestly.

**The bridge works, which is the other thing this run establishes.** The
harness refuses to measure a desktop whose shell never spoke — it waits for
`shell is talking to us` and exits if it does not come — and `servoshell`
produced three runs. So the loopback HTTP bridge and the injected user script
carried the shell's messages to the compositor on real hardware, from a
`file://` page, for a minute of scripted load.

**What is not established.** One sitting, three runs, and a rate that varies
(19/14/14 against webkitgtk's 80/43/48): whether Servo's paint rate here is the
engine, the pacing, or this compositor's handling of it is exactly the question
the Blink pacing bug above should make nobody guess at. The engine is also
nixpkgs' Servo 0.3.0 — a version this flake pins rather than one this project
chose.

### Smooth is not the same as fast

The table above says `servoshell` paints a third as often as the others, and
the desktop it draws is reported as feeling *smoother* than `cef`'s. Both are
true, and the second one is the more useful fact about what a person sees.

`shell fps` is a median over a load that alternates between an overview
repainting hard and a desktop repainting twice a second, so it measures the
mixture. What smoothness depends on is the cadence *while something is moving*.
Taking only the seconds where the shell painted at least five frames — two
runs, from a TTY, same machine and load as above:

| backend | animating fps | steadiness (spread ÷ mean) | range across the run |
| --- | --- | --- | --- |
| webkitgtk | 80 | 0.48 | 7–164 |
| chromium | 64 | 0.43 | 37–115 |
| cef | 54 | 0.47 | 10–89 |
| servoshell | 21 | **0.31** | 6–26 |

**`cef` swings by a factor of eight while it animates and `servoshell` holds a
flat 26.** More frames delivered unevenly reads as stutter; fewer frames at a
steady cadence reads as smooth. That is the whole of the discrepancy between
the throughput column and what the desktop looks like, and it reproduced across
two separate sittings — an earlier three-run set gives 0.49 for `cef` against
0.29 for `servoshell` from the same arithmetic.

`scripts/bench-shell.py` records `shell_fps_animating`, `shell_fps_spread`,
`shell_fps_cov` and `shell_fps_floor` for this reason, and the summary prints
the middle two.

**What this still cannot see.** The compositor emits a rate once a second, so
these are per-second buckets: a steady 26 frames a second and a 26 that stalls
for 200 ms once a second are the same number here. Distinguishing them needs
per-frame timestamps rather than a per-second count, which is a change to the
compositor's `VIEWPORT_SHELL_RATE` path and is not written. Until it is, this
column is evidence about pacing and not a measurement of hitching.

### Reading the caveats

`wpe` runs the engine inside the compositor process, so its cost lands in the
compositor's own columns while the other three carry theirs in a second
process. And `chromium` and `cef` both run `--in-process-gpu`, because with a
GPU process of their own Chromium segfaults on this compositor and falls back
to software; that is a real difference in what is being measured, not a knob
turned for tidiness.

## Reproducing

```sh
# nested, viewport and sway
scripts/bench-vkcube.sh

# nested, all three
scripts/bench-vkcube.sh --only all

# from a TTY, real scanout — this is what the tables above are
scripts/bench-vkcube.sh --drm --only all --runs 3

# two monitors: --output is held busy, --second is measured
scripts/bench-vkcube.sh --drm --only all --output DP-1 --second DP-3
```

`vkcube` and niri are fetched from nixpkgs when they are not already on PATH.
Each run writes a timestamped directory under `bench-results/` holding
`raw.tsv`, `raw-mm.tsv`, `environment.txt`, a per-run `summary.md`, and each
compositor's log. `--scale 0.25` runs a quarter of the frames for a smoke
test; `--runs N` repeats and takes the median.
