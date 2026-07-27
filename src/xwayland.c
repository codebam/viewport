/* SPDX-License-Identifier: MIT
 *
 * XWayland.
 *
 * X11 windows differ from Wayland ones in ways that matter here:
 *
 *   - A surface exists before it has a wl_surface. Xwayland creates the X11
 *     window first and associates a surface later, so mapping is a two-stage
 *     affair rather than a single map event.
 *   - Position and size are one operation, in absolute screen coordinates.
 *     There is no "the compositor decides where you are" — the window is told
 *     its place on the X screen, which is the output layout.
 *   - Override-redirect windows exist. Menus, tooltips and drag icons bypass
 *     the window manager entirely, must never be tiled, and have to be shown
 *     exactly where the client asked. Tiling them would break every X11 menu.
 *
 * Everything else — focus, workspaces, the tiling tree, the status bar — runs
 * through the same viewport_toplevel path as Wayland windows, via the
 * accessors in view.c.
 */
#define _POSIX_C_SOURCE 200809L

#include <stdlib.h>

#include <wlr/types/wlr_scene.h>
#include <wlr/util/log.h>
#include <wlr/xwayland.h>

#include "viewport.h"

/* Give the keyboard back after an unmanaged window that held it goes away.
 *
 * An X11 menu takes focus while it is up and has to hand it back, or the
 * session is left focused on a surface that no longer exists and typing goes
 * nowhere — which from the outside looks like the application has stopped
 * responding, because for keyboard purposes it has.
 *
 * Called from every way one can leave, not just unmap. A menu that is
 * destroyed or dissociated without unmapping first used to strand the focus:
 * wlroots clears the seat's pointer to the dead surface, so nothing is
 * dangling, but nothing hands the keyboard on either and the window underneath
 * never gets it back. */
static void restore_focus_from(struct viewport_toplevel *toplevel)
{
	struct viewport_server *server = toplevel->server;
	struct wlr_xwayland_surface *surface = toplevel->xwayland_surface;

	if (surface->surface == NULL ||
			server->seat->keyboard_state.focused_surface != surface->surface) {
		return;
	}

	if (server->config.debug) {
		wlr_log(WLR_DEBUG, "unmanaged X11 window gave the keyboard back to %s",
			server->focused != NULL ? "the focused window" : "the shell");
	}

	viewport_focus_restore(server);
}

static void handle_map(struct wl_listener *listener, void *data)
{
	struct viewport_toplevel *toplevel = wl_container_of(listener, toplevel, map);
	struct wlr_xwayland_surface *surface = toplevel->xwayland_surface;

	if (surface->override_redirect) {
		/* Unmanaged: an X11 menu or tooltip. It goes above the tiled windows
		 * at the coordinates the client chose, and is never told to move. */
		wlr_scene_node_set_position(&toplevel->scene_tree->node, surface->x,
			surface->y);
		wlr_scene_node_reparent(&toplevel->scene_tree->node,
			toplevel->server->layer_overlay);
		wlr_scene_node_set_enabled(&toplevel->scene_tree->node, true);

		/* Menus need the keyboard, not just the pointer.
		 *
		 * An X11 menu is an override-redirect window that takes focus and
		 * closes when it loses it. Left unfocused it sits there inert: Steam's
		 * menu appeared but nothing in it could be clicked, because the menu
		 * was waiting for a focus it never received. Only surfaces that ask are
		 * given it — a tooltip must not steal the keyboard. */
		if (wlr_xwayland_surface_override_redirect_wants_focus(surface)) {
			struct viewport_server *server = toplevel->server;
			struct wlr_keyboard *keyboard = wlr_seat_get_keyboard(server->seat);
			wlr_seat_keyboard_notify_enter(server->seat, surface->surface,
				keyboard != NULL ? keyboard->keycodes : NULL,
				keyboard != NULL ? keyboard->num_keycodes : 0,
				keyboard != NULL ? &keyboard->modifiers : NULL);
		}

		/* The keyboard is not the half that was missing. A menu opens under a
		 * pointer that has not moved, so nothing had sent it wl_pointer.enter
		 * and every click on it was delivered and then ignored. */
		viewport_cursor_rebase(toplevel->server);
		return;
	}

	viewport_view_map(toplevel);
}

static void handle_unmap(struct wl_listener *listener, void *data)
{
	struct viewport_toplevel *toplevel =
		wl_container_of(listener, toplevel, unmap);

	if (toplevel->xwayland_surface->override_redirect) {
		if (toplevel->scene_tree != NULL) {
			wlr_scene_node_set_enabled(&toplevel->scene_tree->node, false);
		}
		restore_focus_from(toplevel);
		viewport_cursor_rebase(toplevel->server);
		return;
	}

	viewport_view_unmap(toplevel);
}

