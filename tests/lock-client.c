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
 * The other half is what happens when a locker is *not* dead. A compositor
 * that hands the session to a second locker over a working one has two clients
 * owning one screen, only one of them drawn, and unlocking the one you can see
 * leaves the other holding a lock nothing on screen can reach — a session that
 * stays locked after a correct password. Smithay grants every lock request and
 * leaves the refusal to the compositor, so this is the compositor's to get
 * right and worth a client that checks it.
 *
 *   lock-client crash    lock, wait for confirmation, exit hard
 *   lock-client unlock   lock, wait for confirmation, unlock, exit cleanly
 *   lock-client hold     lock, draw a lock screen on every output, stay up
 *   lock-client second   lock, and require the compositor to refuse it
 *
 * Exits 0 once it has done its job, 2 if it could not lock at all, and for
 * `second`, 1 if the compositor granted a lock it should have refused.
 */
#define _GNU_SOURCE

#include <errno.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#include <wayland-client.h>

#include "ext-session-lock-v1-client-protocol.h"

/* However many screens a headless compositor is ever asked for here. */
#define MAX_OUTPUTS 4

struct state {
	struct wl_compositor *compositor;
	struct wl_shm *shm;
	struct ext_session_lock_manager_v1 *manager;
	struct ext_session_lock_v1 *lock;
	struct wl_output *outputs[MAX_OUTPUTS];
	int output_count;
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
	} else if (strcmp(interface, wl_shm_interface.name) == 0) {
		state->shm = wl_registry_bind(registry, name, &wl_shm_interface, 1);
	} else if (strcmp(interface, wl_output_interface.name) == 0) {
		if (state->output_count < MAX_OUTPUTS) {
			state->outputs[state->output_count++] = wl_registry_bind(
				registry, name, &wl_output_interface, 1);
		}
	} else if (strcmp(interface,
			ext_session_lock_manager_v1_interface.name) == 0) {
		state->manager = wl_registry_bind(registry, name,
			&ext_session_lock_manager_v1_interface, 1);
	}
}

/* An opaque buffer of the size the compositor asked for.
 *
 * The colour does not matter to anything here — what matters is that the
 * surface has a buffer, because a lock surface with none is a locker that has
 * not drawn, and "has not drawn" is the case the compositor is allowed to let
 * another locker take over. */
static struct wl_buffer *make_buffer(struct state *state, uint32_t width,
	uint32_t height)
{
	int stride = (int)width * 4;
	size_t size = (size_t)stride * height;

	int fd = memfd_create("lock-client", MFD_CLOEXEC);
	if (fd < 0) {
		fprintf(stderr, "memfd_create: %s\n", strerror(errno));
		return NULL;
	}
	if (ftruncate(fd, (off_t)size) < 0) {
		fprintf(stderr, "ftruncate: %s\n", strerror(errno));
		close(fd);
		return NULL;
	}
	uint32_t *pixels = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED,
		fd, 0);
	if (pixels == MAP_FAILED) {
		fprintf(stderr, "mmap: %s\n", strerror(errno));
		close(fd);
		return NULL;
	}
	for (size_t i = 0; i < size / 4; i++) {
		pixels[i] = 0xff0000ff;
	}
	munmap(pixels, size);

	struct wl_shm_pool *pool = wl_shm_create_pool(state->shm, fd,
		(int32_t)size);
	struct wl_buffer *buffer = wl_shm_pool_create_buffer(pool, 0,
		(int32_t)width, (int32_t)height, stride, WL_SHM_FORMAT_ARGB8888);
	wl_shm_pool_destroy(pool);
	close(fd);
	return buffer;
}

/* One lock surface, and its screen's worth of pixels. */
struct lock_screen {
	struct state *state;
	struct wl_surface *surface;
	struct ext_session_lock_surface_v1 *lock_surface;
	bool drawn;
};

static void handle_configure(void *data,
	struct ext_session_lock_surface_v1 *lock_surface, uint32_t serial,
	uint32_t width, uint32_t height)
{
	struct lock_screen *screen = data;

	/* Acknowledged before anything is attached, which is the order the
	 * protocol asks for: the compositor decides the size and the client
	 * agrees to it before it may show anything. */
	ext_session_lock_surface_v1_ack_configure(lock_surface, serial);

	struct wl_buffer *buffer = make_buffer(screen->state, width, height);
	if (buffer == NULL) {
		return;
	}
	wl_surface_attach(screen->surface, buffer, 0, 0);
	wl_surface_damage_buffer(screen->surface, 0, 0, (int32_t)width,
		(int32_t)height);
	wl_surface_commit(screen->surface);
	screen->drawn = true;
}

static const struct ext_session_lock_surface_v1_listener lock_surface_listener = {
	.configure = handle_configure,
};

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
			 strcmp(argv[1], "unlock") != 0 &&
			 strcmp(argv[1], "hold") != 0 &&
			 strcmp(argv[1], "second") != 0)) {
		fprintf(stderr, "usage: %s crash|unlock|hold|second\n", argv[0]);
		return 2;
	}
	bool crash = strcmp(argv[1], "crash") == 0;
	bool hold = strcmp(argv[1], "hold") == 0;
	bool second = strcmp(argv[1], "second") == 0;

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

	if (second) {
		/* The whole test: a locker is already up and drawing, so this one
		 * must be told `finished` rather than handed the session. Being
		 * granted it is the failure, and a loud one — it is the bug that
		 * leaves a session locked after a correct password. */
		if (state.finished) {
			fprintf(stderr, "refused, as it should be\n");
			return 0;
		}
		fprintf(stderr, "the compositor granted a second lock\n");
		return 1;
	}

	if (state.finished) {
		fprintf(stderr, "the compositor refused the lock\n");
		return 2;
	}

	if (hold) {
		if (state.shm == NULL || state.output_count == 0) {
			fprintf(stderr, "no wl_shm or no outputs to draw on\n");
			return 2;
		}
		/* A lock screen on every output, because a compositor tracks them
		 * per output and one screen left undrawn is one it may hand to
		 * somebody else. */
		struct lock_screen screens[MAX_OUTPUTS] = { 0 };
		for (int i = 0; i < state.output_count; i++) {
			screens[i].state = &state;
			screens[i].surface = wl_compositor_create_surface(state.compositor);
			screens[i].lock_surface = ext_session_lock_v1_get_lock_surface(
				state.lock, screens[i].surface, state.outputs[i]);
			ext_session_lock_surface_v1_add_listener(screens[i].lock_surface,
				&lock_surface_listener, &screens[i]);
		}
		wl_display_roundtrip(display);
		wl_display_roundtrip(display);
		for (int i = 0; i < state.output_count; i++) {
			if (!screens[i].drawn) {
				fprintf(stderr, "a lock surface was never configured\n");
				return 2;
			}
		}
		fprintf(stderr, "locked and drawing on %d output(s)\n",
			state.output_count);
		/* Stay. The test kills this when it is done with it — a locker that
		 * exits is the *other* case, and the compositor is allowed to let
		 * another one take over from it. */
		while (wl_display_dispatch(display) >= 0) {
		}
		return 0;
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
