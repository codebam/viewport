# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Releases are tagged `vX.Y.Z`. The list below starts at the release the tree is
currently cut from; earlier history lives in `git log`, which this file exists
to summarise rather than to duplicate.

## [Unreleased]

### Added
- Native Wayland, Xwayland, and foreign-toplevel maximize requests now fill the
  usable workspace while preserving the bar, gaps, border, and prior window
  placement. `Mod4+m` toggles the same state.
- Window rules accept `capture: false`. Output, region, desk and direct-window
  captures replace matching client pixels, native and related X11 popups, and
  the shell frame with opaque black; portal source selection and restore omit
  private windows. Denials apply before the shell answers, so privacy fails
  closed during shell startup or failure.
  Sandboxed clients no longer receive direct screencopy globals and must use
  the consent-bearing portal.
- Outputs can explicitly mirror another same-GPU physical head. Mirror sinks
  scan out the source scene without creating a second desktop, workspace or
  input rectangle; settings and `output.layout` still expose every physical
  head. Per-output VRR now supports `off`, `always`, `fullscreen` and
  `game-or-video`, with configured and effective state published separately.

### Fixed
- ScreenCast now refuses a request when no trusted consent UI is available
  instead of silently sharing the first source. The Screenshot backend now
  accepts calls only from the current desktop-portal frontend.
- The screencast restore integration test now checks the permission model the
  compositor implements: restore while its application-scoped, in-memory row
  still exists, and ask again after a compositor restart. Its private D-Bus has
  no activation directories, so the host's real portal frontend cannot take
  the fake frontend's name between passes. The notification-centre test now
  sends a replacement from the same D-Bus connection as the original; two
  `gdbus` processes have different owners and were correctly refused.
- Out-of-process shells now have a four-megabyte, byte-counted outbound IPC
  queue. The old 64 KiB batch limit bounded one write but left the channel
  behind it unbounded, so a page producing messages faster than the compositor
  drained them could grow the shell process without limit. Overflow now closes
  the connection instead of dropping an arbitrary state-changing command.
- Launcher commands now expand Desktop Entry field codes according to the
  specification: file and URL placeholders remove only themselves, `%%`, `%c`,
  `%i` and `%k` are expanded, and an unterminated quoted command is refused.
  In particular, an argument after `%U` is no longer mistaken for part of the
  placeholder and silently discarded.
- The launcher finds entries below subdirectories of `applications/` and uses
  their desktop-file IDs for overrides. Its worker caches the parsed tree for
  two seconds and keeps only the newest pending filter query, so typing no
  longer queues a filesystem scan for every stale keystroke.
- The default device — where the shell allocates, what clients are told about,
  the card behind the capture targets and the shell's copy modifiers — is the
  first GPU that is online rather than `devices[0]` no matter its state. A
  primary card that is mid-reset or gone during a hotplug previously answered
  as if it were live, so a screen share or a shell copy read a renderer whose
  GPU was not; the default is now the card that actually answers, and
  `devices[0]` only when none of them do. `devices[0]` was the right answer
  all along on the only path that existed — a single GPU — so nothing changes
  there, and it is the one the multi-GPU audit named as the assumption left
  behind.
- Taskbars drawn per monitor are told which monitor each window is on. The
  wlr foreign-toplevel protocol carries an `output_enter`/`output_leave` pair
  for exactly this, and announce time was the only place it was ever sent — a
  window announced onto an arrangement that already existed named its screen,
  and then never again: moving a window to the other monitor, plugging one in,
  or switching workspaces left every taskbar in the session convinced the
  window was still where it started. The lists are now kept true wherever the
  space changes under them, and the update diffs against what was last said,
  so the shell laying every window out on every frame of an animation sends
  nothing at all.
- A `shell.overlay` carrying more than 4096 rectangles is refused at parse.
  The compositor already refused to *store* more than that; refusing there
  meant the message had still been parsed whole and the sender told nothing
  until after it had been built. The bound now sits at the same boundary as
  the workspace-list one, so a hostile list is a parse error against
  `shell.overlay` rather than a silence.
- A tearing refusal is forgotten the moment the thing it was measured under
  changes. Adaptive sync toggled off, or a mode set, were exactly the
  conditions a display had refused tearing in — the user's own answer to a
  display that tears badly — and the latch used to outlive both until the
  render pass happened to notice. The clear is now eager, and takes the
  failure count with it: three refusals under one mode say nothing about the
  next.
