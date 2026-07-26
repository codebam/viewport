/* SPDX-License-Identifier: MIT
 *
 * The seam between window types.
 *
 * A tiled window may be an xdg_toplevel or an X11 window owned by Xwayland.
 * Above this file nothing knows the difference: the tiling tree, workspaces,
 * focus, the status bar and the whole IPC view model operate on
 * `struct viewport_toplevel` regardless.
 *
 * That is the point of collecting the differences here. They are small and
 * uninteresting — a different call to set a size, a different field holding a
 * title — but if each were branched at its call site the distinction would
 * spread through every layer of the compositor.
 */
#define _POSIX_C_SOURCE 200809L

#include <stdlib.h>

#include <wlr/types/wlr_scene.h>
#include <wlr/util/log.h>

#include "viewport.h"

const char *viewport_view_title(struct viewport_toplevel *toplevel)
{
	const char *title = toplevel->kind == VIEWPORT_VIEW_XDG
		? toplevel->xdg_toplevel->title
		: toplevel->xwayland_surface->title;
	return title != NULL ? title : "";
}

/* X11 has no app id; the WM_CLASS is the closest equivalent and is what
 * launchers and taskbars have always keyed off. */
const char *viewport_view_app_id(struct viewport_toplevel *toplevel)
{
	const char *app_id = toplevel->kind == VIEWPORT_VIEW_XDG
		? toplevel->xdg_toplevel->app_id
		: toplevel->xwayland_surface->class;
	return app_id != NULL ? app_id : "";
}

struct wlr_surface *viewport_view_surface(struct viewport_toplevel *toplevel)
{
	if (toplevel->kind == VIEWPORT_VIEW_XDG) {
		return toplevel->xdg_toplevel->base->surface;
	}
	return toplevel->xwayland_surface->surface;
}

void viewport_view_set_size(struct viewport_toplevel *toplevel, int width,
	int height)
{
	if (toplevel->kind == VIEWPORT_VIEW_XDG) {
		wlr_xdg_toplevel_set_size(toplevel->xdg_toplevel, width, height);
		return;
	}

	/* X11 has no separate notion of position and size: a configure carries
	 * both, and the coordinates are absolute in the X screen, which here is
	 * the output layout. So the window's placement has to be repeated on
	 * every resize. */
	wlr_xwayland_surface_configure(toplevel->xwayland_surface,
		toplevel->box.x, toplevel->box.y, width, height);
}

void viewport_view_set_activated(struct viewport_toplevel *toplevel,
	bool activated)
{
	if (toplevel->kind == VIEWPORT_VIEW_XDG) {
		wlr_xdg_toplevel_set_activated(toplevel->xdg_toplevel, activated);
		return;
	}
	wlr_xwayland_surface_activate(toplevel->xwayland_surface, activated);
}

void viewport_view_set_fullscreen(struct viewport_toplevel *toplevel, bool on)
{
	if (toplevel->kind == VIEWPORT_VIEW_XDG) {
		wlr_xdg_toplevel_set_fullscreen(toplevel->xdg_toplevel, on);
		return;
	}
	wlr_xwayland_surface_set_fullscreen(toplevel->xwayland_surface, on);
}

void viewport_view_close(struct viewport_toplevel *toplevel)
{
	if (toplevel->kind == VIEWPORT_VIEW_XDG) {
		wlr_xdg_toplevel_send_close(toplevel->xdg_toplevel);
		return;
	}
	wlr_xwayland_surface_close(toplevel->xwayland_surface);
}

/* Window geometry inside the surface. X11 windows have no decoration margin of
 * this kind, so the offset is always zero and the size is the surface's. */
struct wlr_box viewport_view_geometry(struct viewport_toplevel *toplevel)
{
	if (toplevel->kind == VIEWPORT_VIEW_XDG) {
		return toplevel->xdg_toplevel->base->geometry;
	}

	struct wlr_xwayland_surface *surface = toplevel->xwayland_surface;
	return (struct wlr_box){
		.x = 0,
		.y = 0,
		.width = surface->width,
		.height = surface->height,
	};
}

/* ------------------------------------------------------------------------
 * Shared lifecycle
 *
 * Both surface types join and leave the layout the same way, so the sequencing
 * that took several attempts to get right — staying hidden until placed, not
 * announcing focus before removal — lives in one place.
 * --------------------------------------------------------------------- */

void viewport_view_map(struct viewport_toplevel *toplevel)
{
	toplevel->mapped = true;
	wl_list_insert(&toplevel->server->toplevels, &toplevel->link);

	/* Stay invisible until the shell has placed us, or the window flashes at
	 * 0,0 for a frame before the first layout message lands. */
	wlr_scene_node_set_enabled(&toplevel->scene_tree->node, toplevel->has_box);

	viewport_ipc_notify_view_added(toplevel);
}

void viewport_view_unmap(struct viewport_toplevel *toplevel)
{
	struct viewport_server *server = toplevel->server;

	if (server->focused == toplevel) {
		/* Drop focus without announcing it: view.focused id=0 would reach the
		 * shell before view.removed, and the shell would no longer know that
		 * the window being removed had focus. Picking the next window is its
		 * decision, made when view.removed arrives. */
		server->focused = NULL;
		wlr_seat_keyboard_notify_clear_focus(server->seat);
	}

	viewport_ipc_notify_view_removed(toplevel);

	toplevel->mapped = false;
	wl_list_remove(&toplevel->link);
}
