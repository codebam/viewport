# Suggestions

A review of the tree at `68b52df`, covering the compositor (`src/`, ~7.9k lines
of C), the reference shell (`data/shell/`, ~3.9k lines of JS/CSS), the build,
the tests, and the packaging.

Items are ordered by value, not by size. Each names the file and line it
applies to, what the current behaviour is, and what to do instead.

## Where this codebase already is

Worth stating so the rest reads in proportion. The commit history is almost
entirely `fix:` commits with the reasoning written into the code next to the
fix; there are zero `TODO`, `FIXME`, `XXX` or `HACK` markers in the tree; every
teardown path is guarded and commented with the assertion it prevents; the IPC
socket does `SO_PEERCRED`, `accept4(SOCK_CLOEXEC)`, a bind-time `umask`, and
bounded read and write queues; `js_string_literal()` routes every page-bound
string through a JSON encoder rather than concatenation. There is no low-hanging
fruit here. What follows is a mix of one real user-facing bug, a class of input
handling that is fixed in one place but not the others, some measurable waste,
and structural work that would pay off as the file sizes keep growing.

---

## 1. Correctness

### 1.1 The NixOS module points every user at a dev server

`flake.nix:284-288`, used at `flake.nix:264`.

```nix
url = mkOption {
  type = types.str;
  default = "http://localhost:3000";
};
```

`configFile` unconditionally emits `inherit (cfg) url`, so a user who writes
nothing but `programs.viewport.enable = true` gets a config file containing
`"url": "http://localhost:3000"`. Nothing is listening there. The load fails,
the first-paint timeout fires, and the session comes up on `fallback.html`.

The compositor's own default (`src/main.c:51`) is the bundled shell, and
`src/main.c:229` deliberately arranges for the config file *not* to shadow an
explicit flag — but there is no flag here, and the file always sets the key. The
module overrides a good default with a bad one.

Fix:

```nix
url = mkOption {
  type = types.nullOr types.str;
  default = null;
  description = ''
    Web endpoint the shell UI is loaded from. Null uses the bundled shell.
  '';
};
```

and in `configFile`, replace `inherit (cfg) url;` with
`lib.optionalAttrs (cfg.url != null) { inherit (cfg) url; }`.

This is the highest-value change in this document: it is the difference between
`enable = true` producing a working desktop and producing the fallback page.

### 1.2 `viewport_config_reload()` leaks a cursor manager and half-ignores the config

`src/config.c:354-363`.

```c
if (server->xcursor_mgr != NULL && server->config.cursor_size > 0) {
    struct wlr_xcursor_manager *manager = wlr_xcursor_manager_create(
        server->config.cursor_theme, server->config.cursor_size);
    if (manager != NULL) {
        server->xcursor_mgr = manager;
    }
}
```

Two problems in five lines.

The old manager is never destroyed. The comment says it "stays alive because the
cursor may still be showing an image from it", which is a real hazard, but
nothing ever frees it — not on the next reload, not in
`viewport_server_finish()`. Every `Mod4+Shift+c` leaks a manager and its loaded
theme. Under ASan that shows up as a growing leak report on a session where
someone is iterating on their config, which is exactly the session where reloads
happen most.

The guard is also `cursor_size > 0`, so changing only `cursor.theme` in the
config file and reloading does nothing at all. The user sees the same cursor and
concludes reload is broken.

Fix: drop `cursor_size > 0` from the condition (fall back to the same `24`
default `viewport_cursor_init()` uses), and keep the outgoing manager in a
one-slot "previous" field on the server that `viewport_server_finish()` destroys
— the hazard is a live image outliving the reload, not outliving the process.

### 1.3 `scale_states` is a process-global keyed by a pointer that can be reused

`src/xdg_shell.c:641`, cleared only from `src/ipc.c:1235`.