- A notification closing no longer shows the desktop background where it was.
  The compositor draws the notification strip over a window by redrawing that
  piece of the page cropped to the rectangle the page reported, and what is
  behind a notification inside that rectangle is the wallpaper rather than the
  window — so the exit's fade was a rectangle of background fading *in* over
  whatever the notification had been covering, and the shrink that went with
  it left the same background around the edges, because a transform moves what
  is painted and not the layout box the rectangle is measured from. Both
  animations are now the box itself collapsing and growing, so the shrinking
  rectangle and the shrinking picture are the same shrinking thing.
- Dark mode is announced in both settings namespaces rather than one. The
  toggle emitted `SettingChanged` for `org.freedesktop.appearance` only, so
  everything reading that namespace followed and everything reading
  `org.gnome.desktop.interface` — GTK3, which goes by the theme *name* rather
  than the scheme — kept whatever it had read at startup. Half a desktop
  switching looks exactly like a portal that does not work. The values are now
  taken from the same functions `ReadAll` answers from, so what is announced
  is what a client that re-reads on the signal is handed; announcing one value
  and answering with another is worse than announcing nothing.

### Added
- Rich window rules can match the active opening workspace (1 through 9).
  Rules and `window.pseudotile.toggle` can pseudotile a client at capped,
  centred preferred or natural dimensions while its tree slot stays full;
  dimensions survive sessions and layout changes. Rules can also safely
  swallow descendants: native Wayland process ancestry is verified from kernel
  credentials and procfs start times, only proven ancestor view IDs cross IPC,
  child rules can opt out, and every move/float/special/fullscreen/close path
  dissolves or restores the exact leaf without timing or focus guesses.
- Window rules now support `contains`, `equals`, and regular-expression
  matching across `app_id`, title, and `xdg-toplevel-tag`, plus scratchpad and
  output-pinned floating windows. `scratchpad.toggle`, `scratchpad.move`, and
  `window.pin.toggle` expose the policy to bindings.
- Per-device libinput settings can match the logged `vendor:product:name`, with
  wildcard defaults and live reload. Discrete swipe and pinch gestures can run
  any existing binding action while unmatched gestures still reach clients.
- Explicit local `layout_extensions` register through a stable shell API and
  load before mapped-window replay. The shipped monocle example demonstrates
  the contract; failed or invalid extensions fall back to tiling.
- Display settings can derive a conservative recommended scale from valid EDID
  dimensions and arrange all connected outputs horizontally or vertically.
- Physical input hotplug events now reach tablet, touchscreen, and device
  configuration lifecycle handling instead of being discarded by the udev
  callback.
- Dependency policy is checked by `cargo-deny` in CI and the commit hook:
  advisories, accepted licenses, registries and the two deliberate Git sources.
  Workflow actions are pinned to full commits, with Dependabot responsible for
  updating those pins.
- A power menu. Until now, leaving meant `Mod4+Shift+e` and a TTY, and a desk
  with no keyboard — a touch screen, a kiosk — had neither. The battery
  widget's picker, which already lists the power profiles, ends with the four
  rows that are on whether or not a daemon offered a profile: suspend, power
  off, reboot and quit. The first three go to logind through the UPower
  worker — `Suspend` is the call the lid policy makes on a hinge close, and
  `Reboot` and `PowerOff` its siblings — and they are answered whether or not
  a battery widget is on the bar, because a lid set to off and no daemon is
  still a machine that can be told to lie down, to go away, or to come back.
  Quit is not a fourth verb handed to logind: this compositor is the session,
  so it goes out as its own `quit`, and two ways to leave does not mean two
  different things. Opened by the battery widget or the `power` shell
  command. One new message, `power.action`.
- A launcher of its own. `Mod4+d` used to be `exec wmenu-run` — a
  layer-shell client with its own theme, its own configuration file and no
  idea what the layout is: it could not open on the monitor that asked for it,
  or know which workspace what it launches would land on. The shell now draws
  the picker, on the same terms as the clipboard and the two radios, and the
  compositor feeds it: the `.desktop` scan is the compositor's, because the
  page cannot read `XDG_DATA_DIRS` any more than it can read `/proc`, and the
  filter is answered by a re-scan rather than applied to a cached list, so a
  package that installs a new entry shows the moment the field is next typed
  in. What a row starts is the entry's `Exec`, field codes dropped the way the
  Desktop Entry specification says a launcher with no files and no URLs must
  drop them, and `Terminal=true` entries run in the configured terminal. The
  process is handed an xdg-activation token minted for it, so the window that
  appears opens focused rather than behind whatever the user moved on to —
  the launcher knows where the window is going, because it is the thing that
  asked for it, and the token is how it says so. Icons are looked up in the
  installed themes the same walk the tray uses and sent as `data:` URLs, with
  a letter where there is none or the file is too large to send. Two new
  messages, `launcher.query` and `launcher.launch`, and a `launcher.list` in
  answer; `menu` in the config file names an external menu — `wmenu-run`,
  `fuzzel`, whatever — and `Mod4+d` runs it when one is named.
