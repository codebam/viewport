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

/* Same reason as the xdg path: a commit resets the buffers' destination size,
 * so a scaled X11 window would snap back to full size as soon as it painted. */
static void handle_surface_commit(struct wl_listener *listener, void *data)
{
	struct viewport_toplevel *toplevel =
		wl_container_of(listener, toplevel, commit);

	if (toplevel->scale > 0.0 && toplevel->scale < 1.0) {
		viewport_toplevel_apply_scale(toplevel);
	}
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
		return;
	}

	viewport_view_map(toplevel);
}

static void handle_unmap(struct wl_listener *listener, void *data)
{
	struct viewport_toplevel *toplevel =
		wl_container_of(listener, toplevel, unmap);

	if (toplevel->xwayland_surface->override_redirect) {
		wlr_scene_node_set_enabled(&toplevel->scene_tree->node, false);
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

	char command[96];
	snprintf(command, sizeof(command), "window.fullscreen.set %u %d",
		toplevel->id, wants ? 1 : 0);
	viewport_ipc_notify_shell_command(toplevel->server, command);
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

	toplevel->commit.notify = handle_surface_commit;
	wl_signal_add(&surface->surface->events.commit, &toplevel->commit);
	toplevel->map.notify = handle_map;
	wl_signal_add(&surface->surface->events.map, &toplevel->map);
	toplevel->unmap.notify = handle_unmap;
	wl_signal_add(&surface->surface->events.unmap, &toplevel->unmap);
}

static void handle_dissociate(struct wl_listener *listener, void *data)
{
	struct viewport_toplevel *toplevel =
		wl_container_of(listener, toplevel, dissociate);

	wl_list_remove(&toplevel->commit.link);
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
	wl_list_remove(&toplevel->request_configure.link);
	wl_list_remove(&toplevel->set_title.link);
	wl_list_remove(&toplevel->set_app_id.link);
	wl_list_remove(&toplevel->request_fullscreen.link);
	wl_list_remove(&toplevel->destroy.link);

	viewport_watchdog_disarm(toplevel);

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

	/* Clients find the X server through DISPLAY, so it has to be exported
	 * before anything is launched — and it is only known once Xwayland has
	 * started, which is why this is not set up front. */
	setenv("DISPLAY", server->xwayland->display_name, 1);
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

	server->new_xwayland_surface.notify = viewport_handle_new_xwayland_surface;
	wl_signal_add(&server->xwayland->events.new_surface,
		&server->new_xwayland_surface);
	server->xwayland_ready.notify = handle_ready;
	wl_signal_add(&server->xwayland->events.ready, &server->xwayland_ready);
}
