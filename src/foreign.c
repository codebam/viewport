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

#include <string.h>

#include <wlr/types/wlr_ext_image_capture_source_v1.h>
#include <stdlib.h>

#include <wlr/types/wlr_ext_foreign_toplevel_list_v1.h>
#include <wlr/types/wlr_foreign_toplevel_management_v1.h>
#include <wlr/types/wlr_scene.h>
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
	viewport_ipc_notify_fullscreen(foreign->toplevel, event->fullscreen);
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

	struct viewport_output *output;
	wl_list_for_each(output, &server->outputs, link) {
		if (!toplevel->has_box) {
			wlr_foreign_toplevel_handle_v1_output_enter(foreign->handle,
				output->wlr_output);
			continue;
		}
		struct wlr_box output_box;
		wlr_output_layout_get_box(server->output_layout, output->wlr_output,
			&output_box);
		struct wlr_box intersection;
		if (wlr_box_intersection(&intersection, &output_box, &toplevel->box)) {
			wlr_foreign_toplevel_handle_v1_output_enter(foreign->handle,
				output->wlr_output);
		}
	}
}

void viewport_foreign_view_unmap(struct viewport_toplevel *toplevel)
{
	/* Before the early return: a window may have been captured without ever
	 * getting a foreign handle, and the capture scene is ours to free either
	 * way. */
	viewport_foreign_capture_finish(toplevel);

	struct viewport_foreign *foreign = toplevel->foreign;
	if (foreign == NULL) {
		return;
	}
	toplevel->foreign = NULL;

	struct viewport_output *output;
	wl_list_for_each(output, &toplevel->server->outputs, link) {
		wlr_foreign_toplevel_handle_v1_output_leave(foreign->handle,
			output->wlr_output);
	}

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

static void handle_capture_source_destroy(struct wl_listener *listener,
	void *data)
{
	struct viewport_toplevel *toplevel =
		wl_container_of(listener, toplevel, capture_source_destroy);

	/* wlroots owns the source and frees it when the scene node it was built
	 * from goes away — which happens when the client destroys its surface,
	 * before this window is unmapped. Forget it here or the teardown below
	 * hands out a dangling pointer to the next picker. */
	wl_list_remove(&toplevel->capture_source_destroy.link);
	toplevel->capture_source = NULL;
}

/* Trim the capture to the window, not the surface.
 *
 * A client drawing its own decorations paints shadows and resize handles
 * outside the window it appears to be, and the capture is aimed at the whole
 * surface, so without this the viewer gets the window adrift in a transparent
 * margin — 26x23 of it for Firefox — and a stream larger than the window in
 * both dimensions. xdg_surface.geometry marks the real window inside that
 * larger surface, and it is already in root-surface coordinates, which is what
 * the clip wants.
 *
 * Only for xdg: an X11 window's geometry is its surface, so there is nothing
 * to trim and clipping it would only be a way to get it wrong.
 *
 * Called on every commit, because the margin is not fixed — it changes as the
 * client resizes. The comparison matters: re-clipping damages the capture
 * scene, and doing that per commit for a clip that has not moved would have
 * the capture re-rendering for no reason. */
void viewport_foreign_capture_update(struct viewport_toplevel *toplevel)
{
	if (toplevel->capture_tree == NULL ||
			toplevel->kind != VIEWPORT_VIEW_XDG) {
		return;
	}

	struct wlr_box geo = viewport_view_geometry(toplevel);
	if (geo.width <= 0 || geo.height <= 0) {
		/* Nothing committed yet. A zero-sized clip disables clipping rather
		 * than hiding everything, so it would show the margin instead — but it
		 * would also be recorded as applied, and the real geometry that
		 * follows might compare equal to it. Wait for a real one. */
		return;
	}

	if (memcmp(&geo, &toplevel->capture_clip, sizeof(geo)) == 0) {
		return;
	}
	toplevel->capture_clip = geo;

	wlr_scene_subsurface_tree_set_clip(&toplevel->capture_tree->node, &geo);
}

/* Tear down the capture pipeline built by the request handler below. Called
 * when the window goes away; the picker's session ends with it. */
void viewport_foreign_capture_finish(struct viewport_toplevel *toplevel)
{
	toplevel->capture_tree = NULL;
	toplevel->capture_clip = (struct wlr_box){0};

	if (toplevel->capture_scene == NULL) {
		return;
	}
	struct wlr_scene *scene = toplevel->capture_scene;
	toplevel->capture_scene = NULL;

	/* Destroying the root takes the scene's capture output with it, then the
	 * surface tree beneath it; that second step is what destroys the source
	 * and fires the listener above. */
	wlr_scene_node_destroy(&scene->tree.node);

	if (toplevel->capture_source != NULL) {
		wl_list_remove(&toplevel->capture_source_destroy.link);
		toplevel->capture_source = NULL;
	}
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
 * The capture gets a scene of its own containing one thing: this window's
 * surface. It cannot share the layout's tree, for two reasons, both of them
 * in wlroots' scene.c:
 *
 *  - source_render() parks a scene output over the node's extents and renders
 *    THE WHOLE SCENE through it. Aimed at the layout tree that means every
 *    window stacked above this one, and the shell behind it, land in the
 *    capture. A scene holding nothing else has nothing else to leak.
 *  - the output's mode is set from those extents on every render, and the
 *    scrolling layout re-clips and re-scales the window each frame, so the
 *    extents move constantly. Each move reallocates the capture swapchain and
 *    re-sends buffer constraints, and the client spends its time renegotiating
 *    instead of receiving frames — the freeze. An unclipped, unscaled surface
 *    tree only changes size when the client itself resizes.
 *
 * Made once and kept: a picker that asks twice about the same window should
 * not build a second capture pipeline for it.
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

		struct wlr_surface *surface = viewport_view_surface(toplevel);
		if (surface == NULL) {
			/* Nothing to capture yet. Rejecting is the honest answer. */
			return;
		}

		if (toplevel->capture_source == NULL) {
			viewport_foreign_capture_finish(toplevel);

			toplevel->capture_scene = wlr_scene_create();
			if (toplevel->capture_scene == NULL) {
				return;
			}
			struct wlr_scene_tree *tree = wlr_scene_subsurface_tree_create(
				&toplevel->capture_scene->tree, surface);
			if (tree == NULL) {
				wlr_scene_node_destroy(&toplevel->capture_scene->tree.node);
				toplevel->capture_scene = NULL;
				return;
			}

			toplevel->capture_source =
				wlr_ext_image_capture_source_v1_create_with_scene_node(
					&tree->node, server->wl_event_loop, server->allocator,
					server->renderer);
			if (toplevel->capture_source == NULL) {
				wlr_scene_node_destroy(&toplevel->capture_scene->tree.node);
				toplevel->capture_scene = NULL;
				return;
			}
			toplevel->capture_source_destroy.notify =
				handle_capture_source_destroy;
			wl_signal_add(&toplevel->capture_source->events.destroy,
				&toplevel->capture_source_destroy);

			toplevel->capture_tree = tree;
			viewport_foreign_capture_update(toplevel);
		}

		/* The capture is aimed at the surface, not at the layout, so what
		 * matters is the surface and the window inside it — between them they
		 * decide the stream's resolution, and neither is visible from outside.
		 * A window narrower than its surface is a client drawing its own
		 * shadows; the two being equal is server-side decorations or X11. */
		wlr_log(WLR_INFO,
			"capture requested for view %u: surface %dx%d, window %dx%d",
			toplevel->id, surface->current.width, surface->current.height,
			toplevel->capture_clip.width, toplevel->capture_clip.height);

		wlr_ext_foreign_toplevel_image_capture_source_manager_v1_request_accept(
			request, toplevel->capture_source);
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