- A notification centre. A notification was a popup and then it was nothing:
  one that arrived over a fullscreen window, or while the screens were
  blanked, was never seen and could not be gone back to — which is what every
  desktop's notification centre is for, and what a second daemon is usually
  installed to keep, while the only copy that ever existed was already in this
  process on its way to the shell. The compositor now keeps the last 50
  (`notifications.history`; `0` keeps none) and the shell draws them at
  `Mod4+Shift+m`, with the application's own action buttons still on each row.
  Popups that expired or were dismissed stay in the list; ones that were acted
  on, or withdrawn by the application that sent them, leave it. The record is
  the compositor's rather than the page's because the shell restarts when it
  crashes and reloads when its stylesheet changes, and a history kept there
  would be lost by both. Two new messages, `notification.list` and
  `notification.forget`, and `notification.add` now carries the time it
  arrived.
- Global shortcuts: `org.freedesktop.impl.portal.GlobalShortcuts`, which is
  how a chat program gets push-to-talk and a recorder gets a start key that
  works while somebody is typing in another window. X11 gave those out as
  server-side key grabs and Wayland does not, deliberately — a client that can
  grab one chord can grab every chord — so what replaces the grab is a
  question, and the compositor is the only thing in the session that can
  answer it: the chord has to be resolved before the focused client is offered
  the key. The dialogue is the one the screen-share and remote-control
  requests already use, with a third sentence at the top and the chords listed
  as the config file spells them, because the person answering is comparing
  them against the keyboard in front of them. The desk's own keymap wins: a
  shortcut is matched only after the built-in chords and everything in `binds`
  have declined the key, so an application asking for `Mod4+Return` gets a
  grant that never fires rather than a terminal that stops opening. A trigger
  this keymap cannot read is refused before anybody is asked, since agreeing
  to a chord that can never fire is agreeing to nothing while telling the
  application it has something. A grant is remembered — by application and by
  chord, in `~/.local/state/viewport/shortcuts.json` — which is the opposite
  of what this compositor does with a remote-desktop grant and for a reason
  worth stating: that one is a process that could type anything on the
  strength of a file, this one is a single chord reaching a single application
  while it runs, and asking again at every login is how somebody learns to
  agree to dialogues without reading them. Both halves of a press are
  announced, because a push-to-talk key holds a microphone open and nothing
  else would ever say it came back up.
- Modifier state sent back to a libei client, over `ei_keyboard.modifiers`.
  A remote client composes a capital the way a keyboard does — press Shift,
  press the letter, release both — against its own idea of the seat's state,
  and that state has two sources the client cannot see: somebody at the
  machine pressing Shift, and a key the client itself sent latching Caps Lock.
  Nothing told it about either, so a session drifted and typed capitals nobody
  asked for until something unrelated resettled it. Sent on a change rather
  than per event, since typing at the desk moves the modifier state twice per
  shifted character and none of that concerns a remote client that has not
  drifted; a client is caught up outright the moment it binds a keyboard,
  because until then `ei_keyboard.modifiers` has no device to go to and a
  session that started under a locked Caps Lock would otherwise hear about it
  only when somebody pressed Caps Lock again. The four numbers go in libei's
  order, which is not `wl_keyboard`'s — depressed, locked, latched, group,
  with the middle pair swapped — and there is a test for that alone, because
  getting it wrong is silent and reads as a held Shift that never comes up.
- The two D-Bus interfaces that keep a screen awake: `org.freedesktop.
  ScreenSaver`, which is what Firefox and mpv reach for, and
  `org.freedesktop.impl.portal.Inhibit`, which is where a sandboxed
  application's request arrives from the portal frontend. Wayland's
  `idle-inhibit-v1` was already honoured and is not the interface anything
  actually uses — it postdates the bus one, and every toolkit already had code
  for that — so a film on this desktop was watched with the screen blanking
  under it, and the fix a user finds is to turn the idle policy off for
  everything. Both end in one registry that the idle timer reads where it
  already reads the Wayland inhibitors, so a hold of either kind holds off the
  same two deadlines. The screensaver interface is served at
  `/org/freedesktop/ScreenSaver` and at `/ScreenSaver`, because software asks
  at both. A hold is released when the connection that took it goes, not only
  when the program remembers to give it back: a player killed mid-film never
  calls `UnInhibit`, and waiting for one would keep the screens lit for the
  rest of the session with nothing on screen to say why. A cookie may only be
  released by the connection that took it, since the session bus is reachable
  by every process in the session and one program guessing another's cookie
  would turn the screen off in the middle of somebody's film. The portal
  interface is version 1 deliberately — version 2 adds the logout dialog and
  the wait for its answer, and there is no logout here to be about — and
  `GetActive` answers false rather than erroring, because a client that gets an
  error there sometimes concludes the whole interface is missing and stops
  inhibiting with it.
