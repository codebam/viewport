/* SPDX-License-Identifier: MIT
 *
 * Exposing the window list to outside tools.
 *
 * The shell already knows every window — it is drawing them. But nothing
 * *outside* the compositor does, and that is what taskbars, window switchers
 * (rofi -show window, wofi), screen-sharing pickers and `wlrctl` all need.
 * They bind wlr-foreign-toplevel-management-v1 and get told about each window
 * as it appears, along with its title, app id and state.
 *
 * The protocol is two-way: those tools may also ask for a window to be focused
 * or closed, which is how an alt-tab replacement written as an ordinary client
 * can work at all.
 *
 * Every window gets a handle regardless of type — the accessors in view.c mean
 * an X11 window is published exactly like a Wayland one.
 */
#define _POSIX_C_SOURCE 200809L

#include <stdio.h>

#include <wlr/types/wlr_ext_image_capture_source_v1.h>
#include <stdlib.h>

#include <wlr/types/wlr_ext_foreign_toplevel_list_v1.h>
#include <wlr/types/wlr_foreign_toplevel_management_v1.h>
#include <wlr/util/log.h>

#include "viewport.h"

struct viewport_foreign {
	struct wlr_foreign_toplevel_handle_v1 *handle;
	/* The newer, read-only list. Screen-share pickers are moving to it, and a
	 * client that binds it sees nothing at all if only the wlr protocol is
	 * published — so both are, and both describe the same windows. */
	struct wlr_ext_foreign_toplevel_handle_v1 *ext_handle;
	struct viewport_toplevel *toplevel;

	struct wl_listener request_activate;
	struct wl_listener request_close;
	struct wl_listener request_fullscreen;
};

static void handle_request_activate(struct wl_listener *listener, void *data)
{
	struct viewport_foreign *foreign =
		wl_container_of(listener, foreign, request_activate);

	/* The request names a seat, but there is only one here, so any request is
	 * for ours. */
	if (foreign->toplevel->mapped) {
		viewport_toplevel_focus(foreign->toplevel);
	}
}

static void handle_request_close(struct wl_listener *listener, void *data)
{
	struct viewport_foreign *foreign =
		wl_container_of(listener, foreign, request_close);
	viewport_view_close(foreign->toplevel);
}

static void handle_request_fullscreen(struct wl_listener *listener, void *data)
{
	struct viewport_foreign *foreign =
		wl_container_of(listener, foreign, request_fullscreen);
	struct wlr_foreign_toplevel_handle_v1_fullscreen_event *event = data;

	/* Fullscreen is the shell's decision — it owns the tiling tree and the bar
	 * — so the request is forwarded rather than applied here. The state comes
	 * back through viewport_view_set_fullscreen. */
	char command[96];
	snprintf(command, sizeof(command), "window.fullscreen.set %u %d",
		foreign->toplevel->id, event->fullscreen ? 1 : 0);
	viewport_ipc_notify_shell_command(foreign->toplevel->server, command);
}

void viewport_foreign_view_map(struct viewport_toplevel *toplevel)
{
	struct viewport_server *server = toplevel->server;
	if (server->foreign_toplevel_manager == NULL || toplevel->foreign != NULL) {
		return;
	}

	struct viewport_foreign *foreign = calloc(1, sizeof(*foreign));
	if (foreign == NULL) {
		return;
	}
	foreign->toplevel = toplevel;
	foreign->handle = wlr_foreign_toplevel_handle_v1_create(
		server->foreign_toplevel_manager);
	if (foreign->handle == NULL) {
		free(foreign);
		return;
	}

	foreign->request_activate.notify = handle_request_activate;
	wl_signal_add(&foreign->handle->events.request_activate,
		&foreign->request_activate);
	foreign->request_close.notify = handle_request_close;
	wl_signal_add(&foreign->handle->events.request_close,
		&foreign->request_close);
	foreign->request_fullscreen.notify = handle_request_fullscreen;
	wl_signal_add(&foreign->handle->events.request_fullscreen,
		&foreign->request_fullscreen);

	if (server->ext_foreign_toplevel_list != NULL) {
		const struct wlr_ext_foreign_toplevel_handle_v1_state state = {
			.title = viewport_view_title(toplevel),
			.app_id = viewport_view_app_id(toplevel),
		};
		foreign->ext_handle = wlr_ext_foreign_toplevel_handle_v1_create(
			server->ext_foreign_toplevel_list, &state);
	}

	toplevel->foreign = foreign;
	viewport_foreign_view_props(toplevel);
}

void viewport_foreign_view_unmap(struct viewport_toplevel *toplevel)
{
	struct viewport_foreign *foreign = toplevel->foreign;
	if (foreign == NULL) {
		return;
	}
	toplevel->foreign = NULL;

	wl_list_remove(&foreign->request_activate.link);
	wl_list_remove(&foreign->request_close.link);
	wl_list_remove(&foreign->request_fullscreen.link);

	if (foreign->ext_handle != NULL) {
		wlr_ext_foreign_toplevel_handle_v1_destroy(foreign->ext_handle);
	}
	wlr_foreign_toplevel_handle_v1_destroy(foreign->handle);
	free(foreign);
}

