/* SPDX-License-Identifier: MIT
 *
 * Screen locking.
 *
 * Without ext-session-lock-v1 there is no way to lock the session at all:
 * swaylock and every other locker binds this protocol and exits immediately if
 * it is missing, so the screen simply never locks.
 *
 * The protocol exists because a lock screen has a security property no
 * ordinary client can be trusted with: once the compositor acknowledges a
 * lock, nothing else may be shown or receive input until the locker says
 * otherwise — and if the locker *crashes*, the session must stay locked rather
 * than falling open. That is why the lock surfaces live on their own layer
 * above everything, and why a crashed locker leaves a blank screen instead of
 * revealing the desktop.
 */
#define _POSIX_C_SOURCE 200809L

#include <stdlib.h>

#include <wlr/types/wlr_output_layout.h>
#include <wlr/types/wlr_scene.h>
#include <wlr/types/wlr_session_lock_v1.h>
#include <wlr/util/log.h>

#include "viewport-view.h"
#include "viewport-input.h"
#include "viewport-output.h"

struct viewport_lock_surface {
	struct wlr_session_lock_surface_v1 *lock_surface;
	struct viewport_output *output;
	struct wlr_scene_tree *tree;

	struct wl_listener map;
	struct wl_listener destroy;
};

static void handle_lock_surface_map(struct wl_listener *listener, void *data)
{
	struct viewport_lock_surface *surface =
		wl_container_of(listener, surface, map);
	struct viewport_server *server = surface->output->server;

	/* Focus the locker so typing a password reaches it rather than whatever
	 * was focused when the screen locked. */
	struct wlr_keyboard *keyboard = wlr_seat_get_keyboard(server->seat);
	wlr_seat_keyboard_notify_enter(server->seat,
		surface->lock_surface->surface,
		keyboard != NULL ? keyboard->keycodes : NULL,
		keyboard != NULL ? keyboard->num_keycodes : 0,
		keyboard != NULL ? &keyboard->modifiers : NULL);
}

static void handle_lock_surface_destroy(struct wl_listener *listener,
	void *data)
{
	struct viewport_lock_surface *surface =
		wl_container_of(listener, surface, destroy);

	wl_list_remove(&surface->map.link);
	wl_list_remove(&surface->destroy.link);
	free(surface);
}

static void handle_new_lock_surface(struct wl_listener *listener, void *data)
{
	struct viewport_lock *lock = wl_container_of(listener, lock, new_surface);
	struct wlr_session_lock_surface_v1 *lock_surface = data;
	struct viewport_output *output = lock_surface->output->data;
	if (output == NULL) {
		/* The monitor is already going away: handle_output_destroy() clears
		 * wlr_output->data before freeing the struct, so this downcast legally
		 * yields NULL, and every line below dereferences it. wlroots listens on
		 * the same output's destroy signal and will take this lock surface with
		 * it, so dropping it here loses nothing. */
		wlr_log(WLR_DEBUG, "lock surface for a dying output, ignoring");
		return;
	}

	struct viewport_lock_surface *surface = calloc(1, sizeof(*surface));
	if (surface == NULL) {
		return;
	}
	surface->lock_surface = lock_surface;
	surface->output = output;
	surface->tree = wlr_scene_subsurface_tree_create(lock->server->layer_lock,
		lock_surface->surface);

	if (surface->tree != NULL) {
		struct wlr_box box;
		wlr_output_layout_get_box(lock->server->output_layout,
			output->wlr_output, &box);
		wlr_scene_node_set_position(&surface->tree->node, box.x, box.y);
		/* The locker covers exactly one output, so it is told that size. */
		wlr_session_lock_surface_v1_configure(lock_surface, box.width,
			box.height);
	}

	surface->map.notify = handle_lock_surface_map;
	wl_signal_add(&lock_surface->surface->events.map, &surface->map);
	surface->destroy.notify = handle_lock_surface_destroy;
	wl_signal_add(&lock_surface->events.destroy, &surface->destroy);
}

/* A black rectangle under every lock surface, covering the whole layout.
 *
 * The locker draws one surface per output, but it only learns about an output
 * once it exists — so a monitor plugged in while the session is locked would
 * show the desktop until the locker caught up. Covering the entire layout means
 * there is no such window, and it costs one rectangle.
 *
 * It also covers the case of a locker that simply never draws on some output,
 * which the protocol allows and which would otherwise leave that screen
 * readable. */