- An EI server, so `org.freedesktop.impl.portal.RemoteDesktop` answers
  ConnectToEIS and the interface advertises version 2. The Notify calls it
  sits beside are one D-Bus round trip through the portal frontend per input
  event — a remote pointer moves hundreds of times a second — and libei
  replaces the whole of that with a socket the application speaks directly.
  The bus thread makes a `socketpair`, answers the call with one half and
  sends the other to the compositor, because an EI context is read by a
  calloop source and the D-Bus reply is synchronous; neither thread can do
  the other's job. Events off the socket go through `process_input_event`,
  the same path libinput's take, because smithay's `backend_libei` presents
  an EI client as an `InputBackend` — so a remote chord is filtered by the
  table a typed one is filtered by and counts as the same activity against
  the idle timer. Only the granted devices are created, so a session allowed
  a mouse and not a keyboard has nothing to type with rather than something
  to be refused per event; the client is sent the keymap the seat is actually
  using, and an absolute pointer or touchscreen is told where the monitors
  are in the layout's own coordinates, refreshed when they move. Closing the
  session closes the socket — as does the portal frontend disappearing, which
  is the case nothing else would catch — and whatever the client was holding
  down when it went is released, because a process killed mid-drag would
  otherwise leave a button or a modifier latched in the one real seat.
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
- An `"osk"` config key for the on-screen keyboard above: `"auto"` (the
  default), `"manual"` or `"off"`. A boolean was not enough, because "stop it
  popping up while I have a real keyboard" and "I never want to see it" are
  different requests — the first still wants `Mod4+Shift+k` to reach it, and
  only the second wants the chord dead too, which `"off"` makes it, on
  purpose. `"auto"` also changed its own default behaviour: it now raises the
  keyboard only once the seat has actually seen a touch device, rather than
  for any client that enables a text-input regardless of hardware, so a desk
  with just a keyboard and a mouse no longer gets a keyboard fighting its own
  keyboard for a login prompt. Applies immediately on a config reload,
  including hiding a keyboard that was pinned open by hand before `"off"` was
  set.
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
  ConnectToEIS and the version the interface advertises are the entry above
  this one, which is the other half of the same portal.
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
- A shell that has given up is believed. `viewport-shell-gtk` counts its tries
  — three web-process crashes ask to be started degraded, five slow reloads
  are spent there — and when those have died too it exits with status 88
  rather than reload for ever. The compositor used to answer every exit with
  another turn of the restart treadmill, so 88 bought exactly what the cap
  exists to end: the same page rebuilt on the same GPU, at whatever pace. The
  code means what the shell meant by it now. Exit 88 is logged loudly and that
  slot stays down; the rest of the session goes on around it, and the page
  comes back only through something human — a monitor arriving while another
  page is still running starts every planned page afresh — or a session
  restart. Kept apart from exit 87, which asks for one specific second chance
  and gets it. See `docs/shell-backends.md`.
- A lock screen the shell draws, and PAM behind it. Locking used to mean one
  thing — run `idle.lock_command`, which is `swaylock` unless the config says
  otherwise — and the compositor's whole part in it was being a correct
  `ext-session-lock` server for whatever that program turned out to be. Every
  other modal surface on this desk is drawn by the page: the launcher, the
  power menu, the notification centre, the on-screen keyboard. The power menu
  was moved here on the argument that a desk with no keyboard — a touch
  screen, a kiosk — could not otherwise leave; that same desk could not get
  back *in*, because a locker in another process cannot reach
  `data/shell/osk.js` and swaylock has no keyboard of its own. So the surface
  with the strongest case for being drawn here was the one surface that was
  not. Now, with no `idle.lock_command` set, locking draws a clock on every
  monitor, a password box on the one being looked at, and a button that raises
  the on-screen keyboard. The password goes to PAM on a thread of its own,
  because `pam_authenticate` reads a file, hashes with a deliberately slow KDF
  and sleeps up to two seconds inside `pam_fail_delay` on a wrong one — and
  pam_sss or pam_krb5 talk to the network — every second of which on the event
  loop is the whole desk frozen while somebody types at the lock screen.
  libpam is opened with `dlopen` rather than linked, so a build on a machine
  without PAM headers still runs and a machine without libpam at all gets a
  lock screen that refuses every password rather than one that accepts any.
  `idle.lock_command` is unchanged and is still the escape hatch: somebody who
  configured swaylock keeps getting swaylock, and setting the key back is the
  way out of the built-in screen. The `lock` binding, `idle.lock_after`, the
  lid action and the power menu's new Lock row all go through the one
  `lock_session`, as that function was always documented to guarantee.