void viewport_foreign_view_props(struct viewport_toplevel *toplevel)
{
	struct viewport_foreign *foreign = toplevel->foreign;
	if (foreign == NULL) {
		return;
	}
	wlr_foreign_toplevel_handle_v1_set_title(foreign->handle,
		viewport_view_title(toplevel));
	wlr_foreign_toplevel_handle_v1_set_app_id(foreign->handle,
		viewport_view_app_id(toplevel));

	if (foreign->ext_handle != NULL) {
		const struct wlr_ext_foreign_toplevel_handle_v1_state state = {
			.title = viewport_view_title(toplevel),
			.app_id = viewport_view_app_id(toplevel),
		};
		wlr_ext_foreign_toplevel_handle_v1_update_state(foreign->ext_handle,
			&state);
	}
}

void viewport_foreign_view_state(struct viewport_toplevel *toplevel,
	bool activated, bool fullscreen)
{
	struct viewport_foreign *foreign = toplevel->foreign;
	if (foreign == NULL) {
		return;
	}
	wlr_foreign_toplevel_handle_v1_set_activated(foreign->handle, activated);
	wlr_foreign_toplevel_handle_v1_set_fullscreen(foreign->handle, fullscreen);
}

/* A screen-share picker asking to capture one window rather than a whole
 * output.
 *
 * ext-image-copy-capture will not let a client capture a toplevel on its own
 * say-so: the compositor is asked, and answers by handing back a capture
 * source, or by ignoring the request, which rejects it. Publishing the source
 * manager without answering is the worst of the three — the picker offers
 * windows, the choice is made, and the session dies with
 *
 *   invalid arguments for ext_image_copy_capture_manager_v1.create_session
 *
 * which reaches the browser as a bare NotAllowedError naming nothing.
 *
 * The source is the window's own scene node, so what gets captured is exactly
 * what is composited for that window and nothing behind it. It is made once
 * and kept: a picker that asks twice about the same window should not build a
 * second capture pipeline for it.
 *
 * The policy here is to say yes. The request has already been through the
 * portal, which ran its own chooser and got an answer from the person at the
 * keyboard; refusing afterwards would deny something already agreed to. */
static void handle_toplevel_capture_request(struct wl_listener *listener,
	void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, toplevel_capture_request);
	struct wlr_ext_foreign_toplevel_image_capture_source_manager_v1_request
		*request = data;

	struct viewport_toplevel *toplevel;
	wl_list_for_each(toplevel, &server->toplevels, link) {
		if (toplevel->foreign == NULL ||
				toplevel->foreign->ext_handle != request->toplevel_handle) {
			continue;
		}
		if (toplevel->capture_source == NULL) {
			toplevel->capture_source =
				wlr_ext_image_capture_source_v1_create_with_scene_node(
					&toplevel->scene_tree->node, server->wl_event_loop,
					server->allocator, server->renderer);
		}
		/* What wlroots will capture is the scene region this node reports,
		 * so report it: a window sharing the top left of the screen means
		 * these coordinates came back as zero, and there is no other way to
		 * tell that from the outside. */
		int lx = 0, ly = 0;
		bool on_screen = wlr_scene_node_coords(&toplevel->scene_tree->node,
			&lx, &ly);
		wlr_log(WLR_INFO,
			"capture requested for view %u at %d,%d (enabled=%d, clipped=%d)",
			toplevel->id, lx, ly, on_screen, toplevel->has_clip);

		if (toplevel->capture_source != NULL) {
			wlr_ext_foreign_toplevel_image_capture_source_manager_v1_request_accept(
				request, toplevel->capture_source);
		}
		return;
	}
}

void viewport_foreign_init(struct viewport_server *server)
{
	server->foreign_toplevel_manager =
		wlr_foreign_toplevel_manager_v1_create(server->wl_display);
	if (server->foreign_toplevel_manager == NULL) {
		wlr_log(WLR_ERROR, "foreign-toplevel unavailable; no external taskbars");
	}

	/* The successor protocol. It carries no requests — a client can see the
	 * windows but not act on them — so the older one stays for the taskbars
	 * that need to raise and close. */
	server->ext_foreign_toplevel_list =
		wlr_ext_foreign_toplevel_list_v1_create(server->wl_display, 1);

	/* Window capture. The manager is created here rather than beside the other
	 * capture globals in server.c because answering its requests means looking
	 * up a toplevel by its foreign handle, which is this file's business. */
	server->toplevel_capture =
		wlr_ext_foreign_toplevel_image_capture_source_manager_v1_create(
			server->wl_display, 1);
	if (server->toplevel_capture != NULL) {
		server->toplevel_capture_request.notify =
			handle_toplevel_capture_request;
		wl_signal_add(&server->toplevel_capture->events.new_request,
			&server->toplevel_capture_request);
	}
}
