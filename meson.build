project(
  'viewport',
  'c',
  version: '0.1.0',
  license: 'MIT',
  meson_version: '>=1.1',
  default_options: [
    'c_std=c11',
    'warning_level=2',
    'werror=false',
    'b_ndebug=if-release',
  ],
)

cc = meson.get_compiler('c')

add_project_arguments(
  cc.get_supported_arguments([
    '-DWLR_USE_UNSTABLE',
    '-D_POSIX_C_SOURCE=200809L',
    '-Wno-unused-parameter',
    '-Wno-missing-field-initializers',
    '-Wundef',
  ]),
  language: 'c',
)

# ---------------------------------------------------------------------------
# Dependencies
#
# WPE WebKit is built with -DENABLE_WPE_PLATFORM=ON, which pins
# WPE_API_VERSION to 2.0. That gives us `wpe-platform-2.0` (WPEDisplay,
# WPEView, WPEBufferDMABuf) and `wpe-webkit-2.0` (WebKitWebView). libwpe and
# wpebackend-fdo are deliberately absent — WPEPlatform replaces both.
# ---------------------------------------------------------------------------

wlroots = dependency('wlroots-0.20', version: ['>=0.20.0', '<0.21.0'])
wayland_server = dependency('wayland-server', version: '>=1.24')

# wlroots 0.20 public headers include <wayland-protocols/xdg-shell-enum.h> and
# friends, which wayland-protocols installs but does not advertise: its .pc
# file carries no Cflags. Without this every wlr_xdg_shell.h include fails.
wayland_protocols = dependency('wayland-protocols', version: '>=1.41')

# wlr-layer-shell is not part of wayland-protocols, and wlroots installs the
# header that needs it (wlr_layer_shell_v1.h) without installing the generated
# protocol header itself — so wayland-scanner has to run here or that header
# will not compile at all.
#
# The XML is vendored under protocols/ rather than taken from the wlr-protocols
# package: that package ships its .pc in share/pkgconfig, which is not on
# PKG_CONFIG_PATH in every environment, and the file is a small, stable,
# MIT-licensed protocol definition. Vendoring removes the dependency.
wayland_scanner = find_program('wayland-scanner')

layer_shell_protocol_h = custom_target(
  'wlr-layer-shell-unstable-v1-protocol.h',
  input: 'protocols/wlr-layer-shell-unstable-v1.xml',
  output: 'wlr-layer-shell-unstable-v1-protocol.h',
  command: [wayland_scanner, 'server-header', '@INPUT@', '@OUTPUT@'],
)

# Colour management comes from wayland-protocols staging rather than being
# vendored — it is shipped there, unlike wlr-layer-shell — but wlroots still
# does not install the generated header its own header needs.
color_management_protocol_h = custom_target(
  'color-management-v1-protocol.h',
  input: wayland_protocols.get_variable(pkgconfig: 'pkgdatadir')
    / 'staging/color-management/color-management-v1.xml',
  output: 'color-management-v1-protocol.h',
  command: [wayland_scanner, 'server-header', '@INPUT@', '@OUTPUT@'],
)

# Same story for output power management, whose wlroots header needs the
# generated one to compile.
output_power_protocol_h = custom_target(
  'wlr-output-power-management-unstable-v1-protocol.h',
  input: 'protocols/wlr-output-power-management-unstable-v1.xml',
  output: 'wlr-output-power-management-unstable-v1-protocol.h',
  command: [wayland_scanner, 'server-header', '@INPUT@', '@OUTPUT@'],
)
add_project_arguments(
  ['-I' + wayland_protocols.get_variable(pkgconfig: 'includedir')],
  language: 'c',
)
xkbcommon = dependency('xkbcommon', version: '>=1.8')
pixman = dependency('pixman-1')
libdrm = dependency('libdrm')
libinput = dependency('libinput')

# wlroots' xwayland.h includes xcb/xcb_ewmh.h directly, so the header has to be
# on the include path even though we never call xcb ourselves.
xcb_ewmh = dependency('xcb-ewmh')

glib = dependency('glib-2.0', version: '>=2.70')
gobject = dependency('gobject-2.0')
gio = dependency('gio-2.0')
json_glib = dependency('json-glib-1.0')

