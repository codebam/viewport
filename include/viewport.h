/* SPDX-License-Identifier: MIT */
#ifndef VIEWPORT_H
#define VIEWPORT_H

#include <stdbool.h>
#include <stdint.h>

#include <wayland-server-core.h>

#include <wlr/backend.h>
#include <wlr/backend/session.h>
#include <wlr/render/allocator.h>
#include <wlr/render/wlr_renderer.h>
#include <wlr/types/wlr_compositor.h>
#include <wlr/types/wlr_cursor.h>
#include <wlr/types/wlr_input_device.h>
#include <wlr/types/wlr_keyboard.h>
#include <wlr/types/wlr_output.h>
#include <wlr/types/wlr_output_layout.h>
#include <wlr/types/wlr_pointer_constraints_v1.h>
#include <wlr/types/wlr_relative_pointer_v1.h>
#include <wlr/types/wlr_scene.h>
#include <wlr/types/wlr_seat.h>
#include <wlr/types/wlr_session_lock_v1.h>
#include <wlr/types/wlr_xcursor_manager.h>
#include <wlr/types/wlr_xdg_decoration_v1.h>
#include <wlr/types/wlr_xdg_shell.h>
#include <wlr/util/box.h>
#include <wlr/xwayland.h>
#include <xkbcommon/xkbcommon.h>

/* Scene nodes carry a back-pointer in wlr_scene_node.data so hit-testing can
 * walk up from whatever node the cursor landed on. More than one kind of
 * object lives in the scene graph, so that pointer must be self-describing:
 * casting a layer surface to a toplevel because both set `data` is a
 * segfault the compiler cannot catch. Every struct stored there begins with
 * one of these. */
enum viewport_node_type {
	VIEWPORT_NODE_TOPLEVEL,
	VIEWPORT_NODE_LAYER,
};

/* Where a window's surface came from.
 *
 * An X11 window is not an xdg_toplevel: it has no xdg surface, its size and
 * position are set through a different call, and it reports its title and
 * class differently. Everything above this — tiling, workspaces, focus, the
 * IPC view model — is identical, so the difference is confined to the few
 * operations that actually touch the protocol. */
enum viewport_view_kind {
	VIEWPORT_VIEW_XDG,
	VIEWPORT_VIEW_XWAYLAND,
};

struct viewport_node {
	enum viewport_node_type type;
};

struct viewport_web;
struct viewport_ipc;
struct viewport_glib_loop;
struct viewport_status;
struct viewport_appearance;
struct viewport_toplevel;

/* -------------------------------------------------------------------------
 * Configuration
 * ---------------------------------------------------------------------- */

struct viewport_config {
	/* Shell UI endpoint, e.g. "http://localhost:3000". */
	const char *url;
	/* Loaded if `url` fails or does not commit a first frame in time. */
	const char *fallback_url;
	/* Milliseconds to wait for the shell's first paint before falling back. */
	unsigned load_timeout_ms;
	/* Optional command spawned once the compositor is up. */
	const char *startup_cmd;
	/* Path of the JSON control socket; NULL selects the default under
	 * $XDG_RUNTIME_DIR. */
	const char *ipc_path;
	/* Run headless (nested/CI) rather than taking a DRM session. */
	bool headless;
	/* Verbose logging, and mirror the shell's console into the compositor
	 * log — without this, a JS error in the shell is completely silent. */
	bool debug;
	/* Keyboard layout, in xkb terms; NULL means the system default. */
	const char *xkb_layout;
	const char *xkb_variant;
	const char *xkb_options;
	int repeat_rate;
	int repeat_delay;
	/* Cursor theme and size; NULL/0 use the defaults. */
	const char *cursor_theme;
	int cursor_size;
	/* Which layout model the shell runs: "tiling" for i3-style splits, or
	 * "scrolling" for niri's infinite strip of columns. The shell implements
	 * both; this only picks the default and the matching key bindings. */
	const char *layout;
	/* Commands bound to the default terminal and launcher chords. */
	const char *terminal;
	const char *menu;
	/* Set when the config file supplied a "binds" object, which suppresses
	 * the built-in defaults so a user can define an empty keymap. */
	bool binds_from_config;
	/* Prefer dark for client applications, served over the settings portal. */
	bool dark_mode;
	/* Ask clients to drop their own titlebars and let the shell draw the
	 * frame. Without this every window carries a second, redundant titlebar
	 * above the one the shell already drew. */
	bool server_decorations;
};