`scale_states` is a `static GHashTable *` keyed by `struct wlr_scene_buffer *`,
with no destroy notification on the buffer. It is dropped wholesale by
`viewport_scale_forget()` when the overview closes, which covers the common
case, but:

- A window closed *while the overview is up* leaves its entry behind. The
  allocator is free to hand the same address to the next `wlr_scene_buffer`, and
  that buffer inherits a stale `natural_width`/`applied_width` pair — the exact
  "multiplying that again shrinks the window a little further every frame"
  failure the mechanism exists to prevent.
- Nothing calls `viewport_scale_forget()` from `viewport_server_finish()`, so
  exiting with the overview open leaks the table.

Fix: move the two fields onto the buffer's owning `viewport_toplevel` (or hang
them off `wlr_scene_buffer->node.data` with a destroy listener) and delete the
hash table. If the table stays, at minimum call `viewport_scale_forget()` from
`viewport_server_finish()` and clear a window's entries from
`viewport_view_unmap()`.

The same file has `static bool debug_scale;` (`src/xdg_shell.c:648`). Mutable
file-scope state in a compositor is a small thing until something needs two
servers in one process — which is what a unit-test harness for the C side would
be (see §5.2).

### 1.4 `--bind` silently drops the 65th binding

`src/main.c:93,140`. `bind_specs[64]` with `if (bind_count < ...)` and no `else`.
Someone driving the compositor from a generated command line gets a keymap
quietly missing its tail. Either log at `WLR_ERROR` on overflow or make it a
`GPtrArray`; the array is freed before the loop ends anyway, so there is no
lifetime argument for the fixed size.

---

## 2. IPC input handling

`viewport_ipc_handle()` is reachable from two places: the shell's
`postMessage()` — i.e. any page loaded at `config.url`, over the network — and
the UNIX socket. Two crash fixes already landed here (`src/ipc.c:1142` for a
non-string `type`, `src/ipc.c:618` for a non-string `transform`), and both
comments correctly identify the shape of the problem: *`json_object_has_member()`
proves presence, not type.*

The fix was applied at the two sites that crashed. The rest of the file still
uses the unchecked pattern.

### 2.1 Close the class, not the instances

`src/ipc.c:749` (`object_int`) and every `json_object_get_*_member()` call in
the inbound half.

- `object_int()` calls `json_object_get_int_member()` after a bare
  `has_member()`. For `{"type":"view.layout","id":{}}` json-glib logs a
  `Json-CRITICAL` and returns `0`.
- `json_object_get_object_member(object, "clip")` (`src/ipc.c:793`) returns
  `NULL` for `"clip": 5`, and that `NULL` is then passed to `object_int()`,
  which passes it to `json_object_has_member(NULL, ...)` — another
  `g_return_val_if_fail` and another critical.
- `json_object_get_object_member(object, "mode")` (`src/ipc.c:1021`) and the
  `notification.*` / `output.*` string reads have the same shape.

None of these are crashes today, because glib's `g_return_val_if_fail` returns a
default rather than dereferencing. They are, however, a stream of `CRITICAL`
lines into the compositor log from one malformed message, and the next accessor
someone adds is one `strcmp()` away from being the third crash of this kind.

Fix: add four checked accessors near the top of the inbound section and use them
everywhere.

```c
static bool object_int_checked(JsonObject *o, const char *name, int *out);
static const char *object_string(JsonObject *o, const char *name);
static bool object_bool(JsonObject *o, const char *name, bool fallback);
static JsonObject *object_object(JsonObject *o, const char *name);
```

Each does `o != NULL && json_object_has_member() &&
JSON_NODE_HOLDS_VALUE(json_object_get_member()) && json_node_get_value_type() ==
G_TYPE_*`. This is maybe 40 lines and it retires the whole category, including
for whoever adds the next message type. The comment at `src/ipc.c:608-620`
already argues for exactly this — "Rejecting it here rather than at the call site
closes it for whoever adds the next caller" — it just needs to be applied to the
other twenty call sites.

