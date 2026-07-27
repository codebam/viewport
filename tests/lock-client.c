/* SPDX-License-Identifier: MIT
 *
 * A screen locker that dies without unlocking.
 *
 * ext-session-lock-v1 exists for one property that cannot be tested from
 * inside the compositor: if the locker crashes, the session must stay locked.
 * A compositor can satisfy every other part of the protocol and still fail
 * this one, and the failure is invisible from the compositor's own state —
 * `locked` stays true, the lock layer stays enabled, and everything looks
 * right. What is wrong is what is on the screen, which only something outside
 * can see.
 *
 * So this locks the session, waits until the compositor confirms it, and then
 * exits without unlocking — the same thing a segfaulting swaylock does. What
 * the screen looks like afterwards is the capture client's question.
 *
 *   lock-client crash    lock, wait for confirmation, exit hard
 *   lock-client unlock   lock, wait for confirmation, unlock, exit cleanly
 *
 * Exits 0 once it has done its job, 2 if it could not lock at all.
 */
#define _GNU_SOURCE

#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <wayland-client.h>

#include "ext-session-lock-v1-client-protocol.h"

struct state {
	struct wl_compositor *compositor;
	struct ext_session_lock_manager_v1 *manager;
	struct ext_session_lock_v1 *lock;
	bool locked;
	bool finished;
};

static void handle_locked(void *data, struct ext_session_lock_v1 *lock)
{
	struct state *state = data;
	state->locked = true;
}

/* The compositor refusing the lock — someone else already holds one. */
static void handle_finished(void *data, struct ext_session_lock_v1 *lock)
{
	struct state *state = data;
	state->finished = true;
}

static const struct ext_session_lock_v1_listener lock_listener = {
	.locked = handle_locked,
	.finished = handle_finished,
};

static void handle_global(void *data, struct wl_registry *registry,
	uint32_t name, const char *interface, uint32_t version)
{
	struct state *state = data;

	if (strcmp(interface, wl_compositor_interface.name) == 0) {
		state->compositor = wl_registry_bind(registry, name,
			&wl_compositor_interface, 4);
	} else if (strcmp(interface,
			ext_session_lock_manager_v1_interface.name) == 0) {
		state->manager = wl_registry_bind(registry, name,
			&ext_session_lock_manager_v1_interface, 1);
	}
}

static void handle_global_remove(void *data, struct wl_registry *registry,
	uint32_t name)
{
}

static const struct wl_registry_listener registry_listener = {
	.global = handle_global,
	.global_remove = handle_global_remove,
};

int main(int argc, char *argv[])
{
	if (argc != 2 ||
			(strcmp(argv[1], "crash") != 0 &&
			 strcmp(argv[1], "unlock") != 0)) {
		fprintf(stderr, "usage: %s crash|unlock\n", argv[0]);
		return 2;
	}
	bool crash = strcmp(argv[1], "crash") == 0;

	struct wl_display *display = wl_display_connect(NULL);
	if (display == NULL) {
		fprintf(stderr, "cannot connect to WAYLAND_DISPLAY\n");
		return 2;
	}

	struct state state = { 0 };
	struct wl_registry *registry = wl_display_get_registry(display);
	wl_registry_add_listener(registry, &registry_listener, &state);
	wl_display_roundtrip(display);

	if (state.manager == NULL) {
		fprintf(stderr, "compositor does not offer ext-session-lock-v1\n");
		return 2;
	}

	state.lock = ext_session_lock_manager_v1_lock(state.manager);
	ext_session_lock_v1_add_listener(state.lock, &lock_listener, &state);

	/* Deliberately no lock surface. The compositor must cover the screen on
	 * its own from the moment it confirms the lock: a locker is allowed to be
	 * slow to draw, and one that dies before drawing anything is exactly the
	 * case being tested. */
	while (!state.locked && !state.finished) {
		if (wl_display_dispatch(display) < 0) {
			fprintf(stderr, "disconnected before the lock was confirmed\n");
			return 2;
		}
	}

	if (state.finished) {
		fprintf(stderr, "the compositor refused the lock\n");
		return 2;
	}

	if (crash) {
		/* No unlock, no destroy, no flush of a polite goodbye — just go, the
		 * way a crash does. The compositor sees the connection drop. */
		fprintf(stderr, "locked; exiting without unlocking\n");
		_exit(0);
	}

	ext_session_lock_v1_unlock_and_destroy(state.lock);
	wl_display_roundtrip(display);
	fprintf(stderr, "locked and unlocked cleanly\n");
	return 0;
}