/* An X11 client may ask to move or resize itself at any time. Tiled windows
 * are placed by the shell, so the request is answered with the geometry they
 * already have — refusing silently would leave the client waiting. */
static void handle_request_configure(struct wl_listener *listener, void *data)
{
	struct viewport_toplevel *toplevel =
		wl_container_of(listener, toplevel, request_configure);
	struct wlr_xwayland_surface_configure_event *event = data;
	struct wlr_xwayland_surface *surface = toplevel->xwayland_surface;

	if (surface->override_redirect || !toplevel->has_box) {
		/* Unmanaged windows, and managed ones the shell has not placed yet,
		 * get what they asked for. */
		wlr_xwayland_surface_configure(surface, event->x, event->y,
			event->width, event->height);
		if (surface->override_redirect) {
			wlr_scene_node_set_position(&toplevel->scene_tree->node, event->x,
				event->y);
		}
		return;
	}

	wlr_xwayland_surface_configure(surface, toplevel->box.x, toplevel->box.y,
		toplevel->box.width, toplevel->box.height);
}

/* An unmanaged surface moving itself. Submenus do this: the parent menu stays
 * put and the child appears beside it, at coordinates the client picks. Without
 * following them a submenu opens wherever its first placement happened to be. */
static void handle_set_geometry(struct wl_listener *listener, void *data)
{
	struct viewport_toplevel *toplevel =
		wl_container_of(listener, toplevel, set_geometry);
	struct wlr_xwayland_surface *surface = toplevel->xwayland_surface;

	if (surface->override_redirect && toplevel->scene_tree != NULL) {
		wlr_scene_node_set_position(&toplevel->scene_tree->node, surface->x,
			surface->y);
	}
}

static void handle_set_title(struct wl_listener *listener, void *data)
{
	struct viewport_toplevel *toplevel =
		wl_container_of(listener, toplevel, set_title);
	if (toplevel->mapped) {
		viewport_ipc_notify_view_props(toplevel);
	}
}

static void handle_set_class(struct wl_listener *listener, void *data)
{
	struct viewport_toplevel *toplevel =
		wl_container_of(listener, toplevel, set_app_id);
	if (toplevel->mapped) {
		viewport_ipc_notify_view_props(toplevel);
	}
}

static void handle_request_fullscreen(struct wl_listener *listener, void *data)
{
	struct viewport_toplevel *toplevel =
		wl_container_of(listener, toplevel, request_fullscreen);
	bool wants = toplevel->xwayland_surface->fullscreen;

	wlr_xwayland_surface_set_fullscreen(toplevel->xwayland_surface, wants);

	viewport_ipc_notify_fullscreen(toplevel, wants);
}

/* The wl_surface arrives after the X11 window. Only now can the surface be put
 * into the scene graph, so map/unmap are wired up here rather than at create. */
static void handle_associate(struct wl_listener *listener, void *data)
{
	struct viewport_toplevel *toplevel =
		wl_container_of(listener, toplevel, associate);
	struct wlr_xwayland_surface *surface = toplevel->xwayland_surface;

	struct wlr_scene_tree *parent = surface->override_redirect
		? toplevel->server->layer_overlay
		: toplevel->server->layer_apps;

	toplevel->scene_tree = wlr_scene_tree_create(parent);
	if (toplevel->scene_tree == NULL) {
		return;
	}
	toplevel->surface_tree = wlr_scene_subsurface_tree_create(
		toplevel->scene_tree, surface->surface);
	toplevel->scene_tree->node.data = toplevel;

	toplevel->map.notify = handle_map;
	wl_signal_add(&surface->surface->events.map, &toplevel->map);
	toplevel->unmap.notify = handle_unmap;
	wl_signal_add(&surface->surface->events.unmap, &toplevel->unmap);
}

static void handle_dissociate(struct wl_listener *listener, void *data)
{
	struct viewport_toplevel *toplevel =
		wl_container_of(listener, toplevel, dissociate);

	/* Before the listeners go: unmap may never arrive for a window that is
	 * torn down this way, and this is the last point at which the surface is
	 * still known. */
	if (toplevel->xwayland_surface->override_redirect) {
		restore_focus_from(toplevel);
	}

	wl_list_remove(&toplevel->map.link);
	wl_list_remove(&toplevel->unmap.link);

	if (toplevel->scene_tree != NULL) {
		wlr_scene_node_destroy(&toplevel->scene_tree->node);
		toplevel->scene_tree = NULL;
		toplevel->surface_tree = NULL;
	}
}

