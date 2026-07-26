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

viewport_sources = files(
  'src/main.c',
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
  'src/ipc.c',
  'src/glib_loop.c',
  'src/wpe_display.c',
  'src/wpe_view.c',
  'src/web.c',
  'src/web_buffer.c',
)

viewport_sources += layer_shell_protocol_h

executable(
  'viewport',
  viewport_sources,
  include_directories: include_directories('include'),
  dependencies: viewport_deps,
  install: true,
)

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

summary({
  'wlroots': wlroots.version(),
  'wpe-platform': wpe_platform.version(),
  'wpe-webkit': wpe_webkit.version(),
  'pkgdatadir': pkgdatadir,
}, section: 'viewport')