### 2.2 Replace the dispatch chain with a table

`src/ipc.c:1152-1314`. Twenty-four `else if (strcmp(type, "...") == 0)` branches,
several with the handler body inlined, in a 215-line function.

```c
static const struct {
    const char *type;
    void (*handle)(struct viewport_server *, JsonObject *);
} ipc_handlers[] = {
    { "view.layout",     handle_view_layout },
    { "view.visible",    handle_view_visible },
    ...
};
```

The inlined bodies (`view.fullscreen`, `shell.overview`, `output.hdr`,
`bind.add`) each become a small static function, which is where they belonged
anyway — `shell.overview` alone is 30 lines of commentary and a loop over every
toplevel. The dispatcher drops to a loop plus the unknown-type error, and adding
a message becomes one table row instead of one more `else if`.

### 2.3 An error broadcast echoes to everyone, including the page

`src/ipc.c:1310-1313`, and `notify_error()` generally.

An unknown message type from *any* socket client produces a broadcast to *every*
listener — the page included. A script poking at the socket therefore writes
`console.error` lines into the shell. The string is JSON-encoded so there is no
injection, but the routing is wrong: an error caused by one client's message
should go back to that client.

This needs the reply path to know which client asked, which `viewport_ipc_handle()`
currently does not — it takes a `server`, not a client. Threading an optional
`struct ipc_client *origin` through it (NULL for the page transport) would let
`notify_error()` answer the originator and keep broadcasts for genuine state
changes. Worth doing before anything else grows a request/response shape.

---

## 3. Performance

### 3.1 Every compositor-to-page message compiles and runs a JavaScript program

`src/web.c:40-56`.

```c
char *script = g_strdup_printf(
    "window.dispatchEvent(new CustomEvent('viewport',"
    "{detail:JSON.parse(%s)}));", literal);
webkit_web_view_evaluate_javascript(web->web_view, script, ...);
```

Per message this is: a `JsonGenerator` allocation to escape the payload, a
`g_strdup_printf`, an IPC round trip to the WebProcess, a script compile, a
`JSON.parse`, and an event dispatch. That is acceptable at one message a second.
It is not what actually happens.

`viewport_ipc_notify_shell_command()` is called from `process_cursor_motion()`
(`src/input.c:180-204`) on every pointer motion event during a `Mod4+drag` move
or resize. A 1000 Hz mouse produces roughly 1000 of these a second, each one
building a `JsonBuilder`, generating a document, escaping it a second time as a
JS string literal, and compiling a fresh script in the web process — all to
deliver `layout.resize.delta 7 3 0`. The shell then coalesces the deltas anyway,
because `resizeByDelta` feeds a layout that only repaints on the next frame.

Two independent fixes, either of which helps:

1. **Coalesce at the source.** Accumulate `dx`/`dy` in the server and flush one
   `layout.*.delta` per output frame from `handle_output_frame()`. The shell
   cannot use them faster than that. This turns ~1000 messages/second into ~144.
2. **Stop recompiling.** Install a permanent handler once via
   `webkit_user_content_manager_add_script()` — a `WebKitUserScript` at document
   start that defines `window.__viewport_deliver = (s) => window.dispatchEvent(...)`
   — then send with `webkit_web_view_call_async_javascript_function()`, passing
   the JSON as a bound argument rather than as source text. That removes the
   double escape and the per-message compile.

Do (1) first; it is smaller and it is where the multiplier is.

### 3.2 The status tick rebuilds both bars every two seconds

`src/status.c:236` publishes on a 2-second timer.
`data/shell/shell.js:3107-3110` handles it with:

```js
case 'status.update':
  lastStatus = message;
  renderBars();
  break;
```

`renderBars()` calls `renderBar()` per output, which does
`workspacesEl.replaceChildren()` and `taskbarEl.replaceChildren()` — allocating
every workspace button and every taskbar button and rebinding every click
listener — before touching the seven module strings that actually changed.

