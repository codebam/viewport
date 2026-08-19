# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Releases are tagged `vX.Y.Z`. The list below starts at the release the tree is
currently cut from; earlier history lives in `git log`, which this file exists
to summarise rather than to duplicate.

## [Unreleased]

### Added
- An on-screen keyboard, HTML and CSS drawn by the shell rather than a second
  Wayland client — `input-method` and `virtual-keyboard` were already wired
  up and touch was already complete, so the last piece was a keyboard to type
  on. It comes up on its own when the focused client enables a
  `zwp_text_input_v3` (`osk.wanted`), or by hand with `Mod4+Shift+k`, and it
  types by pressing keys rather than by committing text: `osk.key` is handed
  straight to `inject_keysym`, the same call remote-desktop keyboard
  injection already used, because `zwp_input_method_v2`'s `commit_string`
  only reaches a client that has bound text-input and enabled it — every
  toolkit does for a real field, a terminal emulator typically does not — and
  a keyboard that only worked one of those ways would go silent on exactly
  the desks that need it most. Letters and symbols are always sent in their
  base, unshifted form, with the shell wrapping a tap in a real `Shift_L`
  press of its own whenever the key it drew needs one, because this
  compositor can press a key but not a glyph; Caps Lock is the same trick
  played once, toggling the seat's real lock modifier so every letter after
  it needs no wrapping at all. Nothing here repeats a character on a timer —
  a key held down is `pressed: true` once and `false` on release, exactly
  like a hardware one, and the seat's own keyboard repeat does the rest.
