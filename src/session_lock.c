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

#include <wlr/types/wlr_scene.h>
#include <wlr/types/wlr_session_lock_v1.h>
#include <wlr/util/log.h>

#include "viewport.h"

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

static void handle_unlock(struct wl_listener *listener, void *data)
{
	struct viewport_lock *lock = wl_container_of(listener, lock, unlock);
	struct viewport_server *server = lock->server;

	server->locked = false;
	wlr_scene_node_set_enabled(&server->layer_lock->node, false);

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
		 * of the protocol: a crash must not expose the desktop, so the lock
		 * layer keeps covering the screen with nothing on it. */
		wlr_log(WLR_ERROR,
			"lock client vanished while locked; session stays locked");
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