- A lock screen that fails closed, which is the part that had to be got right
  before any of the above was worth having. The page drawing the lock screen
  is the same page that draws the desktop, out of the same buffer, so "draw
  the shell" and "show the user's email client" are one instruction apart —
  and a lock screen that fails open is worse than no lock screen. The
  compositor therefore draws no part of the shell's buffer on a locked screen
  until the page has said, naming *this* lock, that it painted a lock screen,
  *and* a frame has landed after it said so. The message alone is not enough,
  because a page can send one from a handler and then never paint again; a
  live process is not the test either, because a page that is running and
  stuck is exactly the case. The page sends it from a double
  `requestAnimationFrame`, which runs after the frame the lock screen was
  rendered into was submitted, so every buffer arriving after the message has
  the lock screen in it. Both facts are dropped on anything that could
  invalidate them — a new lock, the shell process dying, its toplevel going
  away, a reload — and the session stays locked through every one of them. A
  shell that crashes, hangs or is reloaded while it holds the lock is a black
  screen, which is an unhelpful failure; the desktop reappearing would be a
  way past the lock, and that is the one this refuses to have. An output the
  page's rectangle does not cover is black for the same reason.
  `tests/lock-builtin.test.sh` locks a session that has no shell able to draw
  and captures the screen: black, still black after a message claiming the
  lock screen was drawn, and still black after a password on the control
  socket — and the second half of it checks that `idle.lock_command` still
  runs the program it names.
- A settings panel, on `Mod4+Shift+comma`. `docs/configuration.md` has opened
  by justifying two tiers of configuration with "a settings UI cannot run on a
  display that is not working" since the file was written, and named a settings
  panel twice more as the thing the runtime setters were for — while the
  runtime tier stayed three keys deep against a config file with dozens, so the
  argument for the design and the extent of the design drifted apart. The panel
  now exists and the setters it needed were written as it needed them: dark
  mode, the wallpaper and its fitting, the gaps, the window border, and each
  monitor's mode, scale and rotation. Every control sends the runtime setter
  for what it changes, so the desktop is already that shape while you are
  looking at it, and the panel draws nothing it has not been told — a value it
  has just sent is not shown until the compositor has echoed it back, so a
  refusal cannot be mistaken for a change.
- `config.dark_mode` sets the colour scheme applications are told to draw
  themselves in, and the `config` event now carries `dark_mode` so the shell
  can read it back. `appearance toggle` on a chord was the only way in, which
  left a panel able to move the setting and unable to show where it was — and a
  switch drawn from a guess shows the wrong state until it is pressed twice.
  Absent still toggles, which is what a key wants; a named state is what a
  switch wants.
- `config.save` writes the runtime settings into `settings.json` beside the
  config file, and that file is applied over the config file at every start and
  every reload. A settings panel whose every change is lost at the next restart
  is not a settings panel — but the config file is hand-written JSONC and no
  round trip through a JSON parser keeps its comments or its formatting, so
  nothing writes back into it. Saving is one explicit request rather than
  something each setter does, because the same setters are what a wallpaper
  cycler uses and a cycler that wrote the disk would rewrite the file
  twenty-four times a day. Delete `settings.json`, or a line out of it, to put
  the config file back in charge; the command line still beats both. Answered
  with `config.saved` naming the file, and `outputs.enabled` is now a config
  key so that a screen deliberately left off can be written down at all.
- A screen magnifier. `Mod4+Alt+equal` blows up the part of the screen under
  the pointer, `Mod4+Alt+minus` comes back down and `Mod4+Alt+0` goes straight
  to 1:1; the region follows the pointer and is held to the monitor it is on,
  so moving toward an edge slides it along rather than showing a strip of
  whatever is laid out beside that screen. This is not `canvas.zoom`, which is
  a layout the shell draws at a scale and whose own comment records that input
  only lands where it is aimed at 1.0 — the opposite of what magnifying a
  screen is for. Here the pointer is not magnified and the picture is: the
  cursor stays at the real place it was and is drawn through the same transform
  as everything else, so clicks, drags, resize edges and focus-follows-mouse
  all land on the thing under the drawn cursor without any of them being told
  the magnifier exists. The one exception is input that names a place on the
  glass rather than a movement — a touchscreen, a tablet in absolute mode —
  which is mapped back through the magnification before anything sees it.
  `magnify.step` and `magnify.max` configure it; both are clamped rather than
  refused, because a maximum below 1.0 is a magnifier that cannot magnify and
  the useful response to a configuration mistake is a working compositor.

