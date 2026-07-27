/* SPDX-License-Identifier: MIT */
#ifndef VIEWPORT_H
#define VIEWPORT_H

#include <stdbool.h>

#include <glib.h>
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
#include <wlr/types/wlr_xdg_output_v1.h>
#include <wlr/types/wlr_xdg_shell.h>
#include <wlr/types/wlr_content_type_v1.h>
#include <wlr/types/wlr_fractional_scale_v1.h>
#include <wlr/types/wlr_single_pixel_buffer_v1.h>
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
/* One connected control-socket client. Opaque outside ipc.c: the only thing
 * anyone else does with it is hand it back so a reply reaches the client that
 * asked rather than every listener. */
struct viewport_ipc_client;
struct viewport_glib_loop;
struct viewport_status;
struct viewport_appearance;
struct viewport_toplevel;

/* -------------------------------------------------------------------------
 * Configuration
 * ---------------------------------------------------------------------- */

/* What the config file asks of one output.
 *
 * Modes are left to the display by default; this is how to override that for a
 * monitor whose preferred timing is not the one you want. */
struct viewport_output_config {
	/* Output name, or "*" for every output the rest do not name. */
	const char *name;
	/* Use the fastest refresh rate available at the preferred resolution. */
	bool max_refresh;
	/* An explicit mode. Zero means unset; refresh is in mHz, so 239.760Hz is
	 * 239760, and zero refresh matches on resolution alone. */
	int width, height, refresh;
};

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
	/* Mirror the shell's console into the compositor log, and serve the shell
	 * uncached — without this, a JS error in the shell is completely silent and
	 * an edited shell appears to have no effect. Cheap enough to leave on. */
	bool debug;
	/* Per-frame tracing: every placement, clip and scale. Separate from `debug`
	 * because an animation produces one line per window per frame — sixty times
	 * a second — which is formatted I/O in the layout path and a log file that
	 * grows by megabytes a minute. On when you are chasing a geometry bug, off
	 * the rest of the time. */
	bool trace;
	/* Keyboard layout, in xkb terms; NULL means the system default. */
	const char *xkb_layout;
	const char *xkb_variant;
	const char *xkb_options;
	int repeat_rate;
	int repeat_delay;
	/* Cursor theme and size; NULL/0 use the defaults. */
	const char *cursor_theme;
	int cursor_size;
	/* Per-output mode preferences from the config file. */
	struct viewport_output_config *outputs;
	size_t output_count;
	/* Window rules, as the JSON array the config file contained. Passed to the
	 * shell untouched: what they mean is the shell's business. */
	const char *rules_json;
	/* Colours for the shell, as the JSON object the config file contained.
	 * Passed on untouched for the same reason as the rules: they end up as CSS
	 * custom properties, and the compositor has no opinion about them. */
	const char *theme_json;
	/* Whether the shell shows its bar: "visible", "hidden", or "auto", which
	 * reveals it only while Mod4 is held. Anything else is left to the shell's
	 * default. NULL when the config file said nothing. */
	const char *bar;
	/* What the shell draws on an empty desktop. The mark, and the note saying
	 * which keys open something — worth having the first time and steadily
	 * less so, and on an OLED panel they are two more things that sit in the
	 * same pixels all day. Both default to on. */
	bool logo;
	bool tutorial;
	/* Seconds of inactivity before the locker is run and before the outputs
	 * are turned off. Zero disables each; with both zero there is no policy at
	 * all and an external idle daemon can own it. */
	int idle_lock_after;
	int idle_blank_after;
	const char *idle_lock_command;
	/* Ask outputs for variable refresh rate. Off by default: it is a property
	 * of the monitor and the driver as much as of the compositor, and enabling
	 * it where it is not properly supported causes flicker. */
	bool adaptive_sync;
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
	/* Whether Ctrl+Alt+F1..F12 may still switch virtual terminals.
	 *
	 * On by default, and it is the last thing anyone should turn off. This is
	 * the escape hatch: it is checked before the config file, before the
	 * keybinding table and before the shell, so it keeps working when a shell
	 * never paints, a keymap is empty, or the compositor is wedged. Without it
	 * the only ways back into a machine like that are SSH and the power button.
	 *
	 * It exists for kiosks, where a visitor reaching a console is the whole
	 * threat and there is somebody else's plan for administering the box. */
	bool vt_switching;
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
	/* A client holding this receives Mod4 chords itself: a virtual machine, a
	 * nested compositor, a remote desktop. Without it there is no way for one
	 * to ever see them. */
	/* Colour management: what lets the renderer convert between what a client
	 * painted and what an output is showing, which is what makes HDR usable. */
	struct wlr_color_manager_v1 *color_manager;

	/* Kept rather than discarded: the hint a client sets through it is read on
	 * every frame, and a manager nobody queries is a protocol advertised and
	 * then ignored. */
	struct wlr_tearing_control_manager_v1 *tearing_control;
	struct wlr_fractional_scale_manager_v1 *fractional_scale_manager;
	struct wlr_single_pixel_buffer_manager_v1 *single_pixel_buffer_manager;
	struct wlr_xdg_output_manager_v1 *xdg_output_manager;
	struct wlr_content_type_manager_v1 *content_type_manager;

	struct wlr_idle_notifier_v1 *idle_notifier;
	struct wlr_idle_inhibit_manager_v1 *idle_inhibit;
	struct wlr_output_power_manager_v1 *output_power;
	struct wl_listener output_power_mode;
	/* When the seat was last used, and what has already been done about it. */
	gint64 idle_since;
	/* When the screens were turned off, so the keystroke that asked for it
	 * does not immediately turn them back on. */
	gint64 idle_blanked_at;
	unsigned int idle_timer;
	bool idle_locked;
	bool idle_blanked;

	struct wlr_keyboard_shortcuts_inhibit_manager_v1 *shortcuts_inhibit;
	struct wl_listener new_shortcuts_inhibitor;
	struct wlr_keyboard_shortcuts_inhibitor_v1 *active_inhibitor;
	struct wl_listener inhibitor_destroy;

	/* Graphics tablets: the richer protocol, plus a stylus that also drives the
	 * cursor so it works in programs that only understand a pointer. */
	struct wlr_tablet_manager_v2 *tablet_manager;
	struct viewport_tablet_tool *tablet_tool;
	struct wl_listener tablet_axis;
	struct wl_listener tablet_proximity;
	struct wl_listener tablet_tip;
	struct wl_listener tablet_button;

	struct wlr_pointer_gestures_v1 *pointer_gestures;
	struct wl_listener swipe_begin;
	struct wl_listener swipe_update;
	struct wl_listener swipe_end;
	struct wl_listener pinch_begin;
	struct wl_listener pinch_update;
	struct wl_listener pinch_end;
	struct wl_listener hold_begin;
	struct wl_listener hold_end;
	/* Set once teardown begins. Handlers that would normally tell the shell
	 * about a change must not run then: the objects they would consult are
	 * being destroyed in a fixed order, and a monitor disappearing because the
	 * backend is going away is not news anyone can act on. */
	bool shutting_down;
	/* Whether Mod4 is currently held, so the shell hears about the edges
	 * rather than about every modifier event. */
	bool logo_held;
	/* True while the shell is showing the overview. Input goes to the shell
	 * rather than to the shrunken windows it is drawing. */
	bool overview;
	/* A three-finger swipe belongs to the compositor for its whole duration. */
	bool gesture_active;
	double gesture_dx, gesture_dy;
	struct wlr_output_manager_v1 *output_manager;
	struct wl_listener output_manager_apply;
	struct wl_listener output_manager_test;
	struct wlr_foreign_toplevel_manager_v1 *foreign_toplevel_manager;
	struct wlr_ext_foreign_toplevel_list_v1 *ext_foreign_toplevel_list;
	/* Window capture, and the request the compositor has to answer for a
	 * picker's choice of a single window to be allowed at all. */
	struct wlr_ext_foreign_toplevel_image_capture_source_manager_v1
		*toplevel_capture;
	struct wl_listener toplevel_capture_request;
	struct viewport_ime *ime;
	struct viewport_notifications *notifications;
	struct viewport_output_revert *output_revert;
	struct viewport_lock *lock;
	/* Black rectangle covering the whole layout, under the locker's own
	 * surfaces. Owned by the server rather than by the lock, because it has to
	 * outlive a locker that dies: the lock layer occludes nothing once it is
	 * empty, so a crashed locker would otherwise reveal the desktop. */
	struct wlr_scene_rect *lock_backdrop;
	bool locked;

	struct wlr_xwayland *xwayland;
	struct wl_listener new_xwayland_surface;
	struct wl_listener xwayland_ready;

	struct wlr_seat *seat;
	struct wlr_cursor *cursor;
	struct wlr_xcursor_manager *xcursor_mgr;
	/* The manager a config reload replaced, kept until shutdown.
	 *
	 * The cursor may still be showing an image owned by it at the moment the
	 * new one is installed, so it cannot be destroyed there — but it was never
	 * destroyed at all, which leaked a manager and its loaded theme on every
	 * reload. One slot is enough: by the time a second reload happens the
	 * pointer has long since been redrawn from the manager that replaced it. */
	struct wlr_xcursor_manager *xcursor_mgr_prev;

	struct wl_list outputs;    /* viewport_output.link */
	struct wl_list toplevels;  /* viewport_toplevel.link */
	struct wl_list keyboards;  /* viewport_keyboard.link */

	struct viewport_web *web;
	struct viewport_ipc *ipc;
	struct viewport_glib_loop *glib_loop;
	struct viewport_status *status;
	struct viewport_appearance *appearance;
	struct viewport_config config;
	/* Path given with --config, or NULL when the default location is used. */
	const char *config_path;

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

	/* Drag movement the shell has not been told about yet.
	 *
	 * A move or resize drag reports how far the pointer travelled, and it used
	 * to report it from the motion handler — once per input event. A 1000Hz
	 * mouse therefore built, generated, escaped and compiled a thousand
	 * JavaScript programs a second to deliver three small integers, and the
	 * shell could not act on them faster than one frame anyway: the layout it
	 * feeds only repaints on the next animation frame.
	 *
	 * So the deltas accumulate here and are flushed once per output frame. The
	 * target is held as an id rather than a pointer because the window can be
	 * closed between the motion and the flush. */
	uint32_t delta_id;
	double delta_x, delta_y;
	bool delta_is_resize;
	bool delta_pending;

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
struct viewport_tablet_tool;
struct viewport_notifications;

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
	/* Whether this output is currently in an HDR mode. Per output, because a
	 * monitor that can do it sits next to one that cannot. */
	bool hdr;
	/* Set once the backend has refused a tearing page-flip, so it is not asked
	 * again every frame. Cleared on a mode change, which is the point at which
	 * the answer could differ. */
	bool tearing_refused;

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
	/* Built the first time something asks to capture this window, and kept:
	 * a picker that asks twice should not get two capture pipelines. The scene
	 * is private to the capture and holds nothing but this window's surface;
	 * see foreign.c for why it cannot be the layout's own tree. */
	struct wlr_scene *capture_scene;
	struct wlr_scene_tree *capture_tree;
	/* Last clip applied to capture_tree, so a commit that did not move the
	 * window geometry does not disturb the capture. */
	struct wlr_box capture_clip;
	struct wlr_ext_image_capture_source_v1 *capture_source;
	struct wl_listener capture_source_destroy;
	struct wlr_scene_tree *surface_tree;

	/* Target rect most recently supplied by the shell over IPC. */
	struct wlr_box box;
	/* Region of the window the shell wants shown, in layout coordinates.
	 *
	 * Normally the whole window, but a scrolled strip pushes columns past the
	 * edge of their output, and a client surface is not clipped by anything the
	 * shell draws — CSS overflow bounds the shell's own painting, not a real
	 * Wayland surface. Without this a column scrolled off the left of one
	 * monitor is simply drawn on the monitor next to it. */
	struct wlr_box clip;
	bool has_clip;
	/* Size most recently requested of the client, so a move does not
	 * reconfigure it. */
	int last_width, last_height;
	/* Fires if the shell never places this window; see watchdog.c. */
	unsigned int watchdog;
	/* Render scale. 1.0 normally; smaller in the overview, where a window is
	 * drawn shrunk without its client being resized. */
	double scale;
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
	struct wl_listener request_maximize;
	struct wl_listener request_minimize;
	struct wl_listener request_move;
	struct wl_listener request_resize;
	/* XWayland only. */
	struct wl_listener associate;
	struct wl_listener dissociate;
	struct wl_listener request_configure;
	struct wl_listener set_geometry;
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
	/* Supplies its own keymap; never reconfigured from the config file. */
	bool virtual_keyboard;
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