/* -------------------------------------------------------------------------
 * Scene layers
 *
 * Bottom to top:
 *   layer_web     the WPE WebKit shell, one full-output scene buffer
 *   layer_apps    xdg-shell toplevels, positioned by the shell over IPC
 *   layer_overlay drag icons, popups that must outrank apps
 *
 * Hit-testing falls out of this ordering: wlr_scene_node_at() returns a
 * client surface when the pointer is over an app rect, and the web buffer
 * everywhere else — which is exactly the routing rule we want.
 * ---------------------------------------------------------------------- */

struct viewport_server {
	struct wl_display *wl_display;
	struct wl_event_loop *wl_event_loop;

	struct wlr_backend *backend;
	struct wlr_session *session;
	struct wlr_renderer *renderer;
	struct wlr_allocator *allocator;
	struct wlr_compositor *compositor;

	struct wlr_scene *scene;
	struct wlr_scene_output_layout *scene_layout;
	struct wlr_output_layout *output_layout;

	/* Bottom to top. Layer-shell surfaces bracket the app layer so a panel
	 * can sit under windows and a launcher over them. */
	struct wlr_scene_tree *layer_web;
	struct wlr_scene_tree *layer_bg;
	struct wlr_scene_tree *layer_bottom;
	struct wlr_scene_tree *layer_apps;
	struct wlr_scene_tree *layer_top;
	struct wlr_scene_tree *layer_overlay;
	/* Above everything: while locked nothing else may be shown. */
	struct wlr_scene_tree *layer_lock;

	struct wlr_layer_shell_v1 *layer_shell;
	struct wl_listener new_layer_surface;

	struct wlr_xdg_shell *xdg_shell;

	struct wlr_session_lock_manager_v1 *session_lock_manager;
	struct wl_listener new_session_lock;
	struct wlr_xdg_activation_v1 *activation;
	struct wl_listener request_activate;
	struct wlr_cursor_shape_manager_v1 *cursor_shape;
	struct wl_listener request_set_shape;
	struct wl_listener touch_down;
	struct wl_listener touch_up;
	struct wl_listener touch_motion;
	struct wl_listener touch_frame;
	struct wl_listener touch_cancel;
	/* Set once a touchscreen appears; drives the seat's touch capability. */
	bool has_touch;
	struct wlr_output_manager_v1 *output_manager;
	struct wl_listener output_manager_apply;
	struct wl_listener output_manager_test;
	struct wlr_foreign_toplevel_manager_v1 *foreign_toplevel_manager;
	struct viewport_ime *ime;
	struct viewport_output_revert *output_revert;
	struct viewport_lock *lock;
	bool locked;

	struct wlr_xwayland *xwayland;
	struct wl_listener new_xwayland_surface;
	struct wl_listener xwayland_ready;

	struct wlr_seat *seat;
	struct wlr_cursor *cursor;
	struct wlr_xcursor_manager *xcursor_mgr;

	struct wl_list outputs;    /* viewport_output.link */
	struct wl_list toplevels;  /* viewport_toplevel.link */
	struct wl_list keyboards;  /* viewport_keyboard.link */

	struct viewport_web *web;
	struct viewport_ipc *ipc;
	struct viewport_glib_loop *glib_loop;
	struct viewport_status *status;
	struct viewport_appearance *appearance;
	struct viewport_config config;

	/* Toplevel holding keyboard focus, or NULL when the shell has it. */
	struct viewport_toplevel *focused;
	/* True while the pointer is over the web shell rather than a client. */
	bool pointer_on_web;

	uint32_t next_view_id;
	const char *socket_name;

	/* Output the shell considers active. The shell owns this concept — it
	 * changes on keyboard focus moves, not just pointer motion — so C is told
	 * rather than guessing from the cursor. NULL until the shell reports. */
	char *active_output;

	/* Active binding mode. Only bindings tagged with it — plus the ones that
	 * leave it — are considered while it is not "default". */
	char *mode;

	/* True from a press over the shell until release, so a drag that starts on
	 * shell chrome keeps receiving motion after the pointer crosses a window. */
	bool pointer_grab_web;