Commit `95f625c` ("perf: stop rebuilding both bars once a second") fixed exactly
this for the clock tick, and the comment above `renderClocks()` spells out the
cost: "every shell repaint is a composited frame", so an idle machine repaints
the desktop purely to redraw text. The status path was left on the old
behaviour, so the win is halved — an idle machine still repaints every two
seconds instead of every one.

Fix: split `renderBar()` into `renderBarChrome(name)` (workspaces + taskbar,
called when the window list or workspace set changes) and
`renderBarModules(name)` (the seven strings, guarded by `!==` the way
`renderClocks()` already guards the clock). `status.update` calls only the
latter. Also skip outputs whose bar is hidden, as `renderClocks()` does.

### 3.3 `fadeIn()` sends an IPC message per animation frame

`data/shell/shell.js:1987-2001`. A 120 ms tween at 60 Hz is ~8 messages, each a
`postMessage` → `viewport_ipc_handle` → JSON parse → scene walk. Per window
opened. It is not enormous, but `handle_view_opacity()` walks the whole surface
tree with `wlr_scene_node_for_each_buffer()` each time, and a CSS `opacity`
transition on the frame plus a two-message (0 → 1 with a duration) protocol
would let the compositor do the tween. Lower priority than the two above.

---

## 4. Structure

These are not defects. They are the shape the code will need if it keeps
growing at the rate the history shows.

### 4.1 `include/viewport.h` is a 1030-line god-header

Every `.c` file includes every declaration in the project, plus 25 wlroots
headers. Touching one comment in it recompiles all 29 translation units. Three
of the four largest files (`ipc.c`, `input.c`, `xdg_shell.c`) are near or past
1000 lines each and every one of them sees the tablet API, the notification API
and the HDR API whether or not it uses them.

Suggestion: keep `viewport.h` for `struct viewport_server`, `struct
viewport_toplevel`, `struct viewport_config` and the enums — the types genuinely
shared — and split the function declarations into `viewport-input.h`,
`viewport-ipc.h`, `viewport-web.h`, `viewport-output.h`, `viewport-view.h`. The
file is already sectioned by `/* ---- ipc.c ---- */` banners, so the split lines
are drawn; it is mostly a matter of moving them into files.

### 4.2 `struct viewport_server` has grown into a bag

`include/viewport.h:184-389`. Around 120 members, mixing protocol managers, 20
`wl_listener`s, cursor state, idle state, gesture state, drag state and the
shell's active-output string.

Grouping into `server->protocols`, `server->input`, `server->idle`,
`server->drag` would not change a line of logic, but it would make
`init_listener_links()` (`src/server.c:73-105`) — which is a hand-maintained
list of 45 listeners that must be kept in sync with the struct, with a comment
saying so — mechanical instead of error-prone. A missed entry there is a
segfault on a failing startup path, which is the path least likely to be
exercised.

### 4.3 `data/shell/shell.js` is 3144 lines in one file

There are clean seams already marked by the banner comments: the tiling tree
(~190-510), the scrolling strip (~520-800), session save/restore (~800-990),
notifications (~990-1130), the overview (~1170-1300), resize (~1500-1760),
geometry reporting (~1770-2120), outputs and workspaces (~2125-2440), the view
lifecycle (~2440-2770), the bar (~2770-2900), and dispatch (~2900-3144).

Splitting into ES modules with `<script type="module">` in `index.html` is
straightforward *except* for `tests/shell.test.js`, which reads the single file
and evaluates it against a stubbed DOM. That harness is 989 lines with 94
assertions and it is the only thing testing the layout engine — it must not be
broken for a cosmetic win.

If this is done, do it as: modules first, then teach the harness to load the
entry module through Node's ESM loader with the DOM stub installed on
`globalThis` before import. The `globalThis.__shell` escape hatch the tests
already use suggests the seam exists. If that turns out to be awkward, leaving
the file as-is is a defensible call — it is well-sectioned and the tests are
worth more than the file count.

