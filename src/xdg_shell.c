/* SPDX-License-Identifier: MIT
 *
 * xdg-shell glue.
 *
 * The unusual part: this compositor has no layout policy of its own. Every
 * toplevel is parked disabled until the web shell tells us, over IPC, which
 * screen rect its DOM has reserved for that view. Placement is JS's job; ours
 * is to make the client's dma-buf land exactly there.
 */
#define _POSIX_C_SOURCE 200809L

#include <stdlib.h>
#include <stdio.h>
#include <string.h>

#include <wlr/types/wlr_output.h>
#include <wlr/types/wlr_scene.h>
#include <wlr/types/wlr_xdg_activation_v1.h>
#include <wlr/types/wlr_xdg_decoration_v1.h>
#include <wlr/util/log.h>

#include "viewport.h"

static void handle_toplevel_map(struct wl_listener *listener, void *data)
{
	struct viewport_toplevel *toplevel = wl_container_of(listener, toplevel, map);

	viewport_view_map(toplevel);

	if (toplevel->server->config.debug) {
		wlr_log(WLR_DEBUG, "view %u mapped (has_box=%d -> visible=%d)",
			toplevel->id, toplevel->has_box, toplevel->has_box);
	}

}

static void handle_toplevel_unmap(struct wl_listener *listener, void *data)
{
	struct viewport_toplevel *toplevel =
		wl_container_of(listener, toplevel, unmap);
	viewport_view_unmap(toplevel);
}

static void handle_toplevel_commit(struct wl_listener *listener, void *data)
{
	struct viewport_toplevel *toplevel =
		wl_container_of(listener, toplevel, commit);

	/* The decoration margin is not fixed: it changes as the client resizes, and
	 * on the first commits it is not known at all. Re-apply the placement on
	 * every commit so the window stays pinned to its rect instead of drifting
	 * by whatever the margin happened to be when the shell last spoke. */
	if (toplevel->has_box && !toplevel->xdg_toplevel->base->initial_commit) {
		/* Position the container directly at the assigned rect.
		 *
		 * No decoration offset is subtracted here. wlr_scene_xdg_surface_create
		 * documents that "the origin of the returned scene-graph node will
		 * match the top-left corner of the xdg_surface window geometry" — it
		 * already compensates. Subtracting the offset again pushed every
		 * client-side-decorated window up and left by exactly its shadow
		 * margin: 26x23 for Firefox, less for Chromium, none for a client
		 * using server-side decorations. */
		wlr_scene_node_set_position(&toplevel->scene_tree->node,
			toplevel->box.x, toplevel->box.y);
	}

	if (toplevel->xdg_toplevel->base->initial_commit) {
		/* The protocol requires a configure in response to the first commit.
		 * Send the shell's rect if we already have one; otherwise 0x0 lets the
		 * client pick, and we resize it the moment JS reports a viewport. */
		if (toplevel->has_box) {
			wlr_xdg_toplevel_set_size(toplevel->xdg_toplevel,
				toplevel->box.width, toplevel->box.height);
		} else {
			wlr_xdg_toplevel_set_size(toplevel->xdg_toplevel, 0, 0);
		}
	}
}

static void handle_toplevel_destroy(struct wl_listener *listener, void *data)
{
	struct viewport_toplevel *toplevel =
		wl_container_of(listener, toplevel, destroy);

	wl_list_remove(&toplevel->map.link);
	wl_list_remove(&toplevel->unmap.link);
	wl_list_remove(&toplevel->commit.link);
	wl_list_remove(&toplevel->destroy.link);
	wl_list_remove(&toplevel->set_title.link);
	wl_list_remove(&toplevel->set_app_id.link);
	wl_list_remove(&toplevel->request_fullscreen.link);

	/* The surface tree goes with the xdg_surface, but the container tree around
	 * it is ours and nothing else frees it. Without this every window ever
	 * opened leaves an empty tree behind in layer_apps for the life of the
	 * session. */
	if (toplevel->scene_tree != NULL) {
		wlr_scene_node_destroy(&toplevel->scene_tree->node);
		toplevel->scene_tree = NULL;
	}

	free(toplevel);
}

static void handle_toplevel_set_title(struct wl_listener *listener, void *data)
{
	struct viewport_toplevel *toplevel =
		wl_container_of(listener, toplevel, set_title);
	if (toplevel->mapped) {
		viewport_ipc_notify_view_props(toplevel);
	}
}