	/* Pointer capture for games: relative motion plus lock/confine. */
	struct wlr_relative_pointer_manager_v1 *relative_pointer;
	struct wlr_pointer_constraints_v1 *pointer_constraints;
	struct wlr_pointer_constraint_v1 *active_constraint;
	struct wl_listener new_constraint;

	/* Interactive resize driven by Mod4 + right drag. */
	struct viewport_toplevel *resizing;
	/* Mod4 + left drag on a floating window moves it. */
	struct viewport_toplevel *moving;
	double resize_start_x, resize_start_y;

	struct wl_list bindings; /* viewport_binding.link */

	struct wl_listener new_output;
	struct wl_listener new_input;
	struct wl_listener new_xdg_toplevel;
	struct wl_listener new_xdg_popup;
	struct wl_listener new_decoration;
	struct wl_listener new_virtual_keyboard;
	struct wl_listener session_active;
	struct wl_listener renderer_lost;

	struct wl_listener cursor_motion;
	struct wl_listener cursor_motion_absolute;
	struct wl_listener cursor_button;
	struct wl_listener cursor_axis;
	struct wl_listener cursor_frame;
	struct wl_listener request_cursor;
	struct wl_listener request_set_selection;
	struct wl_listener request_set_primary_selection;
	struct wl_listener request_start_drag;
	struct wl_listener start_drag;
};

struct viewport_foreign;
struct viewport_ime;

/* An output configuration applied but not yet confirmed.
 *
 * A wrong mode or a bad position blanks the very screen the user would need in
 * order to undo it. So a change is provisional: it reverts on its own unless an
 * output.confirm arrives, which is what every display settings panel does with
 * its "keep this?" countdown. */
struct viewport_output_revert {
	struct viewport_server *server;
	char *name;

	bool enabled;
	int width, height, refresh;
	float scale;
	enum wl_output_transform transform;
	bool adaptive_sync;
	int x, y;

	unsigned int timer;
};

struct viewport_output {
	struct wl_list link;
	struct viewport_server *server;
	struct wlr_output *wlr_output;
	struct wlr_scene_output *scene_output;
	/* What is left of the output after panels have claimed their exclusive
	 * zones. Published to the shell, which tiles windows inside it. */
	struct wlr_box usable_area;

	struct wl_listener frame;
	struct wl_listener request_state;
	struct wl_listener destroy;
};

struct viewport_toplevel {
	/* Must stay first: scene node data is downcast through this. */
	struct viewport_node node;

	struct wl_list link;
	struct viewport_server *server;

	/* Stable identifier handed to JS; never reused within a session. */
	uint32_t id;

	enum viewport_view_kind kind;
	/* Exactly one of these is set, according to `kind`. */
	struct wlr_xdg_toplevel *xdg_toplevel;
	struct wlr_xwayland_surface *xwayland_surface;

	/* Container: holds the surface tree and any popups. Popups must live
	 * outside the clipped surface tree or a menu that extends past the window
	 * edge gets cropped. */
	struct wlr_scene_tree *scene_tree;
	struct wlr_scene_tree *surface_tree;

	/* Target rect most recently supplied by the shell over IPC. */
	struct wlr_box box;
	/* Last clip applied, so diagnostics only fire on change. */
	struct wlr_box last_clip;
	bool has_box;
	bool mapped;
	/* Last fullscreen state the shell asked for; mirrored to external tools. */
	bool fullscreen;
	/* Handle published to external taskbars; NULL while unmapped. */
	struct viewport_foreign *foreign;
	/* False while the shell has it parked on another workspace. Directional
	 * focus must skip these: their box is stale and they are not on screen. */
	bool visible;

	struct wl_listener map;
	struct wl_listener unmap;
	struct wl_listener commit;
	struct wl_listener destroy;
	struct wl_listener set_title;
	struct wl_listener set_app_id;
	struct wl_listener request_fullscreen;
	/* XWayland only. */
	struct wl_listener associate;
	struct wl_listener dissociate;
	struct wl_listener request_configure;
};

struct viewport_decoration {
	struct viewport_server *server;
	struct wlr_xdg_toplevel_decoration_v1 *decoration;
	struct wl_listener request_mode;
	struct wl_listener surface_commit;
	struct wl_listener destroy;
};