- Every surface the shell can put on screen can now be finished with a
  keyboard. The launcher and the network picker had `keydown` handlers because
  each has a text field; the tray menu, the clipboard history, the notification
  centre and the power menu had none, so each was opened by a chord and then
  handed to the mouse — the same gap as a power menu a touch screen could not
  open, which is the gap that got it built. Arrows choose, Enter acts, Delete
  forgets a row where a list has a notion of forgetting one, Escape dismisses,
  and a ring that no picker's own styling can paint over says where the
  keyboard is. The half that was not obvious is that a `keydown` handler on
  those surfaces would have received nothing: the shell is a Wayland client,
  the window under it has the keyboard, and only the two surfaces with text
  fields ever asked for it. Every surface now asks on the way up and hands it
  back on the way down.

- Roles and names on the shell's markup, so that the accessibility tree the
  engine builds out of the desktop is worth reading: `dialog`, `listbox` and
  `menu` on the surfaces, `aria-selected` following the keyboard, `aria-live`
  on the clock and the notification strip, and an explicit label wherever a row
  is drawn out of private-use glyphs — a Wi-Fi row's strength is four bars in
  an icon font, and read aloud that is a nonsense syllable in front of the
  useful part.
- A calendar under the clock, and a clock that is not one American's. The bar's
  clock passed the literal `'en-US'` to `toLocaleDateString` and assembled the
  time by hand out of `getHours()`, so every desk in the world read an American
  date and a twenty-four-hour clock whether or not that is how it writes one,
  and there was no key to ask for either. A `clock` block now carries three
  things to the page: `locale`, a BCP 47 tag; `hour12`, which absent leaves to
  the locale; and `format`, a strftime-style template for the whole module —
  a string rather than a further pile of booleans, because what people change
  is the *arrangement*, and every flag that could be added would be a worse
  spelling of one line of `date(1)`. Absent is not `en-US` and is not any other
  tag written into the shell: it is the locale the engine is running under,
  which is what `LANG` already said and the one answer the compositor cannot
  give on the page's behalf. One session-visible consequence of that: a desk
  running in `en_US` now gets a twelve-hour clock where it used to get a
  twenty-four-hour one, because that is what the locale writes;
  `"clock": { "hour12": false }` asks for the old one back without giving up
  the names. Clicking the clock now opens a month grid under
  it — on the monitor whose clock was clicked, anchored like the tray menu
  rather than centred like the pickers, dismissed by the same document click
  that dismisses them, and taken down with the bar it hangs off when `bar:
  auto` hides it. The grid inherits the same locale for its month name, its
  weekday headings and the day its week starts on, which is not a property of
  the language: `en-US` starts its week on Sunday and `en-GB` on Monday, so
  nothing short of asking about the region can be right. It draws six rows
  whatever the month needs, because a panel that changed height as somebody
  paged through the year would move the rectangle the compositor is drawing it
  in. None of it needed a compositor change beyond carrying the three config
  fields: a month is arithmetic a page can do. Every call into `Intl` is
  guarded and falls back to English names, because the shell is drawn by
  whichever engine the backend names and a clock that went blank looking up a
  month name would be worse than one in the wrong language.
- `xwayland.scale` in the config file: `"off"` (the default, and what every
  release before this one did), `"auto"`, or a whole number. X11 clients have
  always drawn a buffer of logical pixels that the compositor then magnifies
  onto a HiDPI panel — the right size and blurry — because per-output `scale`
  is honoured for Wayland clients and nothing at all reached Xwayland. The key
  scales the Xwayland connection, so the X screen has that many times the
  pixels and everything coming back is divided by the same number, and
  publishes the XSETTINGS that tell the toolkits to spend them:
  `Gdk/WindowScalingFactor`, `Gdk/UnscaledDPI` and `Xft/DPI`. Both halves or
  neither — half of it is every X11 window at twice or a quarter of the size
  it asked for.

  It reaches GTK 3, GTK 4, Qt 6 and Chromium. It does not reach Qt 5 without
  `QT_AUTO_SCREEN_SCALE_FACTOR`, and it cannot reach xterm, SDL, GLFW or
  Java/AWT at all: those come out sharp and half the size, which is why the
  default is off rather than `"auto"`. `docs/protocols.md` records that trade,
  the mixed-DPI case that has no right answer, and why `GDK_SCALE` in the
  child environment was refused — it reaches only programs the compositor
  spawned, and nothing at spawn time knows whether the program will turn out
  to be an X11 client or a Wayland one.