static void handle_toplevel_set_app_id(struct wl_listener *listener, void *data)
{
	struct viewport_toplevel *toplevel =
		wl_container_of(listener, toplevel, set_app_id);
	if (toplevel->mapped) {
		viewport_ipc_notify_view_props(toplevel);
	}
}

/* A client asking to go fullscreen — the button on a YouTube video, or any
 * app's own fullscreen control — arrives as xdg_toplevel.set_fullscreen.
 *
 * Ignoring it means the button appears to do nothing at all. The compositor
 * must acknowledge the request, and the shell has to lay the window out
 * fullscreen, so this both acks and forwards. */
static void handle_request_fullscreen(struct wl_listener *listener, void *data)
{
	struct viewport_toplevel *toplevel =
		wl_container_of(listener, toplevel, request_fullscreen);
	bool wants = toplevel->xdg_toplevel->requested.fullscreen;

	/* The protocol requires a response whether or not we honour it. */
	wlr_xdg_toplevel_set_fullscreen(toplevel->xdg_toplevel, wants);

	char command[96];
	snprintf(command, sizeof(command), "window.fullscreen.set %u %d",
		toplevel->id, wants ? 1 : 0);
	viewport_ipc_notify_shell_command(toplevel->server, command);
}

void viewport_handle_new_xdg_toplevel(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, new_xdg_toplevel);
	struct wlr_xdg_toplevel *xdg_toplevel = data;

	struct viewport_toplevel *toplevel = calloc(1, sizeof(*toplevel));
	if (toplevel == NULL) {
		return;
	}

	toplevel->node.type = VIEWPORT_NODE_TOPLEVEL;
	toplevel->kind = VIEWPORT_VIEW_XDG;
	toplevel->server = server;
	toplevel->xdg_toplevel = xdg_toplevel;
	toplevel->id = server->next_view_id++;

	toplevel->scene_tree = wlr_scene_tree_create(server->layer_apps);
	if (toplevel->scene_tree == NULL) {
		free(toplevel);
		return;
	}
	toplevel->surface_tree = wlr_scene_xdg_surface_create(toplevel->scene_tree,
		xdg_toplevel->base);
	if (toplevel->surface_tree == NULL) {
		wlr_scene_node_destroy(&toplevel->scene_tree->node);
		free(toplevel);
		return;
	}

	/* Hit-testing walks up from the scene node it lands on until it finds a
	 * node carrying a back-pointer. Only this container gets one. */
	toplevel->scene_tree->node.data = toplevel;
	/* Popups resolve their parent through this. */
	xdg_toplevel->base->data = toplevel->scene_tree;

	toplevel->map.notify = handle_toplevel_map;
	wl_signal_add(&xdg_toplevel->base->surface->events.map, &toplevel->map);
	toplevel->unmap.notify = handle_toplevel_unmap;
	wl_signal_add(&xdg_toplevel->base->surface->events.unmap, &toplevel->unmap);
	toplevel->commit.notify = handle_toplevel_commit;
	wl_signal_add(&xdg_toplevel->base->surface->events.commit,
		&toplevel->commit);
	toplevel->destroy.notify = handle_toplevel_destroy;
	wl_signal_add(&xdg_toplevel->events.destroy, &toplevel->destroy);
	toplevel->set_title.notify = handle_toplevel_set_title;
	wl_signal_add(&xdg_toplevel->events.set_title, &toplevel->set_title);
	toplevel->set_app_id.notify = handle_toplevel_set_app_id;
	wl_signal_add(&xdg_toplevel->events.set_app_id, &toplevel->set_app_id);
	toplevel->request_fullscreen.notify = handle_request_fullscreen;
	wl_signal_add(&xdg_toplevel->events.request_fullscreen,
		&toplevel->request_fullscreen);
}

/* Server-side decorations.
 *
 * The shell already draws a titlebar around every window. A client drawing its
 * own on top of that is the duplicate-titlebar effect — and the client's
 * decorations also inflate its surface beyond its window geometry, which is
 * what throws off the placement the shell computed.
 *
 * xdg-decoration is only a request: clients may refuse, and GTK in particular
 * insists on drawing its own. Nothing here can force them. */