struct viewport_lock {
	struct viewport_server *server;
	struct wlr_session_lock_v1 *lock;
	struct wl_listener new_surface;
	struct wl_listener unlock;
	struct wl_listener destroy;
};

struct viewport_constraint {
	struct viewport_server *server;
	struct wlr_pointer_constraint_v1 *constraint;
	struct wl_listener destroy;
};

struct viewport_popup {
	struct wlr_xdg_popup *xdg_popup;
	/* NULL when the parent is another popup. */
	struct viewport_toplevel *toplevel;
	struct wl_listener commit;
	struct wl_listener reposition;
	struct wl_listener destroy;
};

struct viewport_keyboard {
	struct wl_list link;
	struct viewport_server *server;
	struct wlr_keyboard *wlr_keyboard;

	struct wl_listener modifiers;
	struct wl_listener key;
	struct wl_listener destroy;
};

/* -------------------------------------------------------------------------
 * server.c
 * ---------------------------------------------------------------------- */

bool viewport_server_init(struct viewport_server *server,
	const struct viewport_config *config);
bool viewport_server_start(struct viewport_server *server);

/* Shuts the compositor down.
 *
 * wl_display_terminate() alone is a no-op here: it only unblocks
 * wl_display_run(), which we never call because GLib owns the outer loop. The
 * GLib loop has to be quit explicitly or "exit" silently does nothing. */
void viewport_server_terminate(struct viewport_server *server);
void viewport_server_run(struct viewport_server *server);
void viewport_server_finish(struct viewport_server *server);

struct viewport_toplevel *viewport_server_find_toplevel(
	struct viewport_server *server, uint32_t id);

/* -------------------------------------------------------------------------
 * output.c
 * ---------------------------------------------------------------------- */

void viewport_handle_new_output(struct wl_listener *listener, void *data);

/* Total layout extent, used to size the shell's viewport. */
void viewport_layout_size(struct viewport_server *server,
	int *width, int *height);

/* -------------------------------------------------------------------------
 * xdg_shell.c
 * ---------------------------------------------------------------------- */

void viewport_handle_new_xdg_toplevel(struct wl_listener *listener, void *data);
void viewport_handle_new_xdg_popup(struct wl_listener *listener, void *data);
void viewport_handle_new_decoration(struct wl_listener *listener, void *data);
void viewport_handle_request_activate(struct wl_listener *listener, void *data);

/* Display configuration, for wlr-randr and kanshi. */
void viewport_output_manager_init(struct viewport_server *server);
void viewport_output_manager_update(struct viewport_server *server);
void viewport_output_revert_cancel(struct viewport_server *server);

/* Input methods: the relay between text-input-v3 and input-method-v2. */
struct viewport_ime *viewport_ime_create(struct viewport_server *server);
void viewport_ime_destroy(struct viewport_ime *ime);
bool viewport_ime_handle_key(struct viewport_ime *ime,
	struct wlr_keyboard *keyboard, uint32_t time_msec, uint32_t keycode,
	uint32_t state);
bool viewport_ime_handle_modifiers(struct viewport_ime *ime,
	struct wlr_keyboard *keyboard);

/* The window list, as seen by taskbars and window switchers. */
void viewport_foreign_init(struct viewport_server *server);
void viewport_foreign_view_map(struct viewport_toplevel *toplevel);
void viewport_foreign_view_unmap(struct viewport_toplevel *toplevel);
void viewport_foreign_view_props(struct viewport_toplevel *toplevel);
void viewport_foreign_view_state(struct viewport_toplevel *toplevel,
	bool activated, bool fullscreen);

/* Applies an IPC-supplied rect: repositions the scene node and reconfigures
 * the client. Safe to call before the toplevel maps. */
void viewport_toplevel_set_box(struct viewport_toplevel *toplevel,
	const struct wlr_box *box);

void viewport_toplevel_focus(struct viewport_toplevel *toplevel);
void viewport_toplevel_close(struct viewport_toplevel *toplevel);

/* Moves focus. `direction` is next, prev, left, right, up or down.
 *
 * Focus lives in C rather than the shell because it needs the seat, and
 * because it has to keep working when the shell is unreachable. Directional
 * moves compare window centres, so they follow what is on screen rather than
 * stacking order. */
