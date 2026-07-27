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
#include <wlr/xwayland.h>
#include <wlr/util/log.h>

#include "viewport-view.h"
#include "viewport-input.h"
#include "viewport-output.h"
#include "viewport-shell.h"

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
	 * every resize.
	 *
	 * Not while the window has no surface. The shell keeps sending rects for a
	 * window it has not yet heard is gone, and configuring an X11 window whose
	 * client has already destroyed it produces the BadWindow errors xwm logs.
	 * Harmless — the X server discards them — but they are noise that would
	 * hide a real one. */
	if (toplevel->xwayland_surface->surface == NULL) {
		return;
	}

	wlr_xwayland_surface_configure(toplevel->xwayland_surface,
		toplevel->box.x, toplevel->box.y, width, height);
}

void viewport_view_set_activated(struct viewport_toplevel *toplevel,
	bool activated)
{
	if (toplevel->kind == VIEWPORT_VIEW_XDG) {
		wlr_xdg_toplevel_set_activated(toplevel->xdg_toplevel, activated);
	} else {
		wlr_xwayland_surface_activate(toplevel->xwayland_surface, activated);
	}
	/* Routing this through the same seam keeps an external switcher's idea of
	 * which window is focused in step with the seat's, without every caller
	 * having to remember to tell it. */
	viewport_foreign_view_state(toplevel, activated, toplevel->fullscreen);
}