static void handle_decoration_destroy(struct wl_listener *listener, void *data)
{
	struct viewport_decoration *decoration =
		wl_container_of(listener, decoration, destroy);
	wl_list_remove(&decoration->request_mode.link);
	wl_list_remove(&decoration->surface_commit.link);
	wl_list_remove(&decoration->destroy.link);
	free(decoration);
}

static void handle_decoration_request_mode(struct wl_listener *listener,
	void *data)
{
	struct viewport_decoration *decoration =
		wl_container_of(listener, decoration, request_mode);
	struct viewport_server *server = decoration->server;

	enum wlr_xdg_toplevel_decoration_v1_mode mode =
		server->config.server_decorations
			? WLR_XDG_TOPLEVEL_DECORATION_V1_MODE_SERVER_SIDE
			: WLR_XDG_TOPLEVEL_DECORATION_V1_MODE_CLIENT_SIDE;

	/* set_mode schedules a configure, and scheduling one before the
	 * xdg_surface has completed its first commit trips
	 *   wlr_xdg_surface_schedule_configure: Assertion `surface->initialized'
	 * and aborts the compositor. Wait for the surface to be initialized; the
	 * commit handler below re-runs this at exactly that point. */
	if (!decoration->decoration->toplevel->base->initialized) {
		return;
	}

	wlr_xdg_toplevel_decoration_v1_set_mode(decoration->decoration, mode);
}

static void handle_decoration_surface_commit(struct wl_listener *listener,
	void *data)
{
	struct viewport_decoration *decoration =
		wl_container_of(listener, decoration, surface_commit);

	if (decoration->decoration->toplevel->base->initial_commit) {
		handle_decoration_request_mode(&decoration->request_mode, NULL);
	}
}

void viewport_handle_new_decoration(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, new_decoration);
	struct wlr_xdg_toplevel_decoration_v1 *wlr_decoration = data;

	struct viewport_decoration *decoration = calloc(1, sizeof(*decoration));
	if (decoration == NULL) {
		return;
	}
	decoration->server = server;
	decoration->decoration = wlr_decoration;

	decoration->request_mode.notify = handle_decoration_request_mode;
	wl_signal_add(&wlr_decoration->events.request_mode,
		&decoration->request_mode);
	decoration->destroy.notify = handle_decoration_destroy;
	wl_signal_add(&wlr_decoration->events.destroy, &decoration->destroy);

	/* Announce the mode at the first commit rather than waiting to be asked:
	 * clients may attach a buffer without ever sending a request, and by then
	 * the surface is initialized so scheduling a configure is legal. */
	decoration->surface_commit.notify = handle_decoration_surface_commit;
	wl_signal_add(&wlr_decoration->toplevel->base->surface->events.commit,
		&decoration->surface_commit);

	/* A client may create the decoration object *after* its surface is already
	 * initialized, in which case initial_commit has come and gone and the
	 * commit handler above would never fire — leaving the mode unset and the
	 * client drawing its own titlebar. Apply it now in that case. */
	if (wlr_decoration->toplevel->base->initialized) {
		handle_decoration_request_mode(&decoration->request_mode, NULL);
	}

	wlr_log(WLR_DEBUG, "decoration requested for toplevel (initialized=%d)",
		wlr_decoration->toplevel->base->initialized);
}

/* A popup is not shown until the compositor answers its first commit with a
 * configure. Skipping that is why Firefox's menus and right-click menus never
 * appeared: the client created the popup, waited to be told where it could go,
 * and gave up. */
/* Re-run the positioner against the output the popup's root window is on. */
static void unconstrain_popup(struct viewport_popup *popup);

static void handle_popup_reposition(struct wl_listener *listener, void *data)
{
	struct viewport_popup *popup = wl_container_of(listener, popup, reposition);

	/* A client may replace a popup's positioner after creation — Firefox
	 * creates a menu at a placeholder size and then repositions it once it
	 * knows how big its contents are. Ignoring this leaves the popup stuck at
	 * that placeholder, which is why menus appeared far too small to hold
	 * anything: an 18x70 request was the placeholder, not the menu. */
	unconstrain_popup(popup);
}

static void handle_popup_commit(struct wl_listener *listener, void *data)
{
	struct viewport_popup *popup = wl_container_of(listener, popup, commit);

	if (!popup->xdg_popup->base->initial_commit) {
		return;
	}

	unconstrain_popup(popup);
}

