/* SPDX-License-Identifier: MIT
 *
 * Windows.
 *
 * Everything that turns a client surface into something the shell can place:
 the two shells that produce one (xdg and Xwayland), the layer-shell surfaces
 that bracket them, the accessors that let the rest of the compositor stop
 caring which kind it is, and the foreign-toplevel handles that publish the
 result to taskbars.
 *
 * Split out of viewport.h, which had grown to declare every function in the
 * project: a comment edited in it recompiled all twenty-nine translation
 * units, and nothing recorded which of them any given declaration was for.
 * The types stay there — struct viewport_server embeds them by value, so
 * everything needs it — and only the interfaces moved.
 */
#ifndef VIEWPORT_VIEW_H
#define VIEWPORT_VIEW_H

#include "viewport.h"

/* -------------------------------------------------------------------------
 * xdg_shell.c
 * ---------------------------------------------------------------------- */

void viewport_handle_new_xdg_toplevel(struct wl_listener *listener, void *data);
void viewport_handle_new_xdg_popup(struct wl_listener *listener, void *data);
void viewport_handle_new_decoration(struct wl_listener *listener, void *data);
void viewport_handle_request_activate(struct wl_listener *listener, void *data);
/* Re-applies the window's crop and render scale to its buffers. Needed after
 * any commit, because wlr_scene_surface resets both from the surface, and the
 * two must be applied together — the scale is derived from what the crop left. */
void viewport_toplevel_apply_crop(struct viewport_toplevel *toplevel);
/* Drops the remembered unscaled sizes. Called when nothing is scaled any more.
 *
 * Takes the server because the state lives on the scene buffers themselves,
 * one wlr_addon per buffer, so forgetting means walking the windows rather
 * than emptying a table off to the side. */
void viewport_scale_forget(struct viewport_server *server);

/* Last-resort placement, for when the shell stops answering. */
void viewport_watchdog_arm(struct viewport_toplevel *toplevel);
void viewport_watchdog_disarm(struct viewport_toplevel *toplevel);

/* The window list, as seen by taskbars and window switchers. */
void viewport_foreign_init(struct viewport_server *server);
void viewport_foreign_view_map(struct viewport_toplevel *toplevel);
void viewport_foreign_view_unmap(struct viewport_toplevel *toplevel);
void viewport_foreign_capture_finish(struct viewport_toplevel *toplevel);
void viewport_foreign_capture_update(struct viewport_toplevel *toplevel);
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
/* True for X11 windows that bypass the window manager entirely. */
bool viewport_view_is_unmanaged(struct viewport_toplevel *toplevel);

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

/* Re-runs layer-surface layout for an output, e.g. after a mode change.
 * A NULL output is ignored, so callers holding a surface whose output has
 * gone away need no guard of their own. */
void viewport_layers_arrange(struct viewport_output *output);

/* Destroys every layer surface homed on an output that is being torn down.
 * Must be called while the output is still valid — see the commentary in
 * layer_shell.c for the use-after-free it prevents. */
void viewport_layers_output_destroyed(struct viewport_output *output);

#endif /* VIEWPORT_VIEW_H */
