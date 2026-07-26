/* SPDX-License-Identifier: MIT
 *
 * wlr-layer-shell.
 *
 * Added because Mod4+d did nothing: wmenu, wofi, rofi, fuzzel and every other
 * Wayland launcher is a layer-shell client. Without the protocol they fail to
 * bind it and exit before drawing a single pixel, so the keybinding fires, the
 * process starts, and nothing appears — indistinguishable from a broken bind.
 *
 * The same protocol backs panels, wallpapers and lock screens, so this is also
 * what makes waybar or swaylock usable here.
 *
 * Note the keyboard handling at the bottom. A launcher that cannot receive
 * keystrokes is decoration: it needs an explicit seat focus grab, because the
 * compositor's normal focus rules only ever consider xdg toplevels and the web
 * shell.
 */
#define _POSIX_C_SOURCE 200809L

#include <stdlib.h>
#include <string.h>

#include <wlr/types/wlr_layer_shell_v1.h>
#include <wlr/types/wlr_scene.h>
#include <wlr/util/log.h>

#include "viewport.h"

struct viewport_layer_surface {
	/* Must stay first: scene node data is downcast through this. */
	struct viewport_node node;

	struct wlr_layer_surface_v1 *layer_surface;
	struct wlr_scene_layer_surface_v1 *scene;
	struct viewport_server *server;
	struct viewport_output *output;

	struct wl_listener map;
	struct wl_listener unmap;
	struct wl_listener commit;
	struct wl_listener destroy;
	struct wl_listener node_destroy;
};

static struct wlr_scene_tree *tree_for_layer(struct viewport_server *server,
	enum zwlr_layer_shell_v1_layer layer)
{
	switch (layer) {
	case ZWLR_LAYER_SHELL_V1_LAYER_BACKGROUND:
		return server->layer_bg;
	case ZWLR_LAYER_SHELL_V1_LAYER_BOTTOM:
		return server->layer_bottom;
	case ZWLR_LAYER_SHELL_V1_LAYER_TOP:
		return server->layer_top;
	case ZWLR_LAYER_SHELL_V1_LAYER_OVERLAY:
		return server->layer_overlay;
	}
	return server->layer_top;
}

/* Re-applies the layer surface's anchors and margins against its output. */
static void arrange_surface(struct viewport_layer_surface *surface)
{
	if (surface->output == NULL || surface->scene == NULL) {
		return;
	}

	struct wlr_box full_area;
	wlr_output_layout_get_box(surface->server->output_layout,
		surface->output->wlr_output, &full_area);

	/* Usable area starts as the whole output. A full implementation would
	 * subtract each panel's exclusive zone here and hand the remainder to the
	 * shell as the region it may place windows in; for now the shell owns the
	 * whole output and panels overlap it. */
	struct wlr_box usable_area = full_area;

	wlr_scene_layer_surface_v1_configure(surface->scene, &full_area,
		&usable_area);
}

void viewport_layers_arrange(struct viewport_output *output)
{
	/* Walk the scene trees rather than keeping a second list: the scene graph
	 * is already the authoritative record of what exists. */
	struct viewport_server *server = output->server;
	struct wlr_scene_tree *trees[] = {
		server->layer_bg, server->layer_bottom,
		server->layer_top, server->layer_overlay,
	};

	for (size_t i = 0; i < sizeof(trees) / sizeof(trees[0]); i++) {
		struct wlr_scene_node *node;
		wl_list_for_each(node, &trees[i]->children, link) {
			struct viewport_node *tagged = node->data;
			if (tagged == NULL || tagged->type != VIEWPORT_NODE_LAYER) {
				continue;
			}
			struct viewport_layer_surface *surface =
				(struct viewport_layer_surface *)tagged;
			if (surface->output == output) {
				arrange_surface(surface);
			}
		}
	}
}

static void handle_commit(struct wl_listener *listener, void *data)
{
	struct viewport_layer_surface *surface =
		wl_container_of(listener, surface, commit);
	struct wlr_layer_surface_v1 *layer_surface = surface->layer_surface;

	if (layer_surface->initial_commit) {
		/* The protocol requires a configure in response to the first commit,
		 * before the client may attach a buffer. */
		arrange_surface(surface);
		return;
	}

	if (layer_surface->current.committed != 0) {
		/* Anchors, size, margins or layer changed. */
		struct wlr_scene_tree *tree = tree_for_layer(surface->server,
			layer_surface->current.layer);
		wlr_scene_node_reparent(&surface->scene->tree->node, tree);
		arrange_surface(surface);
	}
}