static void unconstrain_popup(struct viewport_popup *popup)
{
	/* Keep the menu on screen: a dropdown near the bottom of a window should
	 * flip upwards rather than run off the output.
	 *
	 * The box must be the output the window is on, expressed in the parent
	 * *surface's* coordinate space. Two mistakes here both make menus come out
	 * too small to hold their contents, because a client that cannot fit a
	 * popup in the space offered is allowed to shrink it rather than flip it:
	 *
	 *   - passing the whole multi-monitor layout instead of one output, which
	 *     describes space that does not exist beside this window;
	 *   - converting with the window rect alone. The popup's origin is the
	 *     parent surface, not its window geometry, and for a client that draws
	 *     its own decorations those differ by the shadow margin. */
	struct viewport_toplevel *toplevel = popup->toplevel;
	if (toplevel != NULL) {
		struct wlr_output *wlr_output = wlr_output_layout_output_at(
			toplevel->server->output_layout,
			toplevel->box.x + toplevel->box.width / 2,
			toplevel->box.y + toplevel->box.height / 2);

		struct wlr_box output_box;
		wlr_output_layout_get_box(toplevel->server->output_layout, wlr_output,
			&output_box);

		struct wlr_box geo = toplevel->xdg_toplevel->base->geometry;
		output_box.x -= toplevel->box.x - geo.x;
		output_box.y -= toplevel->box.y - geo.y;

		struct wlr_box before = popup->xdg_popup->scheduled.geometry;
		wlr_xdg_popup_unconstrain_from_box(popup->xdg_popup, &output_box);
		struct wlr_box after = popup->xdg_popup->scheduled.geometry;

		if (toplevel->server->config.debug) {
			wlr_log(WLR_DEBUG,
				"popup want %d,%d %dx%d -> %d,%d %dx%d | box %d,%d %dx%d "
				"| parent box %d,%d %dx%d geo %d,%d",
				before.x, before.y, before.width, before.height,
				after.x, after.y, after.width, after.height,
				output_box.x, output_box.y, output_box.width,
				output_box.height, toplevel->box.x, toplevel->box.y,
				toplevel->box.width, toplevel->box.height, geo.x, geo.y);
		}
		return;
	}

	/* A popup whose parent is another popup — a submenu — still has to be
	 * unconstrained, against the same output as the toplevel at the root of
	 * the chain. Walking up to find it is what sway does; skipping it leaves
	 * nested menus unconstrained and free to come out the wrong size. */
	struct wlr_xdg_popup *root = popup->xdg_popup;
	struct viewport_toplevel *owner = NULL;
	int offset_x = 0, offset_y = 0;

	while (root != NULL) {
		struct wlr_xdg_surface *parent =
			wlr_xdg_surface_try_from_wlr_surface(root->parent);
		if (parent == NULL) {
			break;
		}
		/* Each hop adds the popup's own offset within its parent. */
		offset_x += root->current.geometry.x;
		offset_y += root->current.geometry.y;

		if (parent->role == WLR_XDG_SURFACE_ROLE_TOPLEVEL) {
			struct wlr_scene_tree *tree = parent->data;
			struct viewport_node *tagged =
				tree != NULL ? tree->node.data : NULL;
			if (tagged != NULL && tagged->type == VIEWPORT_NODE_TOPLEVEL) {
				owner = (struct viewport_toplevel *)tagged;
			}
			break;
		}
		root = parent->popup;
	}

	if (owner != NULL) {
		struct wlr_output *wlr_output = wlr_output_layout_output_at(
			owner->server->output_layout,
			owner->box.x + owner->box.width / 2,
			owner->box.y + owner->box.height / 2);

		struct wlr_box output_box;
		wlr_output_layout_get_box(owner->server->output_layout, wlr_output,
			&output_box);

		struct wlr_box geo = owner->xdg_toplevel->base->geometry;
		output_box.x -= owner->box.x - geo.x + offset_x;
		output_box.y -= owner->box.y - geo.y + offset_y;

		struct wlr_box before = popup->xdg_popup->scheduled.geometry;
		wlr_xdg_popup_unconstrain_from_box(popup->xdg_popup, &output_box);

		if (owner->server->config.debug) {
			struct wlr_box after = popup->xdg_popup->scheduled.geometry;
			wlr_log(WLR_DEBUG,
				"nested popup want %dx%d -> %dx%d | box %d,%d %dx%d",
				before.width, before.height, after.width, after.height,
				output_box.x, output_box.y, output_box.width,
				output_box.height);
		}
		return;
	}

	wlr_xdg_surface_schedule_configure(popup->xdg_popup->base);
}