wpe_platform = dependency('wpe-platform-2.0', version: '>=2.48')
wpe_webkit = dependency('wpe-webkit-2.0', version: '>=2.48')

math = cc.find_library('m', required: false)

viewport_deps = [
  wlroots,
  wayland_server,
  xkbcommon,
  pixman,
  libdrm,
  libinput,
  xcb_ewmh,
  glib,
  gobject,
  gio,
  json_glib,
  wpe_platform,
  wpe_webkit,
  math,
]

# ---------------------------------------------------------------------------
# Paths baked into the binary
# ---------------------------------------------------------------------------

pkgdatadir = get_option('prefix') / get_option('datadir') / meson.project_name()

add_project_arguments(
  ['-DVIEWPORT_PKGDATADIR="' + pkgdatadir + '"'],
  language: 'c',
)

# ---------------------------------------------------------------------------
# Targets
# ---------------------------------------------------------------------------

# main.c is deliberately not in here. Everything else is linked twice — once
# into the compositor, once into the fuzz harness — and libFuzzer supplies the
# main() of its own binary, so a list that carried ours could not be reused.
viewport_sources = files(
  'src/binding.c',
  'src/config.c',
  'src/status.c',
  'src/appearance.c',
  'src/server.c',
  'src/output.c',
  'src/xdg_shell.c',
  'src/view.c',
  'src/xwayland.c',
  'src/layer_shell.c',
  'src/input.c',
  'src/pointer.c',
  'src/session_lock.c',
  'src/output_management.c',
  'src/foreign.c',
  'src/text_input.c',
  'src/watchdog.c',
  'src/session.c',
  'src/idle.c',
  'src/hdr.c',
  'src/tablet.c',
  'src/notification.c',
  'src/gesture.c',
  'src/ipc.c',
  'src/json_util.c',
  'src/glib_loop.c',
  'src/wpe_display.c',
  'src/wpe_view.c',
  'src/web.c',
  'src/web_buffer.c',
)

viewport_sources += layer_shell_protocol_h
viewport_sources += output_power_protocol_h
viewport_sources += color_management_protocol_h

viewport_exe = executable(
  'viewport',
  viewport_sources + files('src/main.c'),
  include_directories: include_directories('include'),
  dependencies: viewport_deps,
  install: true,
)

# ---------------------------------------------------------------------------
# Unit tests
#
# The four compositor tests below each start a headless server and drive a
# Wayland client against it. That is the only honest way to judge presentation,
# and it is also why none of them can reach the type checks that every field of
# every IPC message passes through: those are pure functions, and a test that
# needs a seat and a renderer to call one cannot run on a hosted runner at all.
#
# This binary links src/json_util.c and json-glib and nothing else, so it needs
# no wlroots, no WebKit and no display, and finishes in milliseconds. What it
# proves is the part the other tests structurally cannot: that a malformed
# message is refused rather than misread.
# ---------------------------------------------------------------------------

unit_test = executable(
  'viewport-test-unit',
  files('tests/unit.c', 'src/json_util.c'),
  include_directories: include_directories('include'),
  dependencies: [glib, gobject, json_glib],
  install: false,
)

test('unit', unit_test, timeout: 30, protocol: 'exitcode')

# Which binding wins when a chord is defined twice.
#
# Not reachable from the compositor tests: they drive Wayland, and a binding
# that loses to a built-in default produces no protocol traffic at all. It is
# also the tier that has to keep working when the shell does not, so "the
# config file had no effect" is the expensive failure here.
#
# Links binding.c against stubs rather than the whole compositor — run_action()
# is the only thing that reaches outward, and no test runs an action.
binding_test = executable(
  'viewport-test-binding',
  files('tests/binding.c', 'src/binding.c', 'src/config.c', 'src/json_util.c'),
  include_directories: include_directories('include'),
  dependencies: viewport_deps,
  install: false,
)

test('binding', binding_test, timeout: 30, protocol: 'exitcode')

