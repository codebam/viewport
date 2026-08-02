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
drawn by four different engines — see
[`docs/shell-backends.md`](shell-backends.md) — and comparing those means
holding the compositor still and changing only the engine:

```sh
scripts/bench-backends.sh --runs 3    # every implemented backend
scripts/bench-backends.sh --only cef
```

Each backend is a package rather than a flag, because the packaged wrapper is
what names the engine and installs the shell program beside the compositor; the
script builds all four before measuring any, so a build failure in the last one
does not arrive after three runs have already had the machine.

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
| wpe | 0.3 | 9.1 | 4.5 | 59 | 0.154 | 360 | 363 | 9 |
| webkitgtk | 0.3 | 11.4 | 3.0 | 52 | 0.219 | 390 | 395 | 10 |
| chromium | 0.3 | 6.7 | 1.9 | 12 | 0.558 | 582 | 589 | 12 |
| cef | 0.0 | 5.3 | 1.9 | 12 | 0.442 | 530 | 547 | 9 |

`cpu per painted frame` is the load column divided by the rate: percent of a
core per frame the shell produced.

**The paint rate is the column that matters, and it reverses the CPU one.**
Read without it, the Blink backends look 40% cheaper. They are not cheaper;
they painted a fifth as many frames. Per frame the shell actually produced,
`cef` costs nearly three times what `wpe` does and twice what `webkitgtk` does.
A desktop repainting at 12fps while a window is being dragged is one anybody
can see stuttering, and no CPU figure was ever going to say so.

This is the second time this page has had to be corrected by a number arriving
later, and both times in the same direction: a cost that looked low because
less work was being done. The vkcube tables could not tell the engines apart
because the shell was idle; this table's CPU column could not either, until the
rate said what the CPU was buying.

**Why Blink paints a fifth as often is not established.** Chromium throttles
compositing in several circumstances and any of them could be it, or Blink may
simply be slower to produce a frame for this page. The measurement says what
happened, not why, and the difference is far too large to leave at that — but
it is not something this run can answer.

**Memory goes the other way, and it is a real trade.** WebKit is 150–190 MB
lighter across the tree. On a machine short of memory rather than CPU that is
the argument for `webkitgtk` over `wpe`, which is otherwise the best of the
four on every column here.

**CEF beats the browser it embeds.** 5.3% against 6.7%, 547 MB against 589,
nine processes against twelve, at the same 12fps. Driving Chromium as a browser
costs a browser process and a round trip per message; embedding it does not.

**Nothing is idling badly.** All four sit at 0.0–0.3% with the desktop up and
nobody touching it, which is what the frame-callback pacing is supposed to
produce and is the first time it has been measured across the backends.

**The spread, since the last set of tables was noisy enough to mislead**: CPU
is tight (9.1/9.1/10.8, 11.4/11.3/11.8, 6.7/6.8/6.7, 5.3/5.3/5.1) and the
paint rate is not (41/59/63, 39/52/68, 12/12/12, 12/12/11). The WebKit rates
vary by half; the Blink ones do not vary at all, which is itself a hint that
something is clamping them.

### Reading the caveats### Reading the caveats

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