static void handle_popup_destroy(struct wl_listener *listener, void *data)
{
	struct viewport_popup *popup = wl_container_of(listener, popup, destroy);
	wl_list_remove(&popup->commit.link);
	wl_list_remove(&popup->reposition.link);
	wl_list_remove(&popup->destroy.link);
	free(popup);
}

/* A client asking for another window to be focused — following a link, or an
 * application raising its existing instance instead of starting a second one.
 * The token is validated by wlroots; all that is left is to honour it. */
void viewport_handle_request_activate(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, request_activate);
	const struct wlr_xdg_activation_v1_request_activate_event *event = data;

	struct viewport_toplevel *toplevel;
	wl_list_for_each(toplevel, &server->toplevels, link) {
		if (viewport_view_surface(toplevel) != event->surface) {
			continue;
		}
		if (toplevel->mapped) {
			viewport_toplevel_focus(toplevel);
		}
		return;
	}
}

void viewport_handle_new_xdg_popup(struct wl_listener *listener, void *data)
{
	struct wlr_xdg_popup *xdg_popup = data;

	/* Popups ride in the parent's tree so they inherit its position and are
	 * stacked with it — but in the *container*, not the clipped surface tree,
	 * so a menu may extend past the window edge. */
	struct wlr_xdg_surface *parent =
		wlr_xdg_surface_try_from_wlr_surface(xdg_popup->parent);
	if (parent == NULL || parent->data == NULL) {
		wlr_log(WLR_ERROR, "popup with no positioned parent, dropping");
		return;
	}

	struct wlr_scene_tree *parent_tree = parent->data;
	xdg_popup->base->data = wlr_scene_xdg_surface_create(parent_tree,
		xdg_popup->base);

	struct viewport_popup *popup = calloc(1, sizeof(*popup));
	if (popup == NULL) {
		return;
	}
	popup->xdg_popup = xdg_popup;
	/* Only a toplevel's container carries a tag; a popup's parent may itself
	 * be a popup, in which case there is no box to unconstrain against. */
	struct viewport_node *tagged = parent_tree->node.data;
	if (tagged != NULL && tagged->type == VIEWPORT_NODE_TOPLEVEL) {
		popup->toplevel = (struct viewport_toplevel *)tagged;
	}

	popup->commit.notify = handle_popup_commit;
	wl_signal_add(&xdg_popup->base->surface->events.commit, &popup->commit);
	popup->reposition.notify = handle_popup_reposition;
	wl_signal_add(&xdg_popup->events.reposition, &popup->reposition);
	popup->destroy.notify = handle_popup_destroy;
	wl_signal_add(&xdg_popup->events.destroy, &popup->destroy);
}

/* Crop the surface to the part of it the shell says is on screen.
 *
 * The clip is expressed in the root surface's coordinate space, whose origin
 * sits at the surface's top-left — which is not the window origin, since a
 * client drawing its own shadows puts the window geometry somewhere inside a
 * larger surface. So the requested region is rebased through the geometry
 * offset, the same correction that placement already makes.
 *
 * Only the surface tree is clipped, never the container: popups are children of
 * the container and a menu is entitled to extend past the window it belongs to. */
static void apply_clip(struct viewport_toplevel *toplevel)
{
	if (toplevel->surface_tree == NULL) {
		return;
	}

	if (!toplevel->has_clip) {
		wlr_scene_subsurface_tree_set_clip(&toplevel->surface_tree->node, NULL);
		toplevel->last_clip = (struct wlr_box){0};
		return;
	}

	struct wlr_box geo = viewport_view_geometry(toplevel);
	struct wlr_box clip = {
		.x = toplevel->clip.x - toplevel->box.x + geo.x,
		.y = toplevel->clip.y - toplevel->box.y + geo.y,
		.width = toplevel->clip.width,
		.height = toplevel->clip.height,
	};

	if (toplevel->server->config.debug &&
			memcmp(&clip, &toplevel->last_clip, sizeof(clip)) != 0) {
		wlr_log(WLR_DEBUG, "view %u clip %d,%d %dx%d", toplevel->id, clip.x,
			clip.y, clip.width, clip.height);
	}
	toplevel->last_clip = clip;

	wlr_scene_subsurface_tree_set_clip(&toplevel->surface_tree->node, &clip);
}