static void update_backdrop(struct viewport_server *server)
{
	/* Keyed on the session being locked rather than on a lock object existing:
	 * after a locker dies there is no lock object and the screen still has to
	 * stay covered. */
	if (!server->locked && server->lock == NULL) {
		return;
	}

	struct wlr_box layout;
	wlr_output_layout_get_box(server->output_layout, NULL, &layout);

	if (server->lock_backdrop == NULL) {
		const float black[4] = { 0.0f, 0.0f, 0.0f, 1.0f };
		server->lock_backdrop = wlr_scene_rect_create(server->layer_lock,
			layout.width, layout.height, black);
		if (server->lock_backdrop == NULL) {
			return;
		}
		/* Below the lock surfaces, which are added to the same tree. */
		wlr_scene_node_lower_to_bottom(&server->lock_backdrop->node);
	} else {
		wlr_scene_rect_set_size(server->lock_backdrop, layout.width,
			layout.height);
	}

	wlr_scene_node_set_position(&server->lock_backdrop->node, layout.x,
		layout.y);
}

/* Called when outputs change, so a screen that appears while locked is covered
 * rather than revealing what is behind. */
void viewport_session_lock_outputs_changed(struct viewport_server *server)
{
	update_backdrop(server);
}

static void handle_unlock(struct wl_listener *listener, void *data)
{
	struct viewport_lock *lock = wl_container_of(listener, lock, unlock);
	struct viewport_server *server = lock->server;

	server->locked = false;
	wlr_scene_node_set_enabled(&server->layer_lock->node, false);

	/* Unlocking is the one path that ends the cover. */
	if (server->lock_backdrop != NULL) {
		wlr_scene_node_destroy(&server->lock_backdrop->node);
		server->lock_backdrop = NULL;
	}

	/* Hand the keyboard back to whatever was focused before. */
	if (server->focused != NULL) {
		struct viewport_toplevel *toplevel = server->focused;
		server->focused = NULL;
		viewport_toplevel_focus(toplevel);
	} else {
		viewport_focus_web(server);
	}

	wlr_log(WLR_INFO, "session unlocked");
}

static void handle_lock_destroy(struct wl_listener *listener, void *data)
{
	struct viewport_lock *lock = wl_container_of(listener, lock, destroy);
	struct viewport_server *server = lock->server;

	wl_list_remove(&lock->new_surface.link);
	wl_list_remove(&lock->unlock.link);
	wl_list_remove(&lock->destroy.link);

	if (server->locked) {
		/* The locker died without unlocking. Staying locked is the whole point
		 * of the protocol: a crash must not expose the desktop.
		 *
		 * The lock layer does not do that by itself. It is a scene tree, and an
		 * empty one draws nothing and occludes nothing — the locker's surfaces
		 * went with the client, so leaving the layer enabled and empty showed
		 * the desktop straight through it, with every window readable and the
		 * shell taking input, while `locked` stayed true and no client could be
		 * clicked again. The backdrop is the only thing actually covering the
		 * screen, so it is what must survive, and update_backdrop() keeps
		 * resizing it for outputs that arrive later.
		 *
		 * It is left deliberately blank. Drawing anything here would be a
		 * password prompt the compositor cannot honour. */
		update_backdrop(server);
		wlr_log(WLR_ERROR,
			"lock client vanished while locked; session stays locked "
			"(screen stays covered; switch VT to recover)");
	} else if (server->lock_backdrop != NULL) {
		wlr_scene_node_destroy(&server->lock_backdrop->node);
		server->lock_backdrop = NULL;
	}

	free(lock);
	server->lock = NULL;
}

static void handle_new_lock(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, new_session_lock);
	struct wlr_session_lock_v1 *wlr_lock = data;

	if (server->lock != NULL) {
		/* Already locked by someone else. */
		wlr_session_lock_v1_destroy(wlr_lock);
		return;
	}

	struct viewport_lock *lock = calloc(1, sizeof(*lock));
	if (lock == NULL) {
		wlr_session_lock_v1_destroy(wlr_lock);
		return;
	}
	lock->server = server;
	lock->lock = wlr_lock;
	server->lock = lock;

	lock->new_surface.notify = handle_new_lock_surface;
	wl_signal_add(&wlr_lock->events.new_surface, &lock->new_surface);
	lock->unlock.notify = handle_unlock;
	wl_signal_add(&wlr_lock->events.unlock, &lock->unlock);
	lock->destroy.notify = handle_lock_destroy;
	wl_signal_add(&wlr_lock->events.destroy, &lock->destroy);

	server->locked = true;
	wlr_scene_node_set_enabled(&server->layer_lock->node, true);
	wlr_scene_node_raise_to_top(&server->layer_lock->node);
	update_backdrop(server);

	/* Any pointer grab a game held must not survive the lock. */
	viewport_pointer_deactivate_constraint(server);
	wlr_seat_pointer_notify_clear_focus(server->seat);

	wlr_session_lock_v1_send_locked(wlr_lock);
	wlr_log(WLR_INFO, "session locked");
}

void viewport_session_lock_init(struct viewport_server *server)
{
	server->session_lock_manager =
		wlr_session_lock_manager_v1_create(server->wl_display);
	if (server->session_lock_manager == NULL) {
		return;
	}

	server->new_session_lock.notify = handle_new_lock;
	wl_signal_add(&server->session_lock_manager->events.new_lock,
		&server->new_session_lock);
}