void viewport_focus_direction(struct viewport_server *server,
	const char *direction);

/* Surface-kind-independent accessors, so tiling, focus and the IPC layer do
 * not care whether a window is Wayland-native or X11. */
const char *viewport_view_title(struct viewport_toplevel *toplevel);
const char *viewport_view_app_id(struct viewport_toplevel *toplevel);
struct wlr_surface *viewport_view_surface(struct viewport_toplevel *toplevel);
void viewport_view_set_size(struct viewport_toplevel *toplevel, int width,
	int height);
void viewport_view_set_activated(struct viewport_toplevel *toplevel,
	bool activated);
void viewport_view_set_fullscreen(struct viewport_toplevel *toplevel, bool on);
void viewport_view_close(struct viewport_toplevel *toplevel);
struct wlr_box viewport_view_geometry(struct viewport_toplevel *toplevel);
void viewport_view_natural_size(struct viewport_toplevel *toplevel, int *width,
	int *height);
bool viewport_view_wants_floating(struct viewport_toplevel *toplevel);

/* Shared lifecycle, used by both xdg_shell.c and xwayland.c. */
void viewport_view_map(struct viewport_toplevel *toplevel);
void viewport_view_unmap(struct viewport_toplevel *toplevel);

/* -------------------------------------------------------------------------
 * xwayland.c
 * ---------------------------------------------------------------------- */

void viewport_xwayland_init(struct viewport_server *server);
void viewport_handle_new_xwayland_surface(struct wl_listener *listener,
	void *data);

/* -------------------------------------------------------------------------
 * layer_shell.c
 *
 * wlr-layer-shell backs panels, wallpapers, lock screens and — the reason it
 * is here — launchers. wmenu, wofi, rofi and friends are layer-shell clients;
 * without this protocol they exit immediately and the launcher keybinding
 * appears to do nothing at all.
 * ---------------------------------------------------------------------- */

void viewport_handle_new_layer_surface(struct wl_listener *listener,
	void *data);

/* Re-runs layer-surface layout for an output, e.g. after a mode change. */
void viewport_layers_arrange(struct viewport_output *output);

/* -------------------------------------------------------------------------
 * input.c
 * ---------------------------------------------------------------------- */

void viewport_handle_new_input(struct wl_listener *listener, void *data);
void viewport_handle_new_virtual_keyboard(struct wl_listener *listener,
	void *data);
void viewport_cursor_init(struct viewport_server *server);

/* Topmost surface under a layout-space point, or NULL if the point lands on
 * the web shell. `sx`/`sy` receive surface-local coordinates. */
struct wlr_surface *viewport_surface_at(struct viewport_server *server,
	double lx, double ly, double *sx, double *sy,
	struct viewport_toplevel **toplevel_out);

/* Moves keyboard focus to the shell (no client toplevel focused). */
void viewport_focus_web(struct viewport_server *server);

/* -------------------------------------------------------------------------
 * appearance.c
 *
 * Serves org.freedesktop.appearance/color-scheme so client applications pick
 * a dark or light theme. Styling the shell cannot do this: every toolkit asks
 * the portal, and with nothing answering they all default to light.
 * ---------------------------------------------------------------------- */

struct viewport_appearance *viewport_appearance_create(
	struct viewport_server *server, bool dark);
void viewport_appearance_destroy(struct viewport_appearance *appearance);
void viewport_appearance_set_dark(struct viewport_appearance *appearance,
	bool dark);
bool viewport_appearance_is_dark(struct viewport_appearance *appearance);

/* -------------------------------------------------------------------------
 * status.c
 *
 * Samples /proc for the shell's status bar. Only raw values — formatting,
 * icons and which modules exist are the shell's business.
 * ---------------------------------------------------------------------- */

struct viewport_status *viewport_status_create(struct viewport_server *server);
void viewport_status_destroy(struct viewport_status *status);

/* -------------------------------------------------------------------------
 * config.c
 * ---------------------------------------------------------------------- */

/* $XDG_CONFIG_HOME/viewport/config.json, else ~/.config/... Caller frees. */
char *viewport_config_default_path(void);

/* Applies a JSON config over `config` and registers its binds on `server`.
 * Returns false if the file is absent or malformed. */