void viewport_toplevel_set_box(struct viewport_toplevel *toplevel,
	const struct wlr_box *box)
{
	toplevel->box = *box;
	toplevel->has_box = true;
	toplevel->visible = true;

	/* Position the client's *window geometry*, not its surface origin.
	 *
	 * A client using client-side decorations — which is every client here,
	 * since we do not implement xdg-decoration — draws shadows and resize
	 * handles outside its logical window. xdg_surface.geometry marks the real
	 * window inside that larger surface, and its origin is frequently
	 * negative (foot reports 0,-26). Placing the surface origin at the rect
	 * the shell asked for therefore lands every window off by the decoration
	 * margin. Subtracting the offset makes the DOM rect line up with what the
	 * user perceives as the window edge. */
	/* See handle_toplevel_commit(): the scene tree's origin is already the
	 * window geometry origin, so the rect is applied verbatim. */
	wlr_scene_node_set_position(&toplevel->scene_tree->node, box->x, box->y);

	if (toplevel->server->config.debug) {
		struct wlr_box geo = viewport_view_geometry(toplevel);
		wlr_log(WLR_DEBUG, "view %u want %d,%d %dx%d | geo %d,%d %dx%d",
			toplevel->id, box->x, box->y, box->width, box->height,
			geo.x, geo.y, geo.width, geo.height);
	}

	/* Never ask a client for less than it says it can handle.
	 *
	 * A client that receives a configure below its minimum is entitled to
	 * ignore it and commit whatever size it likes, which leaves the window
	 * overflowing the hole the shell drew — the layout and the reality drift
	 * apart with no way to reconcile them. Clamping here keeps the request
	 * honest; the shell is told the minimum separately so it can stop the
	 * resize at the same point rather than appearing to shrink past it. */
	int width = box->width;
	int height = box->height;
	if (toplevel->kind == VIEWPORT_VIEW_XDG) {
		struct wlr_xdg_toplevel_state *state =
			&toplevel->xdg_toplevel->current;
		if (state->min_width > 0 && width < state->min_width) {
			width = state->min_width;
		}
		if (state->min_height > 0 && height < state->min_height) {
			height = state->min_height;
		}
	}

	/* Only reconfigure when the size actually changed: every configure is a
	 * round trip, and the shell may resend the same rect on each animation
	 * frame while dragging. */
	viewport_view_set_size(toplevel, width, height);

	/* Scrolled entirely off its output: nothing to show, and clipping to a
	 * zero-sized region would disable the clip rather than apply it. */
	bool offscreen = toplevel->has_clip &&
		(toplevel->clip.width <= 0 || toplevel->clip.height <= 0);

	apply_clip(toplevel);

	if (toplevel->mapped) {
		wlr_scene_node_set_enabled(&toplevel->scene_tree->node, !offscreen);
	}

	if (toplevel->server->config.debug) {
		wlr_log(WLR_DEBUG, "view %u boxed (mapped=%d -> visible=%d)",
			toplevel->id, toplevel->mapped, toplevel->mapped);
	}

	/* The first window at startup is placed while the shell page is still
	 * loading, and in that window of time nothing else is damaging the scene.
	 * Make placement itself force a repaint rather than relying on whatever
	 * else happens to be going on. */
	struct viewport_output *output;
	wl_list_for_each(output, &toplevel->server->outputs, link) {
		wlr_output_schedule_frame(output->wlr_output);
	}
}