- Wi-Fi and Bluetooth pickers. The bar has always reported link throughput,
  which says a network is being used and nothing about which one or how to
  get on it. NetworkManager and BlueZ both live on the system bus, which the
  page cannot reach, so the compositor talks to them and the shell draws the
  lists — the same split the tray and the power picker already run on, and a
  client of whatever already manages the machine rather than a second thing
  with an opinion about networking. `Mod4+Shift+n` opens the Wi-Fi picker (so
  does clicking the bar's network module) and `Mod4+Shift+t` the Bluetooth
  one. Joining a network that is saved or open is one click; one that is
  neither gets a real passphrase field, which asks the compositor for the
  keyboard with `shell.focus` and hands it back afterwards. A network joined
  here is an ordinary NetworkManager connection that `nmcli` and every other
  applet can see, and the passphrase goes into NetworkManager's secret store
  rather than anywhere in this compositor. Pairing registers a
  `NoInputNoOutput` agent — the piece `bluetoothctl` makes you type `agent on`
  for — and deliberately not as the session's *default* agent, because that
  would auto-accept incoming pairing requests as well. Neither radio is
  touched until a picker opens and both stop when it closes: a scan is the
  radio transmitting, and one nobody is looking at is a battery cost with
  nothing on screen to account for it. New messages: `network.update`,
  `bluetooth.update`, `network.scan`, `network.connect`, `network.disconnect`,
  `network.radio`, `bluetooth.scan`, `bluetooth.power`, `bluetooth.device`.
- The RemoteDesktop portal. `org.freedesktop.impl.portal.RemoteDesktop`, so an
  application can drive this machine and not only watch it — remote support, a
  call where the other end takes the mouse. It is the ScreenCast interface with
  input added and it is served from the same object, the same bus name and the
  same session table: the frontend uses one session handle across both, so a
  configuration that answers one of them from another backend cannot work, and
  `data/portal-config` now names this compositor for both. CreateSession,
  SelectDevices and Start, then one call per input event, each checked against
  the devices the person at the machine allowed and handed to the seat through
  the `inject_*` helpers in `crates/viewport/src/input.rs` — which grew three:
  `inject_pointer_relative` for a mouse that moved rather than one put
  somewhere (a client with the pointer locked reads nothing else),
  `inject_axis` for scrolling, and `inject_keysym` for a caller that has a
  character rather than a key. Consent goes through the screen-share chooser
  with the device set named in it, because being watched and being typed into
  are different questions. A grant is never remembered — `restore_data` is the
  right trade for a picture and the wrong one for a keyboard — and a session is
  refused outright when no desktop page is drawing, rather than falling back to
  granting it the way a screen share falls back to sharing the focused window.
  Version 1 of the interface: version 2 promises ConnectToEIS, which needs a
  libei server this compositor has not got, so the method answers
  `NotSupported` and the frontend stays on the path that works.
- A battery widget, lid policy and power-profile picker. UPower's
  DisplayDevice is the charge the bar should show, `LidIsClosed` is the
  hinge, and the power-profiles daemon is `power-saver` / `balanced` /
  `performance`. The compositor reads them — the page has no bus — and only
  talks to UPower when a `battery` widget is on the bar or a lid policy is
  in force. `"lid"` is `lock`, `blank`, `suspend` (via logind) or `ignore`;
  absent is lock when `idle.lock_command` is set, otherwise blank. Clicking
  the widget opens a picker of the profiles the daemon listed.
- `docs/layout-extension.md`, the contract a sixth layout implements: what
  it must plan, render and clear, what it may not transform, how session
  restore differs, and the three name lists that have to stay in agreement.
  There is no plugin loader; adding a model is still a file and those lists.
- `inject_pointer`, `inject_button` and `inject_touch_*` next to `inject_key`,
  so a scripted pointer, a scripted finger and a real one share a path. The
  RemoteDesktop portal calls these; the on-screen keyboard does too.

- A clipboard history, kept by the compositor and drawn by the shell. A
  Wayland selection is not a buffer anywhere — it is an offer from the client
  that owns it, and it dies with that client, which is why closing the
  terminal you copied from empties the clipboard and why every desktop grows a
  manager for this. The compositor is already the program every selection
  passes through, so keeping the last few is reading what is being offered
  rather than standing up a daemon to hold a `wlr-data-control` connection
  open. `Mod4+Shift+v` opens a picker; choosing an entry puts it back on the
  clipboard with the compositor as the owner, so it can be pasted long after
  the application that copied it has exited. Text only and the clipboard only:
  the primary selection would mean an entry for every word dragged over with a
  mouse. `clipboard_history` says how many to keep and `0` turns it off
  entirely, on reload as well as at startup.
- Pasting into a Wayland window from an X11 one works. The compositor
  advertised XWayland's selection to every Wayland client and then answered
  nothing when one asked for it, because `SelectionHandler::send_selection`
  was never implemented — copying in an X application and pasting in a Wayland
  one did nothing at all, with nothing in the log. The server-side selection
  now records who owns it, so a request is either forwarded to the XWM or
  answered from the clipboard history.
- A media widget for the bar: what is playing, and the buttons to drive it.
  Every player on a Linux desktop publishes MPRIS — a bus name beginning
  `org.mpris.MediaPlayer2.`, an object, and metadata behind it — which is why
  `playerctl` works everywhere and why `mpris` in `bar_widgets` needs nothing
  installed. The compositor reads it rather than the shell, because the page
  has no bus and a widget shelling out to `playerctl` twice a second would be
  two processes a second on an idle desktop. It is the one widget that is not
  a line of text: a cover, previous, play/pause, next and the track — and only
  the buttons the player answers for, since `CanPause` is false on a live
  stream that can only be stopped and a button that does nothing is worse than
  no button. Where several players are running the one that is playing wins,
  which is the rule `playerctl` uses. With no media widget on the bar the
  compositor opens no connection for it and follows no player at all, the same
  rule that already keeps `wpctl` from being spawned for a bar with no volume
  widget.
- Tray menus are drawn by the shell. An item points at a
  `com.canonical.dbusmenu` object — Canonical's specification, which the tray
  one says nothing about, and which is what GTK and Qt both publish a menu
  through — or it implements `ContextMenu` and draws its own window. Both are
  in use, so the shell asks the same question for every item and the
  compositor decides which it was: a menu object is read here and sent over,
  and everything else is asked to draw its own. Reading one is `AboutToShow`
  and then `GetLayout` at depth −1, because a menu is usually built when it is
  asked for and because a round trip per submenu would be a menu that opens in
  stages. Rows an application marked invisible are dropped rather than sent to
  be hidden, labels lose the mnemonic marker the toolkit would have drawn, and
  a row's icon is resolved the same way an item's is. Submenus open in place
  rather than flying out: everything the shell draws over a window is one
  rectangle it has to name, and a panel outside that rectangle would be drawn
  behind the window it is meant to be over. Choosing a row and dismissing the
  menu are both reported to the application — several rebuild their menu on
  close, and one that is never told keeps serving a stale one.
- A system tray. The compositor claims `org.kde.StatusNotifierWatcher` and a
  host name beside it, follows every application that registers an item, and
  forwards the tray to the shell, which draws the icons on the bar — the same
  arrangement notifications have had, and for the same reason: the shell is
  the desktop, and a tray drawn by a separate bar would be a second program
  with a second configuration language floating over a compositor that already
  knows where everything is. There is no Wayland protocol for this and there is
  not going to be one, so Discord, Steam, Nextcloud and everything else that
  lives in a tray had nowhere to live on this desktop at all. Both
  registration forms are accepted, because both are in use — Qt sends a bus
  name and Ayatana's library sends an object path, and handling one of them is
  a tray that works for half the desktop. Icons reach the shell as `data:`
  URLs: an icon name means nothing to a browser and a `file://` path is refused
  in a shell loaded over `http://`, so a name is resolved against the icon
  themes and a pixmap is encoded as a PNG here — uncompressed, because a
  deflate implementation is a lot of machinery to save two kilobytes on a
  message sent when an application starts. Left click activates, right asks
  for the menu, middle is the secondary action and the wheel scrolls. Items
  Items that publish a `com.canonical.dbusmenu` object have their menu read
  and sent to the shell to draw; items implementing `ContextMenu` draw their
  own window. `"tray": false` turns the whole thing off, on reload as well as at
  startup, and `icon_theme` says which theme names are resolved against.
- `docs/roadmap.md`, which is what is missing and why each part of it belongs
  in this program rather than in a daemon beside it.
- Live configuration reload (`--watch-config`, or automatically when `--watch-shell`
  watches local shell assets). Editing `~/.config/viewport/config.json` patches
  live state (gaps, borders, bindings, wallpaper, rules) and notifies the shell
  without losing the session.
- The `org.freedesktop.impl.portal.Screenshot` portal implementation, letting
  desktop screenshot requests capture whole outputs, windows, or interactive
  regions directly via the compositor.

### Fixed
- Starting an application with a large tray icon no longer takes the desktop
  down with it. Electron's tray publishes one 512x512 pixmap and no smaller
  copy, so picking the pixmap nearest the bar's 22 pixels picked a megabyte of
  ARGB; this file's PNG writer does not compress, so a megabyte is what came
  out, and the 1.4MB data URL was a single control message larger than a
  client's whole backlog allowance. The compositor dropped the shell's
  connection the moment such an application started — after which no window
  was ever placed, the bridge broke, and the shell died and restarted, which
  is what opening a music player looked like from the outside. Pixmaps are now
  scaled down to four times the size asked for before they are encoded, with a
  box filter over premultiplied alpha so a shrunken logo keeps its strokes and
  grows no dark halo.
- A control client is no longer dropped for being sent a lot at once. The
  backlog test asked how much a client was owed and not whether it was reading
  any of it, so one oversized event killed a connection that had done nothing
  wrong — and the connection was usually the shell, because the shell is what
  everything is sent. A client over the limit now has five seconds to take
  some of it, any successful write clears the clock, and only a genuinely
  stuck reader is reaped. An absolute ceiling still bounds what that patience
  can cost the compositor's heap.
- The three or four pixels of wallpaper at the corners of a floating window,
  for real this time. Two more places were copying the page's background over
  the window underneath. The corner wedge is a copy of the shell's buffer, and
  with a radius much past the border's width the hole's square corner pokes
  *outside* the page's rounded frame — where the buffer is not border but
  whatever the page drew behind the frame, which is the wallpaper. The wedge
  is now held to the frame's own outer arc. And the border sides drawn above
  the windows underneath used the same outward-rounded staircase as a window's
  corners, which put their edge a pixel past the page's antialiased arc — the
  right direction for a client, which covers those pixels, and exactly the
  wrong one for a piece of the shell's buffer. Shell pieces round inward now;
  the two directions are separate constructors, and tests hold each staircase
  to its own side of the curve. Verified on a nested session with a window
  lifted over another: the pre-fix build paints page background between the
  ring and the window below at every corner, the fixed build paints none —
  headless runs never composite the shell buffer at all, which is how the
  earlier "verifications" of this bug passed without testing it.
- A setting changed over the control socket reaches the windows at once. The
  shell applies a `config` message by writing custom properties and reading
  them back on the next geometry pass — and nothing runs a geometry pass on
  its own, so on a desktop nobody was touching the new value sat in the page
  until something else moved. `config.border --radius 20` did nothing until a
  window opened, and turning `border.smart` off did not bring the corners
  back; opening a second window is what "fixed" both, because opening one lays
  the desktop out again. A config message now does that itself.
- A rounded window is no longer scanned out square. `RoundedRenderElement`
  offered the wrapped element's buffer for direct scanout, and a hardware
  plane draws a rectangle — a buffer, a source rectangle and a destination
  rectangle, with nowhere to say "with the corners taken off" — so on DRM a
  window that was a candidate for a plane went to the display controller whole
  and the band splitting that does the rounding never ran. Headless and nested
  composite everything, which is why nothing here ever saw it. An element that
  actually rounds something now declines the plane; a fullscreen window is
  drawn square anyway and still takes one.
- A lone window keeps its rounded corners. `border.smart` used to default to
  following `gaps.smart` — smart gaps push a lone window against the edge of
  the screen, and a rounded corner there is a notch of wallpaper in the corner
  of the monitor. The argument holds; the default did not. A radius that had
  been asked for went unhonoured on every desktop with one window on it, and
  came back the moment a second window opened, which reads as a broken setting
  rather than as a rule being applied. It is its own setting now and off unless
  asked for: `"border": { "smart": true }` is sway's `smart_borders`.
- The last few pixels of wallpaper in the corner of a window. The rounding
  used to put the boundary between the client and the shell's border curve
  *on* the curve, by rounding to nearest — which leaves the outermost pixel of
  each row one the page antialiased, part border and part hole, and the hole
  in the shell's buffer is the desktop's own background. Over a window a
  floating one is lifted above, that showed as three or four pixels of
  wallpaper at each corner. The boundary is rounded down now, a hair outside
  the curve, so every pixel the compositor takes from the border is inside the
  border and the client covers the rest; the corner is a pixel squarer than
  the page's own arc and nothing shows through it. The two halves are derived
  from one function, because a pixel that belongs to neither falls through to
  the desktop.
- The corners a floating window's border curves into no longer punch four
  wedges of wallpaper through the window underneath. The corners were copied
  back from the shell as whole squares — the curve *and* the piece of the hole
  inside it — on the reasoning that the client covers the inside. A client
  covers what it is drawn on, and a terminal is a few pixels short of its hole
  in each direction because it rounds down to whole cells, so what showed in
  that margin was the desktop's own background, over whatever the window was
  floating above. Only the wedge the curve actually occupies is copied now,
  which is the complement of the rounding the client is cut to and is derived
  from it so the two cannot disagree; where a client falls short of its hole,
  the window underneath shows through, as it does along the straight edges.
- VRAM climbing for as long as a screen share was open — a couple of hundred
  megabytes a minute on a shared 1440p screen, none of it given back until the
  session ended. Two things were doing it, and a share is what made both
  visible: it keeps every client painting, and an idle desktop paints nothing.
  - The Vulkan renderer keeps one image per shared-memory `wl_buffer` it has
    uploaded, so a client that paints every frame is not reallocated and
    re-uploaded every frame. Those entries are keyed by the buffer's object id
    and the renderer offers `forget_shm_buffer` for the compositor to call when
    one dies — which nothing ever did. `buffer_destroyed` was an empty body, so
    every shm buffer any client had ever destroyed left its image behind for
    the life of the session. Destroyed buffers are now queued there and
    forgotten once a turn of the event loop, which is before the renderer is
    moved out to draw with.
  - A capture composited into a buffer it allocated and freed per frame: a
    whole screen off the GPU, thirty times a second, for every share on the
    readback path — and one `vkCreateImage` and one import per frame with it.
    The buffers are now kept between frames by shape, at most four of them, and
    let go of once nothing is being captured.
- The screencast test's stand-in frontend never claimed
  `org.freedesktop.portal.Desktop`, so every call it made was refused as soon
  as the compositor started checking that every caller on the impl interface is
  the frontend. It claims the name now, and retries the first call while the
  compositor's watcher catches up with the signal. What still fails there is a
  token that no longer survives a compositor restart, which is a question about
  the remembered table rather than about the test.
- A share that was resized never went back to being drawn into directly. Only
  the Vulkan renderer can allocate the buffers a consumer imports, and the
  renderer is generic where a resize is answered — so the DRM backend answered
  every renegotiation with no buffers at all, which is the offer of a DMA-BUF
  format withdrawn. One resize of a shared window, and the share spent the rest
  of the session compositing into an offscreen and reading it back. The
  allocation is now asked of whichever renderer is drawing, through
  `Captures::cast_targets`; GLES still answers with nothing, which is what puts
  a nested session on shared memory as before.
- A floating window's rounded corners are drawn over the windows underneath
  it. Everything the shell paints is one buffer under every client, so a
  lifted window's border is copied back on top as four sides — and four sides
  stop at the client's rectangle, while the border's curve crosses into it.
  The piece between the client's own rounded corner and the corner of its hole
  therefore stayed where the shell drew it, under whatever the window was
  floating above: a dialog over a browser had a browser-coloured notch at each
  corner instead of a border. The corners are now copied back as well, behind
  the client rather than in front of it, so the client covers the part of each
  square it fills and the curve is what shows.

## [0.1.8] - 2026-08-17

### Added
- A binding's output and exit status reach the log. A spawned command had its
  stderr on `/dev/null` and its status thrown away, so the log said only that
  something had been started — a screenshot script that died on a bad argument,
  a missing tool or a `set -e` two lines in looked exactly like one that
  worked, and there is no terminal watching to tell them apart. The first
  twenty lines of a child's stderr are now logged against the command that
  produced them, and a non-zero exit is logged with its status. Twenty because
  a failing script says what is wrong at the start, while a browser left
  running for a day writes tens of thousands of lines that would bury
  everything else; reading continues past the cap even though logging stops,
  since a pipe nobody drains fills up and the next write would block the child
  mid-task.

### Changed
- The shell is restarted with a backoff, and is no longer given up on. It used
  to be respawned on the next tick every time, five times a minute, and then
  left down for the rest of the session. Both halves of that were wrong on a
  real fault: an AMD GPU that had run out of memory — a game, a screen capture
  and a language model on the same card — rejected every command submission on
  it, Mesa aborts a process when that happens, and the shell, Chromium and OBS
  all died together. The shell was then respawned five times in ninety
  seconds, each one asking the same exhausted GPU for another 5120x1440
  buffer, before the desktop went blank until the session was restarted. Now
  the first restart is immediate and each one after it waits twice as long
  (1s, 2s, 4s, 8s), and a run that gets through those keeps retrying every
  thirty seconds rather than stopping — so the desktop comes back on its own
  once the cause has gone, and a page that genuinely cannot load costs one log
  line every thirty seconds instead of a session.
- A frame costs less, and the desktop stops re-answering questions it has
  already answered. The shell posts one `view.layout` per window per animation
  frame, and each one used to end by restacking the whole space and re-asking
  which output colour every feedback surface sits on — questions about the
  desktop rather than about the window the message was for, so eight windows
  got the same answer eight times, each time allocating and scanning every
  view. Both are now owed once and settled once per batch, before anything
  else in the event loop can see a stack that has not been restacked. Around
  that: `views` keeps a surface-to-position index, checked against the list
  before it is trusted and falling back to the old walk when it is stale, so
  finding the window behind a commit, a focus change or a hit test no longer
  costs a `WlSurface` clone per window passed; the bridge's writer thread
  drains whatever the producer has already queued into one `write_all`,
  bounded at 64 KiB and never waiting for a straggler, which takes an
  eight-window desk at 60fps from around 480 small syscalls a second to a
  handful; and `frame_for` stops minting border element ids for windows with
  no view, stops building a `String` key per output per frame for a debug line
  nobody is listening to, and stops looking up a lock surface in a map that is
  empty outside a locked session. No behaviour changes from any of it.

### Fixed
- A second locker taking a screen from a lock screen that is drawing, which
  could leave a session locked after a correct password. Smithay grants every
  `ext-session-lock` request and leaves the decision to the compositor, so
  nothing refused the second one; taking it also cleared `lock_surfaces`,
  dropping the surfaces of a locker that was still running and still drawing.
  Two clients then owned one screen, only the newer was rendered, and
  unlocking the one you could see left the other holding a lock nothing on
  screen could reach. It needed nothing unusual to hit: the idle deadline and
  the `lock` binding both call `lock_session` and neither asked whether the
  session was already locked, so locking by hand and then letting the idle
  timer expire was two lockers. The rule is about pixels rather than about who
  asked first — a lock screen that is drawing may not be taken over, one that
  is not may, because running another locker is the only way out of a locker
  that crashed and `check_lock_screen` says to do exactly that. A refusal
  drops the `SessionLocker`, whose `Drop` sends `finished`.
- `Mod4+Shift+b` turning the screens off and something turning them straight
  back on. `render_if_needed` was the only place that checked whether the
  session was blanked, and four paths call `render` around it: the vblank of a
  flip still in the air when the screens went off, the watchdog resuming a
  chain that has legitimately stopped, a connector rescan, and a session
  resume. Any one of them queues a frame, and a queued frame is what wakes a
  panel. The check now sits where the frame is built and committed, and again
  in `render_pass`, the last gate before KMS — so a monitor that arrives
  during a blank comes up asleep with the rest rather than lighting on its
  modeset, which a DisplayPort screen does on its own after sleeping.
- The cursor being drawn at whatever size the theme happened to ship rather
  than the size asked for: the nearest image to `XCURSOR_SIZE` * scale was
  drawn at its own resolution, so asking for 40 from a theme with 32 and 48
  gave 32. The nearest image is now picked for resolution only and drawn with
  an explicit source rectangle and logical size, with the hotspot rescaled to
  match or the pointer aims off the tip of the arrow. Two related leaks are
  closed with it: a reload rebuilt the compositor's theme without telling the
  settings portal, so every toolkit sized its own cursors from the startup
  value for the rest of the session — the portal is now updated, announced,
  and the pointer redrawn rather than waiting for the next motion — and
  neither `XCURSOR` variable reached the session environment, so a client
  started by systemd or activated over D-Bus picked its own default.
- The keymap on an empty desktop not scrolling when it does not fit. The
  tutorial block has had a max-height and `overflow-y: auto` since it was
  written and neither did anything: `.empty` is inert so a click reaches the
  desktop behind it, `pointer-events` is inherited, and the wheel went through
  the list to the page — the one box on screen meant to scroll was the one
  that could not. A `columns: 2` box with a bounded height does not scroll in
  any case, since multicol lays out another column to the right and
  `overflow-y` alone computes the other axis to `auto` as well, so the chords
  that did not fit went behind a horizontal scrollbar instead of below the
  fold. The list is now a wrapping flex row, which overflows downwards, and
  takes the pointer itself while the empty state around it stays
  click-through. Flex and not grid because Servo drops `display: grid` and the
  box falls back to `block` — a single ribbon down the middle, which is what
  two columns exist to avoid.
- The border on a zoomed-out window: one thin line hanging below it and
  nothing anywhere else. `border_sides` was given the frame the shell measured
  on screen together with the hole in the client's own pixels, which are the
  same rectangle only at scale 1. A window on a zoomed-out canvas plane — or a
  thumbnail in the overview, or a cold window in solar's outer orbit — is
  drawn small and never asked to resize itself, so the hole was far larger
  than the frame around it: the bottom and right sides started past the
  frame's far corner and clamped to nothing, and the left side kept the
  client's full height. The scale now comes with it and the hole is converted
  to what is actually drawn before the sides are cut out.

## [0.1.7] - 2026-08-15

### Added
- Notifications can make a sound. The compositor claims
  `org.freedesktop.Notifications` itself, which took playback away along with
  the window when it replaced mako and dunst — a notification has been silent
  here since. `notifications.sound_file` and `notifications.sound_name` in the
  config file say what one sounds like by default, and all three of the
  specification's sound hints are honoured: a sender's own `sound-file` or
  `sound-name` overrides the default for its notification, and
  `suppress-sound` silences it, because that hint means the sender is playing
  its own and two sounds for one event is worse than none. Playback is
  PipeWire, which this program already links for the screencast portal, with
  symphonia decoding — no libcanberra, and so no new library in the closure or
  in nine AUR packages. Each sound decodes and plays on a thread of its own, so
  no sender blocks for the length of one, and decoded files are kept because
  the same short sound plays all session. `sound_name` is resolved by the
  sound-theme search written out: data directories, `stereo/` before the flat
  layout, `.oga`/`.ogg`/`.wav`, and `Inherits` followed with `freedesktop` as
  every theme's implicit parent. A session with no sound server plays nothing,
  says so once, and drops `sound` from its reported capabilities so a sender
  knows to play its own.

## [0.1.6] - 2026-08-15

### Changed
- `Mod4` + right-drag resizes from the corner nearest where the drag started,
  rather than always pulling the bottom right one. The whole window is the
  handle — there is no border to aim at — so the quarter the press lands in
  names the corner, which is how sway reads the same gesture. Taking hold of a
  window on its left and pulling left made it *smaller* before this, because
  the only edges that ever moved were the far ones. The compositor sends the
  corner along with the delta; the shell decides what it means, which differs
  per layout: a tiled window trades with the sibling that edge faces, a
  floating one and a window on the canvas pin the opposite corner and move as
  they size. An edge with no neighbour to trade with — the leftmost window's
  left edge — still takes from the other side rather than doing nothing.

### Fixed
- The bar showing the old volume after a scroll. The widget spawned `wpctl`
  through `shell.exec` and asked for a re-sample in the next message, which
  samples the sink before the spawned process has run — so the bar redrew the
  number that was already there and the new one waited for the next two-second
  tick. `status.volume` changes and samples in that order.
- The compositor losing `org.freedesktop.Notifications` for the rest of the
  session once anything else took it. zbus asks for a name with `DoNotQueue`
  by default, which also decides what happens *after* being replaced — a
  replaced owner is dropped rather than queued — so when the program that took
  it exited, the name was owned by nobody and every notification failed with
  `ServiceUnknown` while the compositor sat there serving the interface. Both
  names it claims are queued for now, and neither is taken from whoever holds
  it: `ReplaceExisting` was in zbus's default too, and it is what let a nested
  compositor take the notification daemon and the portal backend from the
  session it was started inside — the opposite of what `appearance.rs` has
  always said it did.

## [0.1.5] - 2026-08-14

Packaging, and nothing else: the compiled compositor is what 0.1.4 shipped.
The packages are named after the compositor rather than the toolkit it was
rewritten on, every engine has a `-git` and a `-bin` form beside its source
recipe, and all nine sit in one directory each under `packaging/aur`. This is
the release whose artifacts were built from the recipes in its own tree, which
0.1.4's — cut before the rename — could not be.

### Shipping
- One directory per AUR package, all nine under `packaging/aur`, named exactly
  after the repository each one is pushed to — the three source recipes moved
  there from `packaging/arch`, which is gone. A push is a copy of a directory
  now rather than a rule about which file lives where. `build-in-container.sh`
  and `Containerfile` moved up to `packaging/`, and the build script takes a
  package name (`viewport-wpe-git`) as well as an engine (`wpe`), so a `-git`
  or `-bin` recipe can be built before it is pushed anywhere.
- Nine AUR recipes rather than five: every engine now has a `-git` form that
  follows `main` and reports `0.1.4.rN.gSHORT`, and Chromium has the `-bin`
  form the other two already had. The `-git` recipes are their source recipe
  with three differences — the name, a `pkgver()`, and a branch instead of a
  tag — so a change to a build step belongs in `packaging/arch` and then in its
  twin.
- The packages are named after the compositor rather than after the toolkit it
  was rewritten on: `viewport-webkitgtk`, `viewport-wpe`, `viewport-chromium`
  and the two `-bin` recipes beside them, each providing `viewport` and
  conflicting with the others, since a system takes one. Nothing had been
  published under the old names, so there is nothing to migrate. The recipes,
  the container image and every URL follow the repository, which is
  `codebam/viewport`.

## [0.1.4] - 2026-08-14

A desktop that can be given a picture, and a run of fixes to the things that
touch two monitors — a border drawn onto the screen next door, a capture of
both screens showing the second one empty — plus the keys and clicks that
reached nothing: the play/pause key, `Mod4+Tab` on a layout that keeps windows
out of view, and the bar's own workspace pills and window titles.

### Added
- A colour or a gradient as the wallpaper: `wallpaper` takes a CSS value —
  `#1a1b26`, `rgb(...)`, `linear-gradient(...)`, `url(...)` — as well as a
  path, so a colour scheme with no photograph in it does not need one.
- A picture for the desktop background, set three ways: `wallpaper` and
  `wallpaper_mode` in the config file, `--wallpaper` and `--wallpaper-mode` on
  the command line, and `config.wallpaper` on the control socket for changing
  it without a reload. The five fittings — `fill`, `fit`, `stretch`, `center`
  and `tile` — are stylix's `imageScalingMode` spelled the same way, so a
  themed NixOS session hands its settings straight across; see
  `docs/configuration.md`.

### Fixed
- A window dragged towards the edge of one monitor drawing a strip of the
  *next* monitor's window borders over that monitor's windows. The shell
  measures a frame rather than what it painted of one, and the compositor drew
  the shell's pixels wherever that rectangle reached — which on the screen next
  door is that screen's own desktop. The frame is now reported clipped to the
  output it was drawn on, and the compositor draws a window's frame only on the
  output the shell drew the window on.
- Clicking a workspace number or a window's title in the bar doing nothing
  under `bar: auto`. The bar is on screen only while Mod4 is held, so every
  click on it carries the gesture modifier — and with no window under the
  pointer the press started a *pan*, which swallowed it. The compositor now
  declines its Mod4 gestures over anything the shell drew in front of the
  windows, and the floating bar takes the pointer instead of waving it through.
- A capture of every monitor at once showing the second monitor as its desktop
  and window frames with no windows in them. Each monitor's element list
  carries the whole shell buffer — it spans the layout — and a monitor drawing
  itself is bounded by its own framebuffer, which a capture of the whole desk
  is not: the first monitor's copy of the shell was drawn over every monitor
  after it, with the clients behind it. Each monitor's picture is now held to
  its own rectangle.
- The play/pause media key doing nothing. It was bound as `XF86AudioPause`,
  which xkb puts on the *shifted* level of that key — chords match the
  unshifted keysym, so the binding named a level the key cannot produce, while
  skip and previous worked and made it look like playerctl failing. It is
  `XF86AudioPlay` now, running `playerctl play-pause` rather than `pause`.
- The volume, mute, mic-mute and brightness keys, documented as bound by
  default and bound only in `data/config.example.json`. They are defaults now,
  5% a press.
- The active window's border drawn across the bar. The bar sat on z-index 3
  and three window layers sat above it — floating at 5, the canvas's focused
  window and solar's sun at 4 — and the compositor's copy of the bar is a crop
  of the same page, so the border was over the clock on screen as well. The bar
  is above every window layer now, and a sweep in the shell tests holds it
  there.
- `Mod4+Tab` skipping the windows a layout keeps out of view — a column
  scrolled off the strip, a window panned off the canvas. Those are reported to
  the compositor as not on screen, and its cycle walks what is on screen, so the
  one key whose job is reaching them could not. In those two layouts the chord
  now goes to the shell, which walks the whole workspace and brings the window
  it lands on into view.

### Protocols
- `zwlr_foreign_toplevel_management_v1`: activate, close and fullscreen for
  taskbars (`crates/viewport/src/foreign_toplevel.rs` — see also
  `docs/RUST-REWRITE.md`); maximise and minimise are accepted and not acted
  on, because the shell owns the layout.
- `kde-server-decoration` via Smithay's `KdeDecorationState` — a Qt or KDE
  client that speaks only the KDE verb no longer draws doubled decorations,
  and the manager's default and the per-surface answers both derive from the
  `"decorations"` config key.
- `xdg-toplevel-drag-v1`: not advertised. The tab-tearing global is withdrawn
  rather than carried as an advertised-but-inert object that makes browsers
  take a path the compositor never delivers; see `docs/protocols.md`.
- `wlr-export-dmabuf-v1` and `ext-transient-seat-v1`: not advertised.
  Zero-copy capture has `ext-image-copy-capture` and `wlr-screencopy`, and a
  second seat has no virtual-input back end. An always-refused global leaves a
  client that probes and does not fall back worse off than absence would; see
  `docs/protocols.md`.
- Wire up the color management, HDR, output management and tearing-control
  protocol surfaces the shell and clients use — `color-management-v1`,
  `hdr-output-metadata-v1`, `wlr-output-management` and the smithay fork's
  tearing-control patch — see `docs/protocols.md`.
- Publish the workspaces to outside clients, so an external bar has something
  to draw.

### Tests
- Drive `zwlr_foreign_toplevel_management_v1`, `wlr-output-management-v1` and
  `ext-workspace-v1` over the wire, headless and on a real socket alongside
  the existing capture/paint/output-order/screencast/session-lock tests
  (`scripts/integration.sh`, new `tests/foreign-toplevel*`,
  `tests/output-management*`, `tests/workspace*`). They `wayland-scanner` the
  generated marshalling code and check behaviour, not just presence.
- Keep the Wayland integration tests (`scripts/integration.sh`) driving the
  Rust binary rather than the deleted C compositor — capture, session lock,
  output order and the shell layout variants, all headless on a real socket.
- Run the same Rust suite under AddressSanitizer (`.#asan`).

### Shipping
- Cut `0.1.4`: every place the version is written moves together, and the three
  source recipes in `packaging/arch` go back to naming the tag (`_tag=v0.1.4`)
  rather than the commit they sat on between releases. The `-bin` recipes carry
  the new `pkgver` and their `sha256sums_x86_64` stay stale until the artifacts
  are built and uploaded.
- Move the renderer out to its own repository.
- Point the AUR `-bin` package at the `0.1.3` artifact.
- Ship `viewport-smithay-wpe-bin` alongside `viewport-smithay-webkitgtk-bin`;
  keep a single `viewport-smithay` source recipe at
  `packaging/arch/webkitgtk/PKGBUILD` (see `packaging/aur/README.md`) rather
  than carrying a second copy under `packaging/aur/viewport-smithay/src/`.
  The WPE `-bin` remains not yet pushed until the artifact's real
  `sha256sums_x86_64` is filled.

### Docs
- Add this changelog, a `CONTRIBUTING.md`, and a `viewport(1)` man page
  documenting the binary's command-line flags. The man page is now installed
  as `usr/share/man/man1/viewport.1` from the Arch recipes and from the nix
  package's `postInstall`, and a flag change must be reflected there.

## [0.1.3] - 2026-08-05

The first release cut from the Rust rewrite after it reached parity with the
deleted C compositor and the tree stopped carrying two implementations.

### Added
- A `viewport` binary that nests inside the session it was started from, or
  takes the DRM session from a TTY, and a packaged compositor for Arch
  (`packaging/arch/`) and NixOS (`flake.nix`).
- A `viewport` subcommand for the control socket, so a running session is
  drivable from a terminal without anything else installed.
- The WPE, WebKitGTK and Chromium shell backends, selectable at build time
  with `--features wpe` or at runtime with `--shell-backend`.
- **`servoshell` is the default backend** — `nix build`, `nix run` and
  `programs.viewport.shellBackend` all land on it. It is the lightest desktop
  measured (8.5% of a core under load against 9.9 to 11.5, 357 MB against 449
  to 639, four processes against nine to twelve) and the slowest to paint by
  some way (14 frames a second against 43 to 48). `cef`, the previous default,
  is still the answer for a desktop that should feel quick; see
  `docs/benchmarks.md`.
- Two Servo shell backends, `servo` and `servoshell` — the engine embedded and
  the engine driven, as `cef` and `chromium` are for Blink. `servoshell` runs
  nixpkgs' prebuilt browser and compiles no engine (`nix build .#servoshell`);
  `servo` embeds the `servo` crate from a workspace of its own, so the engine
  build it costs cannot be reached by `cargo test --workspace`, by CI or by a
  compositor rebuild. Neither needs an edit to `data/shell/*.js`: the bridge is
  a user script both ends. See `docs/shell-backends.md`.

### Fixed
- A `--exit-after` deadline that silently did nothing on an idle compositor,
  by arming a timerfd the event loop actually wakes for (see `main.rs`).
- Shell loading on a plain `file://` page when the shared MIME database is
  missing — WebKit treated such pages as empty documents (see the `wpe`
  PKGBUILD's notes on `shared-mime-info`).

[Unreleased]: https://github.com/codebam/viewport/compare/v0.1.8...HEAD
[0.1.8]: https://github.com/codebam/viewport/releases/tag/v0.1.8
[0.1.7]: https://github.com/codebam/viewport/releases/tag/v0.1.7
[0.1.6]: https://github.com/codebam/viewport/releases/tag/v0.1.6
[0.1.5]: https://github.com/codebam/viewport/releases/tag/v0.1.5
[0.1.4]: https://github.com/codebam/viewport/releases/tag/v0.1.4
[0.1.3]: https://github.com/codebam/viewport/releases/tag/v0.1.3