- `gpu` chooses which graphics card renders and which one clients are told
  about — `--gpu`, `$VIEWPORT_GPU` or the config file, flag over variable over
  file, matched as a substring of a device path so `card1`, `renderD129` and a
  whole `/dev/dri/by-path/...` all work. Only the environment variable existed
  before, which is a poor place to keep a standing preference. Every card is
  opened either way; this names the primary, and on a hybrid laptop that is the
  choice between battery and frames, which is not something the hardware
  answers.
- `cross_gpu` says what clients are told they may allocate when there is more
  than one card. `native`, the default, advertises everything the primary's
  renderer can import, which keeps the tiled and compressed modifiers a window
  that never leaves its card should have. `portable` advertises only what every
  card can import, for the desk where windows are dragged between two cards'
  monitors all day and for the client that ignores the per-surface feedback that
  would have told it to reallocate — at the cost, paid by every client, of the
  modifiers only one card understands. Nothing at all on a machine with one
  card.

### Changed
- Modifier state is computed only when a page can read it. `shell_modifiers`
  ran on every key, button and axis event whatever the build, and in one
  without the web engine the answer went nowhere; a build without `wpe` now
  skips the work outright rather than computing what nothing consumes.
- `Event::Config` travels boxed. It is far the largest thing the event enum
  carries — a keymap, the window rules, a theme, the whole right side of the
  bar, and now the clock — and it is sent twice a session, while an enum is as
  big as its widest variant: inline, every `view.geometry` in a resize gesture
  was carrying that much dead space around the event loop.

### Fixed
- The Wi-Fi, Bluetooth, power-profile and clipboard pickers no longer open
  between the two monitors on a two-monitor desk. The shell is one page
  spanning the whole output layout — two 2560x1440 monitors are one 5120x1440
  canvas to it — and all four were centred with `top: 50%; left: 50%`, which
  is the middle of that canvas rather than the middle of either monitor; on
  the usual two-monitor desk that middle is the seam between them. Each is now
  docked over the active output's own rect, the same way the on-screen
  keyboard and the screen-share chooser already were, with the visible dialog
  centred inside that per-output box instead of inside the page. Their
  `max-width`/`max-height` moved from `vw`/`vh` to percentages for the same
  reason: a canvas-wide viewport unit let a picker grow wider than the very
  monitor it was centred over.
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
- Reloading the config no longer writes to the process environment. The cursor
  loader could only be built from `XCURSOR_THEME` and `XCURSOR_SIZE`, so every
  reload that touched the cursor block wrote the pair into environ with
  `set_var` for the constructor to read straight back — a setenv on a live
  multithreaded process, undefined against every thread that might be mid-
  `getenv`. The theme can be asked for by name now, the environment is read
  once at start-up as before, and nothing writes it again; children started by
  the compositor were already told the pair explicitly rather than by
  inheritance.
- A screen share is no longer answered before it exists. PipeWire names a
  stream on its own clock — usually within a couple of milliseconds of being
  asked — and the portal reply carried that name, so a reply sent before the
  daemon had answered went out with the placeholder node number,
  `u32::MAX`, and an application that connected to node 4294967295 connected
  to nothing. The reply now waits for the real name and leaves the moment it
  arrives; a 500 ms deadline refuses the share if it never comes, tearing the
  half-made stream back out of the compositor rather than leaving a stream
  nobody can reach. A remote-desktop session that drives without watching is
  still answered straight away, since its grant is the whole answer.
- An output change is put back if nobody says they can see it. `docs/ipc.md`
  has promised since it was written that a configuration reverts after twelve
  seconds unless `output.confirm` arrives, and `output.confirm` was a handler
  with an empty body and a comment saying nothing armed a revert — so anything
  that read the documentation and skipped the confirmation kept a mode that had
  blanked the screen, which is the exact failure the sentence was there to rule
  out. A mode, scale, rotation or power change now snapshots the monitors and
  starts the clock, on a timerfd because the desktop it has to fire on is one
  where nothing is happening; the restore goes through the same path
  wlr-output-management applies a configuration, so it revalidates and runs the
  same tail rather than being an undo written twice. Two changes inside the
  window are one change, and go back to the state from before the first. The
  new `output.revert` is that undo asked for early, for the screen that came
  back and can be seen to be wrong. The config file's own `outputs` block is
  exempt, since nobody is at a confirmation dialog during startup.
- `docs/ipc.md` no longer claims `output.configure` runs `wlr_output_test_state`
  before committing. It never has — that is the output-management path — and
  what it actually does (refuse a non-positive scale, refuse the last screen
  off, prefer an advertised modeline) is now what the page says.
