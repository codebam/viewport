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
}