/* Runs a command detached from the compositor, so quitting does not take it
 * down and no zombie is left behind. */
void viewport_spawn(const char *command);

/* Idle policy: lock and blank when nobody is there. */
void viewport_idle_init(struct viewport_server *server);
void viewport_idle_finish(struct viewport_server *server);
void viewport_idle_activity(struct viewport_server *server);
/* Turns the outputs off now. Any input turns them back on, exactly as it does
 * when the idle timer blanks them. */
void viewport_idle_blank(struct viewport_server *server);

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
/* The config block for an output, by name, falling back to "*". NULL when the
 * config says nothing about it. */
const struct viewport_output_config *viewport_output_config_for(
	struct viewport_server *server, const char *name);
/* Re-reads the config file into a running compositor: bindings, keyboard,
 * cursor, appearance and layout model. */
void viewport_config_reload(struct viewport_server *server);
/* Re-applies keymap and repeat settings to every attached keyboard. */
void viewport_keyboards_reconfigure(struct viewport_server *server);


/* -------------------------------------------------------------------------
 * The rest
 *
 * Declared in headers of their own rather than here. Include the one you need:
 *
 *   viewport-view.h     windows: xdg, Xwayland, layer shell, foreign toplevels
 *   viewport-input.h    pointer, keyboard, touch, tablet, bindings, IME
 *   viewport-output.h   monitors, modes, HDR, session lock
 *   viewport-shell.h    the web engine, IPC, the GLib loop, status, notifications
 * ---------------------------------------------------------------------- */

#endif /* VIEWPORT_H */