# ---------------------------------------------------------------------------
# Hostile IPC input
#
# IPC is the only input the compositor takes from something it cannot vouch
# for, and both of the crashes fixed in it so far were reachable from a single
# message. tests/fuzz_ipc.c drives that entry point from two front-ends over
# one stub, and both are built here for a reason.
#
# The replay build is an ordinary test. It needs no special compiler, so it
# runs everywhere — including a gcc-only CI runner, which is what this project
# has — and it replays a corpus containing both of the fixed crashes. Without
# it the harness would ship as source that nobody had ever executed, which is
# the same amount of confidence as no harness at all.
#
# The libFuzzer build is the one that finds new things, and it is off unless
# asked for: it needs clang's -fsanitize=fuzzer, which a normal build must
# never be made to require, and it is not registered as a test because a fuzzer
# has no natural end. A reproducer it emits is replayed by the binary above.
# ---------------------------------------------------------------------------

ipc_replay = executable(
  'viewport-test-ipc-replay',
  files('tests/fuzz_ipc.c') + viewport_sources,
  include_directories: include_directories('include'),
  dependencies: viewport_deps,
  install: false,
)

# XDG_STATE_HOME is redirected because session.save is one of the messages
# replayed, and it writes the saved layout to disk. Left alone, a corpus entry
# carrying a well-formed state string would overwrite the layout of whoever ran
# the test suite. Nothing reads it back — the point is only that the write
# lands somewhere disposable.
test(
  'ipc-replay',
  ipc_replay,
  env: {'XDG_STATE_HOME': meson.current_build_dir() / 'ipc-replay-state'},
  timeout: 30,
  protocol: 'exitcode',
)

if get_option('fuzz')
  if not cc.has_argument('-fsanitize=fuzzer-no-link')
    error('-Dfuzz=true needs a compiler with -fsanitize=fuzzer, i.e. clang')
  endif

  executable(
    'viewport-fuzz-ipc',
    files('tests/fuzz_ipc.c') + viewport_sources,
    include_directories: include_directories('include'),
    dependencies: viewport_deps,
    c_args: ['-DVIEWPORT_FUZZ', '-fsanitize=fuzzer-no-link'],
    link_args: ['-fsanitize=fuzzer'],
    install: false,
  )
endif

# ---------------------------------------------------------------------------
# Test clients
#
# Window capture can only be judged from outside the compositor, by a client
# that asks for a window and looks at what comes back. These two build that
# view: one window that is easy to recognise, and one capture that checks it.
# Neither is installed — they exist for tests/capture.test.sh.
# ---------------------------------------------------------------------------

wayland_client = dependency('wayland-client', version: '>=1.24')
protocol_dir = wayland_protocols.get_variable(pkgconfig: 'pkgdatadir')

# Unlike the server headers above, these need the marshalling code as well:
# nothing else in the build links a Wayland *client*.
test_protocols = {
  'xdg-shell': protocol_dir / 'stable/xdg-shell/xdg-shell.xml',
  'ext-foreign-toplevel-list-v1': protocol_dir
    / 'staging/ext-foreign-toplevel-list/ext-foreign-toplevel-list-v1.xml',
  'ext-image-capture-source-v1': protocol_dir
    / 'staging/ext-image-capture-source/ext-image-capture-source-v1.xml',
  'ext-image-copy-capture-v1': protocol_dir
    / 'staging/ext-image-copy-capture/ext-image-copy-capture-v1.xml',
  'ext-session-lock-v1': protocol_dir
    / 'staging/ext-session-lock/ext-session-lock-v1.xml',
  # Vendored, as for the server side: the frame probe has to stay visible on a
  # chosen output even while a game is fullscreen on another, and the overlay
  # layer is the only way to ask for that.
  'wlr-layer-shell-unstable-v1': 'protocols/wlr-layer-shell-unstable-v1.xml',
}

test_protocol_sources = []
foreach name, xml : test_protocols
  test_protocol_sources += custom_target(
    name + '-client-protocol.h',
    input: xml,
    output: name + '-client-protocol.h',
    command: [wayland_scanner, 'client-header', '@INPUT@', '@OUTPUT@'],
  )
  test_protocol_sources += custom_target(
    name + '-protocol.c',
    input: xml,
    output: name + '-protocol.c',
    command: [wayland_scanner, 'private-code', '@INPUT@', '@OUTPUT@'],
  )
endforeach

paint_client = executable(
  'viewport-test-paint-client',
  files('tests/paint-client.c') + test_protocol_sources,
  dependencies: [wayland_client],
  install: false,
)