static void focus_layer_surface(struct viewport_layer_surface *surface)
{
	struct viewport_server *server = surface->server;
	struct wlr_layer_surface_v1 *layer_surface = surface->layer_surface;

	if (layer_surface->current.keyboard_interactive ==
			ZWLR_LAYER_SURFACE_V1_KEYBOARD_INTERACTIVITY_NONE) {
		return;
	}

	/* A launcher must actually receive keystrokes to be usable, and the
	 * compositor's normal focus path only considers xdg toplevels and the web
	 * shell — so grab focus explicitly here. */
	if (server->web != NULL) {
		viewport_web_focus(server->web, false);
	}

	struct wlr_keyboard *keyboard = wlr_seat_get_keyboard(server->seat);
	if (keyboard != NULL) {
		wlr_seat_keyboard_notify_enter(server->seat, layer_surface->surface,
			keyboard->keycodes, keyboard->num_keycodes, &keyboard->modifiers);
	} else {
		wlr_seat_keyboard_notify_enter(server->seat, layer_surface->surface,
			NULL, 0, NULL);
	}
}

static void handle_map(struct wl_listener *listener, void *data)
{
	struct viewport_layer_surface *surface =
		wl_container_of(listener, surface, map);
	arrange_surface(surface);
	focus_layer_surface(surface);
}

static void handle_unmap(struct wl_listener *listener, void *data)
{
	struct viewport_layer_surface *surface =
		wl_container_of(listener, surface, unmap);
	struct viewport_server *server = surface->server;

	/* Hand the keyboard back, or dismissing a launcher leaves the session with
	 * no focus at all and typing goes nowhere. */
	if (server->seat->keyboard_state.focused_surface ==
			surface->layer_surface->surface) {
		if (server->focused != NULL) {
			struct viewport_toplevel *toplevel = server->focused;
			server->focused = NULL;
			viewport_toplevel_focus(toplevel);
		} else {
			viewport_focus_web(server);
		}
	}
}

static void handle_destroy(struct wl_listener *listener, void *data)
{
	struct viewport_layer_surface *surface =
		wl_container_of(listener, surface, destroy);

	wl_list_remove(&surface->map.link);
	wl_list_remove(&surface->unmap.link);
	wl_list_remove(&surface->commit.link);
	wl_list_remove(&surface->destroy.link);
	free(surface);
}

void viewport_handle_new_layer_surface(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, new_layer_surface);
	struct wlr_layer_surface_v1 *layer_surface = data;

	/* The client may leave the output unset, meaning "compositor decides".
	 * Pick the output under the cursor so a launcher opens on the screen the
	 * user is actually looking at. */
	struct viewport_output *output = NULL;
	if (layer_surface->output != NULL) {
		output = layer_surface->output->data;
	} else {
		/* Prefer the output the shell says is active, for the same reason
		 * new windows do: a launcher opened after a keyboard focus move must
		 * appear on the monitor the user is actually working on. */
		struct wlr_output *wlr_output = NULL;
		if (server->active_output != NULL) {
			struct viewport_output *candidate;
			wl_list_for_each(candidate, &server->outputs, link) {
				if (strcmp(candidate->wlr_output->name,
						server->active_output) == 0) {
					wlr_output = candidate->wlr_output;
					break;
				}
			}
		}
		if (wlr_output == NULL) {
			wlr_output = wlr_output_layout_output_at(
				server->output_layout, server->cursor->x, server->cursor->y);
		}
		if (wlr_output == NULL && !wl_list_empty(&server->outputs)) {
			struct viewport_output *first =
				wl_container_of(server->outputs.next, first, link);
			wlr_output = first->wlr_output;
		}
		if (wlr_output != NULL) {
			output = wlr_output->data;
			layer_surface->output = wlr_output;
		}
	}

	if (output == NULL) {
		wlr_log(WLR_ERROR, "layer surface with no usable output, closing");
		wlr_layer_surface_v1_destroy(layer_surface);
		return;
	}

	struct viewport_layer_surface *surface = calloc(1, sizeof(*surface));
	if (surface == NULL) {
		wlr_layer_surface_v1_destroy(layer_surface);
		return;
	}

	surface->node.type = VIEWPORT_NODE_LAYER;
	surface->server = server;
	surface->output = output;
	surface->layer_surface = layer_surface;

	struct wlr_scene_tree *tree = tree_for_layer(server,
		layer_surface->pending.layer);
	surface->scene = wlr_scene_layer_surface_v1_create(tree, layer_surface);
	if (surface->scene == NULL) {
		free(surface);
		wlr_layer_surface_v1_destroy(layer_surface);
		return;
	}
	surface->scene->tree->node.data = surface;

	surface->map.notify = handle_map;
	wl_signal_add(&layer_surface->surface->events.map, &surface->map);
	surface->unmap.notify = handle_unmap;
	wl_signal_add(&layer_surface->surface->events.unmap, &surface->unmap);
	surface->commit.notify = handle_commit;
	wl_signal_add(&layer_surface->surface->events.commit, &surface->commit);
	surface->destroy.notify = handle_destroy;
	wl_signal_add(&layer_surface->events.destroy, &surface->destroy);

	wlr_log(WLR_DEBUG, "layer surface '%s' on %s layer %d",
		layer_surface->namespace ? layer_surface->namespace : "?",
		output->wlr_output->name, layer_surface->pending.layer);
}