- A Viewport session now has an accessibility bus. The desktop is a web page,
  so the engine has already built a real accessibility tree out of it, and with
  the `webkitgtk` backend the whole of that tree is one bus away from Orca —
  the view is the child of a presented `GtkApplicationWindow` inside a live GTK
  main loop, both GTK4 and WebKitGTK speak AT-SPI themselves rather than
  through a bridge, and the shell process inherits the session bus untouched.
  What was missing was the bus: `org.a11y.Bus` is D-Bus activated and a
  compositor started from a TTY has nothing behind it that would have installed
  it, so it was not stopped but unactivatable, and nothing anywhere said so.
  The NixOS module enables it by default, which costs nothing until a screen
  reader asks. `VIEWPORT_CEF_ARGS` is added alongside the `chromium` backend's
  existing pass-through, which is what makes the second Blink backend reachable
  at all. `docs/shell-backends.md` records the verdict for each of the six
  backends and what would have to change for the three that cannot: `wpe` has
  no toolkit in its path and therefore no accessible root to embed the web
  process's tree under, and both Servo backends build AccessKit trees that no
  adapter turns into AT-SPI. The last of those is the default backend, so a
  desk that needs a screen reader wants `shellBackend = "webkitgtk"` until
  Servo grows one.
- A client's buffer is judged by every graphics card on the seat rather than by
  the primary alone. Each card is told which of its screens a window is on and
  each window is told which card to allocate against, and a client that does
  what its per-surface feedback asked — allocating for the second card, because
  that is the card showing it — handed back a buffer that was then offered to
  the *first* card's renderer for approval. A modifier the discrete card
  understands and the display controller does not is the ordinary case, so the
  import failed, and `linux-dmabuf` has one answer to a failed import: a
  protocol error. The client was disconnected for having followed the
  compositor's own instruction. Every online card is asked now and one "yes" is
  enough; a buffer some cards read and others refuse is accepted, and the window
  being missing from the screens that refused it is said once per format,
  modifier and card rather than being a hole in a monitor with nothing in the
  log.
- Two graphics cards no longer give two screens the same name. Connector names
  are handed out per card, so a laptop with an integrated display controller and
  a discrete card beside it has two `DP-1`s, and everything that identifies a
  screen by name takes the name as unique: the config file's per-output rules,
  the active output, the saved layout, the `wl_output` a client binds, and the
  per-output vblank bookkeeping whose entire purpose is that one screen's flip
  must not answer for another's. The first card to claim a name keeps it, so no
  screen on a machine with one card is renamed and no config file stops
  matching, and a collision gets the card index appended and says so.
- `wp-drm-lease-v1` leases a connector from the card that has it. There was one
  lease global for the session, made on the primary card, and a lease request
  was answered out of the primary's DRM device whichever card it named — so a
  headset wired to the discrete card was advertised under the wrong card's node,
  and a client that took the lease would open the wrong card and be handed a
  CRTC number that on that card is very likely a real CRTC, possibly one
  scanning out the desktop. CRTC handles are small integers handed out per
  device, so that collision is the normal case and not the unlucky one. The
  state is per card now, both handlers look the card up by the node the request
  arrived on, and the free-CRTC search is scoped to that card the way the
  connector scan's already was.
- A window that opened before the shell did is focused. Focusing a window is
  the shell's decision, so a client that maps before the shell is running is
  left unfocused for the moment — and the moment used to be for ever. The
  shell would start, find the seat idle and take the keyboard for itself
  under the rule that an empty desktop should still be typable; then the
  window list it asked for would arrive with the window marked as a replay,
  which is a thing a shell restores into a slot and deliberately does not
  steal focus for. Three rules that are each right, and a window between them
  that nothing would ever focus. The floor under an empty desktop now asks
  whether the desktop is actually empty, and focuses the newest mapped window
  if it is not. Worst for X11, which is where it was found: an X client's
  focus is `SetInputFocus`, which is only sent when an `X11Surface` is the
  seat's focus, so an autostarted X11 application sat at `PointerRoot` with
  its keystrokes going to whatever the pointer happened to be over.
- The launcher keeps the caret in its filter field. Every surface the shell
  draws puts the keyboard on the row under the highlight, which is what a
  screen reader is read from — but the launcher is a list *and* a text field,
  and the field is where the typing goes. Its list is rebuilt on the answer to
  every keystroke, and every rebuild moved the caret onto a row, so the field
  lost it a moment after the picker opened and the characters after the first
  went nowhere. The row under the keyboard is now pointed at with
  `aria-activedescendant` rather than focused, which is how a combobox says
  which option is current without moving the caret — and is what the field's
  own `role="combobox"` already claimed. A reader gets the same row read to it
  and the field keeps the keys. The passphrase box in the Wi-Fi picker is the
  other surface with a field and is fixed with it, but only while it is
  asking: that picker's rows hold the keyboard the rest of the time.

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