capture_client = executable(
  'viewport-test-capture-client',
  files('tests/capture-client.c') + test_protocol_sources,
  dependencies: [wayland_client],
  install: false,
)

# Reports when the compositor stops presenting. The event loop can stay
# responsive while the screen does not update, and a frame callback is the only
# thing that measures the difference from outside.
frame_client = executable(
  'viewport-test-frame-client',
  files('tests/frame-client.c') + test_protocol_sources,
  dependencies: [wayland_client],
  install: false,
)

# A locker that dies without unlocking. What the screen looks like afterwards is
# the one part of ext-session-lock the compositor cannot check about itself.
lock_client = executable(
  'viewport-test-lock-client',
  files('tests/lock-client.c') + test_protocol_sources,
  dependencies: [wayland_client],
  install: false,
)

test(
  'session-lock-crash',
  find_program('tests/lock.test.sh'),
  args: [
    viewport_exe.full_path(),
    lock_client.full_path(),
    capture_client.full_path(),
    paint_client.full_path(),
  ],
  depends: [viewport_exe, lock_client, capture_client, paint_client],
  timeout: 120,
  protocol: 'exitcode',
)

test(
  'output-order',
  find_program('tests/output-order.test.sh'),
  args: [viewport_exe.full_path()],
  depends: [viewport_exe],
  timeout: 120,
  protocol: 'exitcode',
)

# ---------------------------------------------------------------------------
# Shell logic tests
#
# The layout engine lives in data/shell/, and nothing else in this build
# reaches it: the compositor tests drive Wayland, and running the shell under a
# headless compositor only proves that it renders — a broken tiling tree renders
# exactly as well as a working one. These stub the DOM and run the real file, so
# they check structure rather than pixels.
#
# Skipped rather than failed when node is absent: it is not a build dependency
# and a checkout without it should still get the compositor tests.
# ---------------------------------------------------------------------------

node = find_program('node', 'nodejs', required: false)
if node.found()
  shell_test = files('tests/shell.test.js')
  shell_dir = meson.current_source_dir() / 'data/shell'

  foreach layout : ['tiling', 'scrolling']
    test(
      'shell-' + layout,
      node,
      args: [shell_test, shell_dir, layout],
      timeout: 60,
      protocol: 'exitcode',
    )
    # The restart path: restore a saved layout into an empty session, then check
    # applications land where they were rather than where they started.
    test(
      'shell-session-' + layout,
      node,
      args: [shell_test, shell_dir, layout, 'session'],
      timeout: 60,
      protocol: 'exitcode',
    )
  endforeach
else
  message('node not found; shell logic tests will not run')
endif

foreach layout : ['tiling', 'scrolling']
  test(
    'capture-' + layout,
    find_program('tests/capture.test.sh'),
    args: [
      viewport_exe.full_path(),
      paint_client.full_path(),
      capture_client.full_path(),
      layout,
    ],
    depends: [viewport_exe, paint_client, capture_client],
    timeout: 120,
    protocol: 'exitcode',
  )
endforeach

install_data('data/fallback.html', install_dir: pkgdatadir)
install_data('data/config.example.json', install_dir: pkgdatadir)

# Tells xdg-desktop-portal that this compositor answers Settings requests, so
# clients asking for color-scheme reach us rather than getting no answer and
# defaulting to a light theme.
install_data(
  'data/portal-share/xdg-desktop-portal/portals/viewport.portal',
  install_dir: get_option('datadir') / 'xdg-desktop-portal' / 'portals',
)
install_data(
  'data/portal-config/xdg-desktop-portal/viewport-portals.conf',
  install_dir: get_option('sysconfdir') / 'xdg-desktop-portal',
)
install_subdir('data/shell', install_dir: pkgdatadir)

# So a display manager offers this as a session to log into, rather than it
# only being startable from a TTY.
install_data(
  'data/viewport.desktop',
  install_dir: get_option('datadir') / 'wayland-sessions',
)

summary({
  'wlroots': wlroots.version(),
  'wpe-platform': wpe_platform.version(),
  'wpe-webkit': wpe_webkit.version(),
  'pkgdatadir': pkgdatadir,
}, section: 'viewport')