### 4.4 Dead code in `reportGeometry()`

`data/shell/shell.js:1843-1845`.

```js
if (prev && prev.scale !== scale) {
  /* Scale alone changed — worth a message even when the rect did not. */
}
```

An empty block. The condition is reachable — the early return above fires only
when the rect, the scale *and* the clip all match, so a scale-only change falls
through with `prev.scale !== scale` true — but the body is empty, so it does
nothing either way. What it describes is already handled by the code below it,
which sends the message unconditionally once the early return has been passed.

Delete the `if` and move the comment onto the `send()` it actually explains.

`reportGeometry()` also returns `undefined` (no view), `false` (zero-sized) and
`true`/`false` (changed or not) from three different paths. `pumpGeometry()`
reads it as a boolean so nothing is broken, but the contract should be one type.

### 4.5 `g_free`/`free` are mixed across the same allocations

`src/ipc.c:1270` does `free(server->active_output)` on a pointer set by
`g_strdup()`; `src/server.c:695` does the same. `viewport_output_revert_cancel()`
(`src/ipc.c:867-880`) does `g_free(revert->name); free(revert);`.

This works — glib's default allocator is `malloc` and has been since 2.46 — so
it is a consistency point, not a bug. But it will read as a bug to the next
person, and it will be a real one on the day someone sets a custom `GMemVTable`
or builds against a glib with a different allocator. Pick one per allocation
site and stay with it.

---

## 5. Testing and CI

### 5.1 The compositor half of CI does not run

`.github/workflows/ci.yml`. The shell logic job runs four node invocations in
about a minute. The compositor job is disabled, correctly, because linking needs
WPE WebKit and `flake.nix` builds it from an overridden source that no binary
cache has — a 20-40 GB build tree on a runner with 14 GB of disk.

The header comment names two ways to enable it. There is a third that is less
work than either: **publish the `wpewebkit` derivation to a binary cache.**
`flake.nix:174-177` already exposes it as its own package output
(`packages.wpewebkit`), so:

- Add a Cachix (or attic) push step on a manually-triggered or weekly workflow
  that builds `.#wpewebkit` on a runner with a large disk, or once from a local
  machine.
- The per-push compositor job then substitutes the prebuilt WebKit and only
  compiles ~8k lines of C, which fits comfortably in a hosted runner's budget.

WPE WebKit's version is pinned in `flake.lock`, so the cache is only rebuilt
when that input moves. This turns `tests/capture.test.sh`, `tests/lock.test.sh`
and `tests/output-order.test.sh` from "exists but never runs in CI" into a real
gate — and those are the tests covering the paths the history shows are hardest
to get right.

### 5.2 There are no unit tests for any C code

`meson.build:261-338` registers four compositor tests, all of which start a real
headless compositor and drive Wayland clients against it. They are good tests.
They are also slow (120-second timeouts), and they cannot reach the pure
functions where the bugs actually were:

