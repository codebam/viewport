# Why the shell is a web page

## On this page

- [What the page does and does not do](#what-the-page-does-and-does-not-do)
- [Why CSS instead of a layout protocol](#why-css-instead-of-a-layout-protocol)
- [It does not mean compiling a browser](#it-does-not-mean-compiling-a-browser)
- [The obvious questions](#the-obvious-questions)
- [Paste-ready answers](#paste-ready-answers)
- [Try it](#try-it)

Viewport is a Wayland compositor. DRM/KMS, input, the Wayland protocols and the
scene graph are Smithay's, exactly as in any other compositor. What is
different is where the *desktop* comes from: the wallpaper, dock, window frames,
titlebars, bar, overview and lock screen are a web page.

That page is not a window and it does not sit in front of your applications. It
is one full-screen buffer *below* every client surface. The browser paints the
furniture into it; the compositor paints real Wayland clients on top. Neither
hands a pixel to the CPU.

This document is for the person who hears "the shell is HTML" and thinks that
sounds cursed. It explains the boundary, the trade and the failure modes. For
something to paste into a chat, jump to [Paste-ready answers](#paste-ready-answers).

> In one sentence: the browser is Viewport's layout and paint engine for the
> desktop chrome, not its window system. Smithay still owns the display, the
> input and the clients; the page draws the furniture and reports where it left
> the holes.

## What the page does and does not do

| The page decides | The compositor owns |
| --- | --- |
| Where the bar, dock and wallpaper go | DRM/KMS, outputs, modes and vblank |
| The tiling tree: splits, weights, gaps, fullscreen | The stack, focus and hit-testing of real clients |
| The frame and titlebar drawn around a window | Moving client scene nodes to the rectangles it is told |
| Overview, notifications, lock and not-responding dialogs | Wayland protocols, input devices and explicit sync |
| The colours, fonts and animation of all of the above | Painting client surfaces above the shell buffer |

An application's own contents are never DOM. A `<div class="viewport">` in the
page is a hole; the compositor paints the client's real Wayland surface into the
screen rectangle that element occupies. A client cannot be styled from the
shell, and a broken page cannot repaint a terminal.

## Why CSS instead of a layout protocol

The hard part of a tiling compositor is turning "split this container, move that
window, fullscreen this one" into rectangles. Viewport builds the layout as a
tree of nested CSS flex containers. The browser computes every rectangle; the
shell measures where each hole landed and sends those rects over IPC. The
compositor moves the client's scene node there.

So the IPC carries *layout results and state*, not pixels and not a stream of
draw commands. "CSS over IPC" describes the mechanism accurately and the
boundary badly. The browser is the layout solver and Rust is the compositor:
there is no second layout implementation in Rust to keep in sync, and dragging
a gap or resizing a split needs no compositor-side geometry code at all.

One consequence is worth stating rather than hiding: the shell buffer is below
every client, so a floating window's border would be covered by the window it
decorates. The compositor draws those few pieces a second time above the
windows. That is an explicit part of the protocol, not a compositing accident —
see [Drawing in front of the windows](ipc.md#drawing-in-front-of-the-windows).

## It does not mean compiling a browser

The engine is a choice, and the default chooses one that nobody builds:

| Backend | What runs |
| --- | --- |
| `chromium` (default) | nixpkgs' Chromium as a child process, driven over DevTools; nothing in the closure links an engine |
| `webkitgtk` | WebKitGTK in a process of its own, as an ordinary Wayland client of Viewport |
| `servoshell` | nixpkgs' `servoshell`, driven the same way as Chromium |
| `cef` | Blink embedded through CEF, from a prebuilt binary bundle |
| `wpe` | WPE WebKit inside the compositor, built from source — hours and tens of gigabytes |
| `servo` | Servo embedded in the shell process, built by hand |

`nix run github:codebam/viewport` gives you the default: a compositor and a
desktop from a flake, with no WebKit compiled anywhere. Because the engine is
nixpkgs' Chromium, it tracks every security update nixpkgs ships. The page
cannot tell which engine drew it. See [Shell backends](shell-backends.md).

## The obvious questions

**Isn't there an IPC round trip per frame?**
No. The engine hands the compositor one DMA-BUF per frame, and the compositor
presents real client surfaces next to it. WebKit will not paint frame N+1 until
frame N is acknowledged, and that acknowledgement comes from the output's frame
handler, so pacing is vblank rather than a timer. In the published one-monitor
measurements the shell at 120 Hz costs Viewport 0.217 ms of compositor CPU per
frame, against 0.217 for sway and 0.183 for niri on the same machine; the
default backend's own paint cost is about 0.261% of a core per frame. See
[Benchmarks](benchmarks.md).

**What if the page hangs?**
The compositor is not the page. With the default, out-of-process backends the
desktop can die without taking the session with it, and the same binary is a
control client: `viewport msg -t quit` from a second TTY stops the session even
when the first one is stuck. `-- --exit-after 30` puts a deadline on a trial
run. The one honest exception is `wpe`, where the engine runs in-process and
cannot crash independently. `--debug` mirrors the page console into the
compositor log. See [Debugging](debugging.md).

**Can any web page just drive the compositor?**
No. The privileged bridge is installed only in the page's top frame, and the
shell must come from a `file:` or loopback URL unless the session was started
with `VIEWPORT_ALLOW_REMOTE_SHELL=1`. A remote origin that needs that override
is being trusted with the same authority as the compositor — which is exactly
why it is an override. See [IPC](ipc.md).

**Can I write my own shell?**
Yes. The protocol is documented in [IPC](ipc.md), the wire types live in the
MIT-licensed `viewport-ipc` crate, and [`examples/kiosk`](../examples/kiosk/README.md)
is a working shell in about two hundred lines. `--url` loads your page on a
nested session; `--url-span` puts it on every monitor.

**Why not a native bar and layer-shell instead?**
That is a different design with different costs, not a missing feature here.
Viewport's bet is that desktop chrome is better as software you can inspect and
change in a browser, at the price of depending on an engine. What is deliberately
kept in the compositor rather than a daemon beside it is recorded in the
[Roadmap](roadmap.md).

## Paste-ready answers

**To "css… over ipc? that sounds cursed"**

> It sounds worse than it is: the browser isn't in charge of the desktop, it's
> the layout and paint engine for the chrome. The compositor is still Smithay —
> it owns DRM, input, focus and the real Wayland clients. The page draws the
> wallpaper, bar and window frames into one buffer *under* those clients, and
> uses CSS flexboxes as a layout solver. It sends the resulting rectangles over
> IPC; the compositor paints the apps into them. No app pixels go through the
> DOM, and there's no second layout implementation in Rust to keep in sync.

**To "so you shipped a browser in my compositor"**

> Not by default. `nix run github:codebam/viewport` drives nixpkgs' Chromium as
> a separate child process and links no engine at all, so it builds in seconds
> and picks up nixpkgs' security updates. WPE-in-process and embedded CEF/Servo
> are options for people who want an engine API; the same shell page runs under
> all six backends. `nix run` gets you a compositor and a desktop without
> compiling WebKit.

**To "what happens when the shell breaks"**

> The compositor is a separate program from the page on the default backend,
> and the `viewport` binary doubles as a control client, so `viewport msg -t
> quit` from another TTY gets you out even with the desktop wedged. There's a
> `--debug` flag that mirrors the page console to the compositor log, and
> `-- --exit-after 30` for a trial run. The in-process WPE backend is the one
> case where the shell and compositor share a fate.

## Try it

```sh
nix run github:codebam/viewport
```

It nests inside the session you are already in when there is one, and takes the
DRM device when there is not — so trying it from a terminal costs a window
rather than the machine. `Mod4+Shift+e` quits. `viewport msg --help` lists every
control message. For the internals behind all of this, start with the
[README](../README.md) and [IPC](ipc.md).