void viewport_toplevel_focus(struct viewport_toplevel *toplevel)
{
	struct viewport_server *server = toplevel->server;
	struct wlr_surface *surface = viewport_view_surface(toplevel);

	if (server->focused == toplevel) {
		return;
	}

	if (server->focused != NULL) {
		viewport_view_set_activated(server->focused, false);
	}

	/* Keyboard focus leaves the shell. WebKit needs to be told explicitly or
	 * it keeps a blinking caret in whatever field was focused. */
	if (server->web != NULL) {
		viewport_web_focus(server->web, false);
	}

	wlr_scene_node_raise_to_top(&toplevel->scene_tree->node);
	viewport_view_set_activated(toplevel, true);

	struct wlr_keyboard *keyboard = wlr_seat_get_keyboard(server->seat);
	if (keyboard != NULL) {
		wlr_seat_keyboard_notify_enter(server->seat, surface,
			keyboard->keycodes, keyboard->num_keycodes, &keyboard->modifiers);
	} else {
		wlr_seat_keyboard_notify_enter(server->seat, surface, NULL, 0, NULL);
	}

	server->focused = toplevel;
	viewport_ipc_notify_focus(server, toplevel->id);
}

/* Centre of a view's target rect, in layout coordinates. */
static void toplevel_centre(struct viewport_toplevel *toplevel, double *x,
	double *y)
{
	*x = toplevel->box.x + toplevel->box.width / 2.0;
	*y = toplevel->box.y + toplevel->box.height / 2.0;
}

void viewport_focus_direction(struct viewport_server *server,
	const char *direction)
{
	if (wl_list_empty(&server->toplevels)) {
		return;
	}

	/* next/prev walk the list; the shell controls its order by the sequence in
	 * which windows were added, which matches what the dock shows. */
	if (strcmp(direction, "next") == 0 || strcmp(direction, "prev") == 0) {
		bool forward = strcmp(direction, "next") == 0;
		struct viewport_toplevel *current = server->focused;
		struct wl_list *link;

		if (current == NULL) {
			link = forward ? server->toplevels.next : server->toplevels.prev;
		} else {
			link = forward ? current->link.next : current->link.prev;
			if (link == &server->toplevels) {
				/* Wrap rather than stop at the ends. */
				link = forward ? server->toplevels.next : server->toplevels.prev;
			}
		}

		struct viewport_toplevel *next = wl_container_of(link, next, link);
		if (next->mapped) {
			viewport_toplevel_focus(next);
		}
		return;
	}

	int dx = 0, dy = 0;
	if (strcmp(direction, "left") == 0) {
		dx = -1;
	} else if (strcmp(direction, "right") == 0) {
		dx = 1;
	} else if (strcmp(direction, "up") == 0) {
		dy = -1;
	} else if (strcmp(direction, "down") == 0) {
		dy = 1;
	} else {
		wlr_log(WLR_ERROR, "unknown focus direction '%s'", direction);
		return;
	}

	/* Directional focus compares window centres so it follows what is on
	 * screen, including across monitors, rather than stacking order. Ties are
	 * broken by distance, so the nearest candidate in that direction wins. */
	double from_x = 0, from_y = 0;
	if (server->focused != NULL) {
		toplevel_centre(server->focused, &from_x, &from_y);
	}

	struct viewport_toplevel *best = NULL;
	double best_distance = 0;
	struct viewport_toplevel *candidate;

	wl_list_for_each(candidate, &server->toplevels, link) {
		if (candidate == server->focused || !candidate->mapped ||
				!candidate->has_box || !candidate->visible) {
			continue;
		}

		double cx, cy;
		toplevel_centre(candidate, &cx, &cy);
		double along = (cx - from_x) * dx + (cy - from_y) * dy;
		if (server->focused != NULL && along <= 0) {
			continue; /* not in the requested direction */
		}

		double offset = (cx - from_x) * dy + (cy - from_y) * dx;
		if (offset < 0) {
			offset = -offset;
		}
		/* Penalise lateral drift so "right" prefers the window beside this
		 * one over one that is further right but far off-axis. */
		double distance = along + offset * 2.0;

		if (best == NULL || distance < best_distance) {
			best = candidate;
			best_distance = distance;
		}
	}

	if (best != NULL) {
		viewport_toplevel_focus(best);
		return;
	}

	/* No window that way. In sway this moves to the monitor in that direction
	 * even when it is empty, so hand the direction to the shell — which owns
	 * the notion of an active output — rather than doing nothing. */
	char command[64];
	snprintf(command, sizeof(command), "output.focus %s", direction);
	viewport_ipc_notify_shell_command(server, command);
}

void viewport_toplevel_close(struct viewport_toplevel *toplevel)
{
	viewport_view_close(toplevel);
}