- `transform_from_name()` / `transform_name()` round-tripping (`src/ipc.c:576`)
- the proposed checked JSON accessors from §2.1
- `viewport_binding_add()` parsing — `src/binding.c` is 619 lines of chord
  parsing and mode qualification, and commit `e8b3769` ("plug the leaks on a
  binding that fails to parse") shows what lives in there
- `viewport_output_config_for()` wildcard precedence (`src/config.c:372`)
- `formatBytes`-equivalent parsing in `src/status.c:85-130`

None of these need a display, a renderer, or a `wl_display`. Building the
relevant `.c` files into a small `viewport-unit-test` executable with a handful
of `assert()`s would take an afternoon and would run in milliseconds on every
push, alongside the shell tests that already pass on hosted runners.

Start with `binding.c` and the IPC accessors — those are where malformed input
meets parsing code.

### 5.3 `viewport_ipc_handle()` is the obvious fuzz target

It takes a length-delimited buffer from an untrusted-ish source (any page served
at `config.url`), it has had two crash fixes, and it has no state that a fuzzer
cannot stub. A libFuzzer harness — `LLVMFuzzerTestOneInput` calling
`viewport_ipc_handle(&stub_server, data, size)` against a zeroed server with
empty `wl_list`s and `web = NULL`, `ipc = NULL` — would cover the whole dispatch
table.

`§4.2`'s struct grouping and `§1.3`'s removal of file-scope state both make this
easier, which is a reason to do them in that order. Add
`-Db_sanitize=address,undefined` and it doubles as the ASan run that the
`build-asan/` directory in the tree says is already part of the workflow.

### 5.4 No sanitizer job

There is a `build-asan/` tree locally and `scripts/asan-hotplug.sh` in the repo,
so ASan is clearly part of how this gets debugged — but nothing runs it
automatically. Once §5.1 lands, a second meson job with
`-Db_sanitize=address,undefined -Db_lundef=false` running the same four
compositor tests would catch the lifetime bugs this codebase has repeatedly had
(`0527403`, `de20263`, `0519d2b`, `a4ca8c9` are all in that family) at the point
they are written rather than at the point someone unplugs a monitor.

---

## 6. Repo hygiene

### 6.1 `.gitignore` misses the sibling build directories

`.gitignore` lists `build/`, but the tree also contains `build-asan/` and
`build-release/`, and `scripts/asan-hotplug.sh` implies more. They are currently
invisible only because of a personal `~/.gitignore`; a fresh clone by anyone else
shows two large untracked directories.

Change `build/` to `build*/`.

### 6.2 `flake.nix` has no `checks` output

`nix flake check` currently verifies nothing. Adding

```nix
checks.${system}.viewport = viewport.overrideAttrs (_: { doCheck = true; });
```

wires the meson suite into the standard entry point, which is what a NixOS
contributor will reach for first — and it makes the flake self-verifying for
anyone consuming it as an input.

### 6.3 No formatter or linter config

No `.clang-format`, no `.editorconfig`, no ESLint config. The C is consistently
tabs-with-80-columns and the JS is consistently 2-space, so the conventions
exist — they are just not written down anywhere a tool or a new contributor can
read them. A `.clang-format` matching the current style (`Linux` base,
`ColumnLimit: 80`, `UseTab: Always`) and an `.editorconfig` are cheap and stop
the first outside patch from arriving reformatted.

`clang-tools` is already in the devShell (`flake.nix:183`), so the tooling is
present and only the config is missing.

### 6.4 `README.md` is 986 lines

It is genuinely good documentation — the IPC protocol, the layout models, the
config schema, the debugging guide and the "when the shell breaks" section are
all worth having. But it is also the file someone lands on first, and the
architectural explanation at the top (lines 1-100, which is the best part) is
followed by 880 lines of reference material.

Suggestion: keep lines 1-100 plus Build and Install in `README.md`, and move
Configuration, IPC, Window rules, Layout models, Writing a shell, Status and
Debugging into `docs/`. Nothing needs rewriting — only the file boundaries move.

---

## Suggested order

1. §1.1 — the NixOS module URL default. One-line user-facing bug.
2. §3.2 — bar rebuild on the status tick. Small, measurable, and finishes a fix
   that was already started.
3. §1.2 — cursor manager leak and the theme-only reload.
4. §2.1 — checked JSON accessors, then §2.2 the dispatch table.
5. §5.1 — cache WPE WebKit so the compositor tests run in CI. Everything after
   this is worth more once it lands.
6. §3.1 — coalesce the drag deltas.
7. §1.3, §5.2, §5.3 — remove the file-scope scale state, then add unit tests and
   the fuzz harness that it unblocks.
8. §4.x, §6.x — structure and hygiene, as they stop being convenient.