void viewport_view_set_fullscreen(struct viewport_toplevel *toplevel, bool on)
{
	toplevel->fullscreen = on;

	if (toplevel->kind == VIEWPORT_VIEW_XDG) {
		wlr_xdg_toplevel_set_fullscreen(toplevel->xdg_toplevel, on);
	} else {
		wlr_xwayland_surface_set_fullscreen(toplevel->xwayland_surface, on);
	}
	viewport_foreign_view_state(toplevel, toplevel->server->focused == toplevel,
		on);
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

/* The size the window would like to be, used to place it when floating. Its
 * first buffer is normally already at that size, so this is what the client
 * chose for itself rather than anything the shell imposed. */
void viewport_view_natural_size(struct viewport_toplevel *toplevel, int *width,
	int *height)
{
	struct wlr_box geometry = viewport_view_geometry(toplevel);
	*width = geometry.width;
	*height = geometry.height;

	if (*width <= 0 || *height <= 0) {
		/* Nothing committed yet. A dialog with no size at all cannot be placed,
		 * so fall back to something usable rather than a zero-sized window. */
		*width = 640;
		*height = 480;
	}
}

/* Whether the window should float rather than join the tiling layout.
 *
 * Tiling a modal dialog or a colour picker is the wrong answer: it squeezes the
 * window it belongs to, and the dialog itself is usually not resizable, so it
 * ends up clipped inside a slot it cannot fill. The signals below are the ones
 * every window manager keys off, and the shell can still override the decision
 * per window afterwards. */
bool viewport_view_wants_floating(struct viewport_toplevel *toplevel)
{
	if (toplevel->kind == VIEWPORT_VIEW_XDG) {
		struct wlr_xdg_toplevel *xdg = toplevel->xdg_toplevel;

		/* A child of another toplevel is a dialog by definition. */
		if (xdg->parent != NULL) {
			return true;
		}

		/* Fixed size: the client is telling us it cannot be tiled. Both bounds
		 * must be set, or a window with only a minimum would qualify. */
		const struct wlr_xdg_toplevel_state *state = &xdg->current;
		if (state->min_width > 0 && state->min_height > 0 &&
				state->min_width == state->max_width &&
				state->min_height == state->max_height) {
			return true;
		}
		return false;
	}

	struct wlr_xwayland_surface *surface = toplevel->xwayland_surface;
	if (surface->modal || surface->parent != NULL) {
		return true;
	}

	static const enum wlr_xwayland_net_wm_window_type floating_types[] = {
		WLR_XWAYLAND_NET_WM_WINDOW_TYPE_DIALOG,
		WLR_XWAYLAND_NET_WM_WINDOW_TYPE_UTILITY,
		WLR_XWAYLAND_NET_WM_WINDOW_TYPE_TOOLBAR,
		WLR_XWAYLAND_NET_WM_WINDOW_TYPE_SPLASH,
	};
	for (size_t i = 0; i < sizeof(floating_types) / sizeof(floating_types[0]);
			i++) {
		if (wlr_xwayland_surface_has_window_type(surface, floating_types[i])) {
			return true;
		}
	}

	/* X11 states fixed size through size hints rather than the toplevel. */
	if (surface->size_hints != NULL &&
			surface->size_hints->min_width > 0 &&
			surface->size_hints->min_width == surface->size_hints->max_width &&
			surface->size_hints->min_height == surface->size_hints->max_height) {
		return true;
	}

	return false;
}

/* An X11 window the window manager does not manage: a menu, a tooltip, a drag
 * icon. They place themselves, they are never tiled, and they must never be
 * treated as focusable windows — activating one is a request the client did not
 * ask for and, for a menu, is enough to make it stop responding. */
bool viewport_view_is_unmanaged(struct viewport_toplevel *toplevel)
{
	return toplevel->kind == VIEWPORT_VIEW_XWAYLAND &&
		toplevel->xwayland_surface->override_redirect;
}

/* ------------------------------------------------------------------------
 * Shared lifecycle
 *
 * Both surface types join and leave the layout the same way, so the sequencing
 * that took several attempts to get right — staying hidden until placed, not
 * announcing focus before removal — lives in one place.
 * --------------------------------------------------------------------- */

/* Whether the client has asked to be fullscreen, however it asked. */
static bool view_requested_fullscreen(struct viewport_toplevel *toplevel)
{
	if (toplevel->kind == VIEWPORT_VIEW_XDG) {
		return toplevel->xdg_toplevel->requested.fullscreen;
	}
	return toplevel->xwayland_surface->fullscreen;
}

void viewport_view_map(struct viewport_toplevel *toplevel)
{
	struct viewport_server *server = toplevel->server;

	toplevel->mapped = true;
	wl_list_insert(&toplevel->server->toplevels, &toplevel->link);

	/* Stay invisible until the shell has placed us, or the window flashes at
	 * 0,0 for a frame before the first layout message lands. */
	wlr_scene_node_set_enabled(&toplevel->scene_tree->node, toplevel->has_box);

	/* Before the first frame, so the client's very first buffer is already at
	 * the right scale rather than being corrected after it is visible. */
	viewport_surface_update_scale(server, viewport_view_surface(toplevel));

	viewport_ipc_notify_view_added(toplevel);
	viewport_foreign_view_map(toplevel);

	/* A client that asked to be fullscreen before it had a window.
	 *
	 * xdg_toplevel.set_fullscreen is answered the moment it arrives, and it
	 * usually arrives before the first commit — SDL sets it while creating the
	 * surface, so every game does this. The compositor duly forwarded
	 * window.fullscreen.set to the shell, which looked the view up, had never
	 * heard of it, and dropped it: view.added is not sent until the surface
	 * maps, which is later. The request was acknowledged and then lost, and the
	 * game opened windowed.
	 *
	 * That is not only a cosmetic problem. A window that is not fullscreen
	 * cannot be scanned out directly — wlroots only tries when the whole render
	 * list is one entry — so the compositor was compositing every frame of a
	 * game that had asked for the fast path.
	 *
	 * Re-sent here, after view.added, when the client still wants it. */
	if (view_requested_fullscreen(toplevel)) {
		toplevel->fullscreen = true;
		viewport_ipc_notify_fullscreen(toplevel, true);
	}

	/* If the shell never answers this, the window is placed without it. */
	viewport_watchdog_arm(toplevel);

	/* What is under the pointer just changed without the pointer moving. */
	viewport_cursor_rebase(server);
}

void viewport_view_unmap(struct viewport_toplevel *toplevel)
{
	struct viewport_server *server = toplevel->server;

	/* An interactive drag holds a bare pointer to its window. Closing the
	 * window mid-drag — a dialog dismissing itself, a crash — would leave that
	 * pointer dangling and the next pointer motion reading freed memory. */
	if (server->resizing == toplevel) {
		server->resizing = NULL;
	}
	if (server->moving == toplevel) {
		server->moving = NULL;
	}

	if (server->focused == toplevel) {
		/* Drop focus without announcing it: view.focused id=0 would reach the
		 * shell before view.removed, and the shell would no longer know that
		 * the window being removed had focus. Picking the next window is its
		 * decision, made when view.removed arrives. */
		server->focused = NULL;
		wlr_seat_keyboard_notify_clear_focus(server->seat);
	}

	viewport_ipc_notify_view_removed(toplevel);

	viewport_watchdog_disarm(toplevel);
	viewport_foreign_view_unmap(toplevel);

	toplevel->mapped = false;
	wl_list_remove(&toplevel->link);

	/* And again on the way out: whatever was behind this window is under the
	 * pointer now and has heard nothing about it. */
	viewport_cursor_rebase(server);
}