static void handle_destroy(struct wl_listener *listener, void *data)
{
	struct viewport_toplevel *toplevel =
		wl_container_of(listener, toplevel, destroy);

	wl_list_remove(&toplevel->associate.link);
	wl_list_remove(&toplevel->dissociate.link);
	wl_list_remove(&toplevel->set_geometry.link);
	wl_list_remove(&toplevel->request_configure.link);
	wl_list_remove(&toplevel->set_title.link);
	wl_list_remove(&toplevel->set_app_id.link);
	wl_list_remove(&toplevel->request_fullscreen.link);
	wl_list_remove(&toplevel->destroy.link);

	/* Leaving while still mapped.
	 *
	 * viewport_view_map() puts a managed window into server->toplevels and only
	 * viewport_view_unmap() takes it out again, so freeing here without that
	 * having run leaves a freed entry in the list — and server->focused, the
	 * foreign handle and any interactive drag all still pointing at it.
	 *
	 * wlroots does send unmap before destroy, so this is a guard rather than a
	 * fix for anything observed. It is here because the cost of being wrong is
	 * a use-after-free on the next window switch, and the check is one branch.
	 * Unmanaged windows never entered the list and never set `mapped`. */
	if (toplevel->mapped) {
		wlr_log(WLR_ERROR, "X11 window %u destroyed while still mapped",
			toplevel->id);
		viewport_view_unmap(toplevel);
	}

	viewport_watchdog_disarm(toplevel);
	viewport_foreign_capture_finish(toplevel);

	free(toplevel);
}

void viewport_handle_new_xwayland_surface(struct wl_listener *listener,
	void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, new_xwayland_surface);
	struct wlr_xwayland_surface *surface = data;

	struct viewport_toplevel *toplevel = calloc(1, sizeof(*toplevel));
	if (toplevel == NULL) {
		return;
	}

	toplevel->node.type = VIEWPORT_NODE_TOPLEVEL;
	toplevel->kind = VIEWPORT_VIEW_XWAYLAND;
	toplevel->server = server;
	toplevel->xwayland_surface = surface;
	toplevel->id = server->next_view_id++;
	surface->data = toplevel;

	/* The scene tree cannot be built yet: there is no wl_surface until the
	 * associate event. */
	toplevel->associate.notify = handle_associate;
	wl_signal_add(&surface->events.associate, &toplevel->associate);
	toplevel->dissociate.notify = handle_dissociate;
	wl_signal_add(&surface->events.dissociate, &toplevel->dissociate);
	toplevel->set_geometry.notify = handle_set_geometry;
	wl_signal_add(&surface->events.set_geometry, &toplevel->set_geometry);
	toplevel->request_configure.notify = handle_request_configure;
	wl_signal_add(&surface->events.request_configure,
		&toplevel->request_configure);
	toplevel->set_title.notify = handle_set_title;
	wl_signal_add(&surface->events.set_title, &toplevel->set_title);
	toplevel->set_app_id.notify = handle_set_class;
	wl_signal_add(&surface->events.set_class, &toplevel->set_app_id);
	toplevel->request_fullscreen.notify = handle_request_fullscreen;
	wl_signal_add(&surface->events.request_fullscreen,
		&toplevel->request_fullscreen);
	toplevel->destroy.notify = handle_destroy;
	wl_signal_add(&surface->events.destroy, &toplevel->destroy);
}

static void handle_ready(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, xwayland_ready);

	wlr_xwayland_set_seat(server->xwayland, server->seat);

	wlr_log(WLR_INFO, "XWayland ready on %s", server->xwayland->display_name);
}

void viewport_xwayland_init(struct viewport_server *server)
{
	/* Lazy: the X server is not started until an X11 client actually connects,
	 * so a session that never runs one pays nothing. */
	server->xwayland = wlr_xwayland_create(server->wl_display,
		server->compositor, true);
	if (server->xwayland == NULL) {
		wlr_log(WLR_INFO, "XWayland unavailable; X11 clients will not run");
		return;
	}

	/* Exported now, not when Xwayland reports ready.
	 *
	 * The X11 socket exists from this moment; starting lazily only defers
	 * running the server until something connects to it. But `ready` does not
	 * fire until that has happened, so setting DISPLAY there means the first
	 * client to launch finds no DISPLAY at all — it fails, and the *second*
	 * launch works because by then something else has woken Xwayland. Which is
	 * exactly how it looked from the outside. */
	setenv("DISPLAY", server->xwayland->display_name, 1);

	server->new_xwayland_surface.notify = viewport_handle_new_xwayland_surface;
	wl_signal_add(&server->xwayland->events.new_surface,
		&server->new_xwayland_surface);
	server->xwayland_ready.notify = handle_ready;
	wl_signal_add(&server->xwayland->events.ready, &server->xwayland_ready);
}