bool viewport_config_load(struct viewport_server *server,
	struct viewport_config *config, const char *path, bool required);

void viewport_config_finish(void);

/* -------------------------------------------------------------------------
 * binding.c
 * ---------------------------------------------------------------------- */

enum viewport_action {
	VIEWPORT_ACTION_EXEC,   /* run a shell command */
	VIEWPORT_ACTION_CLOSE,  /* close the focused window */
	VIEWPORT_ACTION_EXIT,   /* quit the compositor */
	VIEWPORT_ACTION_RELOAD, /* reload the web shell */
	VIEWPORT_ACTION_FOCUS,  /* move focus: next|prev|left|right|up|down */
	VIEWPORT_ACTION_SHELL,  /* forward the rest of the line to the shell */
	VIEWPORT_ACTION_MODE,   /* switch binding mode, e.g. sway's resize mode */
	VIEWPORT_ACTION_APPEARANCE, /* toggle the dark/light preference */
};

struct viewport_binding {
	struct wl_list link;
	/* Mode this binding belongs to; "default" unless qualified. Written in
	 * the config as `resize/h=...`, mirroring sway's `mode "resize"` blocks. */
	char *mode;
	uint32_t modifiers;   /* mask of WLR_MODIFIER_* */
	xkb_keysym_t keysym;
	enum viewport_action action;
	char *argument;       /* command line, for VIEWPORT_ACTION_EXEC */
};

/* Parses "Mod4+Shift+q=close" or "Mod4+Return=exec ghostty". Returns false
 * with a logged reason on malformed input. */
bool viewport_binding_add(struct viewport_server *server, const char *spec);

/* Installs the sway-compatible defaults. `terminal` and `menu` may be NULL to
 * skip those two binds. */
void viewport_bindings_add_defaults(struct viewport_server *server,
	const char *terminal, const char *menu);

/* Runs the binding matching this chord, if any. Returns true when the key was
 * consumed and must not reach the focused client or the shell. */
bool viewport_bindings_handle(struct viewport_server *server,
	uint32_t modifiers, const xkb_keysym_t *keysyms, int nsyms);

void viewport_bindings_finish(struct viewport_server *server);

/* -------------------------------------------------------------------------
 * session_lock.c
 *
 * ext-session-lock-v1. Without it nothing can lock the screen: swaylock and
 * every other locker exits immediately when the protocol is absent.
 * ---------------------------------------------------------------------- */

void viewport_session_lock_init(struct viewport_server *server);

/* -------------------------------------------------------------------------
 * pointer.c
 *
 * Pointer capture: relative motion and lock/confine constraints, which is what
 * lets a first-person game read mouselook. Covers X11 games too — Xwayland
 * implements XGrabPointer using these same protocols.
 * ---------------------------------------------------------------------- */

void viewport_pointer_init(struct viewport_server *server);
void viewport_handle_new_constraint(struct wl_listener *listener, void *data);
void viewport_pointer_apply_constraint(struct viewport_server *server,
	struct wlr_pointer_constraint_v1 *constraint);
void viewport_pointer_deactivate_constraint(struct viewport_server *server);
void viewport_pointer_check_constraint(struct viewport_server *server,
	struct wlr_surface *surface);
bool viewport_pointer_is_locked(struct viewport_server *server);
bool viewport_pointer_confine(struct viewport_server *server, double *lx,
	double *ly);
void viewport_pointer_send_relative(struct viewport_server *server,
	uint32_t time_msec, double dx, double dy, double dx_unaccel,
	double dy_unaccel);

/* -------------------------------------------------------------------------
 * ipc.c
 * ---------------------------------------------------------------------- */

struct viewport_ipc *viewport_ipc_create(struct viewport_server *server,
	const char *path);
void viewport_ipc_destroy(struct viewport_ipc *ipc);

/* Handles one JSON message from either transport (UNIX socket or the page's
 * script message handler). `json` is a NUL-terminated UTF-8 document. */
void viewport_ipc_handle(struct viewport_server *server, const char *json,
	size_t len);

/* Pushes a JSON event to every listener: socket clients and the page. */
void viewport_ipc_broadcast(struct viewport_server *server, const char *json);

/* Convenience emitters used by the shell/xdg glue. */
void viewport_ipc_notify_view_added(struct viewport_toplevel *toplevel);
void viewport_ipc_notify_view_removed(struct viewport_toplevel *toplevel);
void viewport_ipc_notify_view_props(struct viewport_toplevel *toplevel);
void viewport_ipc_notify_output_layout(struct viewport_server *server);

/* Settings the shell needs in order to render: which layout model to use.
 * Sent on connect and on page load, alongside the view replay. */
void viewport_ipc_notify_config(struct viewport_server *server);

/* Replays view.added for every currently mapped toplevel.
 *
 * view.added is an edge, not a state: a client that maps before the shell has
 * finished loading — or before an external tool connects — would otherwise be
 * invisible to it forever. Sent on page load, on socket connect, and on
 * explicit view.query, which also makes shell reloads non-destructive. */
void viewport_ipc_notify_views(struct viewport_server *server);

/* Tells the shell which view now holds focus; id 0 means the shell itself. */
void viewport_ipc_notify_focus(struct viewport_server *server, uint32_t id);

/* Forwards a keybinding to the shell as {"type":"shell.command",...}, so
 * workspaces and other layout policy can live in JS where the layout already
 * is, while still being bound to a key in the config file. */
void viewport_ipc_notify_shell_command(struct viewport_server *server,
	const char *command);

/* -------------------------------------------------------------------------
 * glib_loop.c
 *
 * WebKit needs a GMainContext; wlroots needs a wl_event_loop. We run GLib as
 * the outer loop and attach the Wayland loop's epoll fd to it as a GSource,
 * flushing clients in the prepare phase.
 * ---------------------------------------------------------------------- */

struct viewport_glib_loop;
struct viewport_status;
struct viewport_appearance;

struct viewport_glib_loop *viewport_glib_loop_create(
	struct wl_display *display);
void viewport_glib_loop_run(struct viewport_glib_loop *loop);
void viewport_glib_loop_quit(struct viewport_glib_loop *loop);
void viewport_glib_loop_destroy(struct viewport_glib_loop *loop);

/* -------------------------------------------------------------------------
 * web.c
 * ---------------------------------------------------------------------- */

struct viewport_web *viewport_web_create(struct viewport_server *server);
void viewport_web_destroy(struct viewport_web *web);

void viewport_web_resize(struct viewport_web *web, int width, int height);

/* Called once per presented output frame. Releases the frame WebKit is
 * waiting on, which is what unblocks it to paint the next one — the shell's
 * frame pacing is therefore driven by real vblank, not a timer. */
void viewport_web_notify_presented(struct viewport_web *web);

/* True while a WebKit frame is waiting to be acknowledged. Diagnostic only. */
bool viewport_web_has_pending(struct viewport_web *web);

/* Scene node backing the shell, used for hit-testing. */
struct wlr_scene_buffer *viewport_web_scene_buffer(struct viewport_web *web);

/* Re-fetches the shell from its URL, discarding the current document. */
void viewport_web_reload(struct viewport_web *web);

/* Sends a JSON string to the page as a `viewport` CustomEvent. */
void viewport_web_post_to_page(struct viewport_web *web, const char *json);

/* Routes an input event to the shell. Coordinates are layout-space. */
void viewport_web_pointer_motion(struct viewport_web *web, uint32_t time_msec,
	double lx, double ly);
void viewport_web_pointer_button(struct viewport_web *web, uint32_t time_msec,
	double lx, double ly, uint32_t button, bool pressed);
void viewport_web_pointer_axis(struct viewport_web *web, uint32_t time_msec,
	double lx, double ly, double dx, double dy, bool precise);
void viewport_web_keyboard_key(struct viewport_web *web, uint32_t time_msec,
	uint32_t keycode, uint32_t keysym, bool pressed, uint32_t modifiers);
void viewport_web_focus(struct viewport_web *web, bool focused);

/* -------------------------------------------------------------------------
 * web_buffer.c
 * ---------------------------------------------------------------------- */

struct wpe_buffer_dma_buf;

/* Wraps a WPEBufferDMABuf in a wlr_buffer without copying. The returned
 * buffer owns one reference; drop it after handing it to the scene. */
struct wlr_buffer *viewport_web_buffer_wrap(struct viewport_web *web,
	void *wpe_buffer_dma_buf);

#endif /* VIEWPORT_H */
