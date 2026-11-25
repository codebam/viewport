/* SPDX-License-Identifier: MIT
 *
 * A screen locker, and the hostile lifecycle around one.
 *
 * ext-session-lock-v1 exists for one property that cannot be tested from
 * inside the compositor: if the locker crashes, the session must stay locked.
 * A compositor can satisfy every other part of the protocol and still fail
 * this one, and the failure is invisible from the compositor's own state —
 * `locked` stays true, the lock layer stays enabled, and everything looks
 * right. What is wrong is what is on the screen, which only something outside
 * can see.
 *
 * The other half is what happens when a locker is *not* dead. A compositor
 * that hands the session to a second locker over a working one has two clients
 * owning one screen, only one of them drawn, and unlocking the one you can see
 * leaves the other holding a lock nothing on screen can reach — a session that
 * stays locked after a correct password. Smithay grants every lock request and
 * leaves the refusal to the compositor, so this is the compositor's to get
 * right and worth a client that checks it.
 *
 * Ordinary modes:
 *
 *   lock-client crash          lock, wait for confirmation, exit hard
 *   lock-client unlock         lock, wait for confirmation, unlock, exit
 *   lock-client hold           lock, draw blue on every output, stay up
 *   lock-client hold-green     the same survivor, in a distinguishable colour
 *   lock-client draw-unlock    draw on every output, then unlock cleanly
 *   lock-client second         lock, and require refusal with `finished`
 *
 * Lifecycle modes. `idle` makes no surface; `idle-surface` acknowledges a
 * configure but draws nothing until it is superseded; `stale-unlock` then also
 * tries to unlock the lock that replaced it. The remaining names each drive
 * one protocol error and check the interface and error code, not merely that
 * the connection died. Every wait is bounded: a compositor wedge is a failed
 * probe, not a test that hangs forever.
 */
#define _GNU_SOURCE

#include <errno.h>
#include <poll.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#include <wayland-client.h>

#include "ext-session-lock-v1-client-protocol.h"
#include "xdg-shell-client-protocol.h"

/* However many screens a headless compositor is ever asked for here. */
#define MAX_OUTPUTS 4
#define WAIT_MS 5000

enum configure_action {
	CONFIGURE_DRAW,
	CONFIGURE_ACK,
	CONFIGURE_MANUAL,
	CONFIGURE_NULL_BUFFER,
	CONFIGURE_WRONG_SIZE,
	CONFIGURE_BAD_SERIAL,
};

struct state;

struct lock_screen {
	struct state *state;
	struct wl_surface *surface;
	struct ext_session_lock_surface_v1 *lock_surface;
	enum configure_action action;
	uint32_t serial;
	uint32_t width;
	uint32_t height;
	bool configured;
	bool drawn;
	bool draw_failed;
	struct wl_callback *frame_callback;
	uint32_t frames;
};

struct state {
	struct wl_display *display;
	struct wl_compositor *compositor;
	struct wl_shm *shm;
	struct ext_session_lock_manager_v1 *manager;
	struct xdg_wm_base *wm_base;
	struct wl_seat *seat;
	struct wl_keyboard *keyboard;
	struct ext_session_lock_v1 *lock;
	struct wl_output *outputs[MAX_OUTPUTS];
	int output_count;
	struct lock_screen screens[MAX_OUTPUTS];
	int screen_count;
	uint32_t colour;
	bool locked;
	bool finished;
};

static void handle_locked(void *data, struct ext_session_lock_v1 *lock)
{
	struct state *state = data;
	state->locked = true;
}

/* The compositor refusing the lock, or retiring it after a takeover. */
static void handle_finished(void *data, struct ext_session_lock_v1 *lock)
{
	struct state *state = data;
	state->finished = true;
	fprintf(stderr, "finished\n");
}

static const struct ext_session_lock_v1_listener lock_listener = {
	.locked = handle_locked,
	.finished = handle_finished,
};

static void handle_keymap(void *data, struct wl_keyboard *keyboard,
	uint32_t format, int32_t fd, uint32_t size)
{
	close(fd);
}

static void handle_keyboard_enter(void *data, struct wl_keyboard *keyboard,
	uint32_t serial, struct wl_surface *surface, struct wl_array *keys)
{
	fprintf(stderr, "keyboard-enter surface=%u\n",
		wl_proxy_get_id((struct wl_proxy *)surface));
}

static void handle_keyboard_leave(void *data, struct wl_keyboard *keyboard,
	uint32_t serial, struct wl_surface *surface)
{
	fprintf(stderr, "keyboard-leave surface=%u\n",
		wl_proxy_get_id((struct wl_proxy *)surface));
}

static void handle_key(void *data, struct wl_keyboard *keyboard,
	uint32_t serial, uint32_t time, uint32_t key, uint32_t key_state)
{
	if (key_state == WL_KEYBOARD_KEY_STATE_PRESSED) {
		fprintf(stderr, "key %u\n", key);
	}
}

static void handle_modifiers(void *data, struct wl_keyboard *keyboard,
	uint32_t serial, uint32_t depressed, uint32_t latched, uint32_t locked,
	uint32_t group)
{
}

static void handle_repeat_info(void *data, struct wl_keyboard *keyboard,
	int32_t rate, int32_t delay)
{
}

static const struct wl_keyboard_listener keyboard_listener = {
	.keymap = handle_keymap,
	.enter = handle_keyboard_enter,
	.leave = handle_keyboard_leave,
	.key = handle_key,
	.modifiers = handle_modifiers,
	.repeat_info = handle_repeat_info,
};

static void handle_seat_capabilities(void *data, struct wl_seat *seat,
	uint32_t capabilities)
{
	struct state *state = data;
	bool has_keyboard = capabilities & WL_SEAT_CAPABILITY_KEYBOARD;

	if (has_keyboard && state->keyboard == NULL) {
		state->keyboard = wl_seat_get_keyboard(seat);
		wl_keyboard_add_listener(state->keyboard, &keyboard_listener, state);
	} else if (!has_keyboard && state->keyboard != NULL) {
		wl_keyboard_destroy(state->keyboard);
		state->keyboard = NULL;
	}
}

static void handle_seat_name(void *data, struct wl_seat *seat, const char *name)
{
}

static const struct wl_seat_listener seat_listener = {
	.capabilities = handle_seat_capabilities,
	.name = handle_seat_name,
};

static void handle_wm_base_ping(void *data, struct xdg_wm_base *wm_base,
	uint32_t serial)
{
	xdg_wm_base_pong(wm_base, serial);
}

static const struct xdg_wm_base_listener wm_base_listener = {
	.ping = handle_wm_base_ping,
};

static void handle_global(void *data, struct wl_registry *registry,
	uint32_t name, const char *interface, uint32_t version)
{
	struct state *state = data;

	if (strcmp(interface, wl_compositor_interface.name) == 0) {
		state->compositor = wl_registry_bind(registry, name,
			&wl_compositor_interface, version < 4 ? version : 4);
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
	} else if (strcmp(interface, xdg_wm_base_interface.name) == 0) {
		state->wm_base = wl_registry_bind(registry, name,
			&xdg_wm_base_interface, 1);
		xdg_wm_base_add_listener(state->wm_base, &wm_base_listener, state);
	} else if (strcmp(interface, wl_seat_interface.name) == 0) {
		state->seat = wl_registry_bind(registry, name, &wl_seat_interface,
			version < 5 ? version : 5);
		wl_seat_add_listener(state->seat, &seat_listener, state);
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

static int64_t now_ms(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

/* Dispatch until either flag is set, with a deadline on the compositor's
 * answer. The caller keeps both flags alive for the whole wait. */
static int wait_for_flags(struct state *state, const bool *first,
	const bool *second)
{
	int64_t deadline = now_ms() + WAIT_MS;

	while (!*first && (second == NULL || !*second)) {
		if (wl_display_dispatch_pending(state->display) < 0) {
			return -1;
		}
		if (*first || (second != NULL && *second)) {
			break;
		}
		short events = POLLIN;
		if (wl_display_flush(state->display) < 0) {
			if (errno != EAGAIN) {
				return -1;
			}
			events |= POLLOUT;
		}

		int remaining = (int)(deadline - now_ms());
		if (remaining <= 0) {
			return -1;
		}
		struct pollfd pfd = {
			.fd = wl_display_get_fd(state->display),
			.events = events,
		};
		int ready = poll(&pfd, 1, remaining);
		if (ready < 0 && errno == EINTR) {
			continue;
		}
		if (ready <= 0 || wl_display_dispatch(state->display) < 0) {
			return -1;
		}
	}
	return 0;
}

static void handle_sync_done(void *data, struct wl_callback *callback,
	uint32_t time)
{
	bool *done = data;
	*done = true;
}

static const struct wl_callback_listener sync_listener = {
	.done = handle_sync_done,
};

/* wl_display_roundtrip has no timeout. Every sync in the probe goes through
 * this bounded equivalent so a wedged compositor fails instead of hanging. */
static int sync_display(struct state *state)
{
	bool done = false;
	struct wl_callback *callback = wl_display_sync(state->display);
	if (callback == NULL) {
		return -1;
	}
	wl_callback_add_listener(callback, &sync_listener, &done);
	int result = wait_for_flags(state, &done, NULL);
	wl_callback_destroy(callback);
	return result == 0 && done ? 0 : -1;
}

static void handle_buffer_release(void *data, struct wl_buffer *buffer)
{
	wl_buffer_destroy(buffer);
}

static const struct wl_buffer_listener buffer_listener = {
	.release = handle_buffer_release,
};

/* An opaque buffer of the size the compositor asked for. */
static struct wl_buffer *make_buffer(struct state *state, uint32_t width,
	uint32_t height)
{
	if (width == 0 || height == 0) {
		fprintf(stderr, "refusing to make a zero-sized buffer\n");
		return NULL;
	}
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
		pixels[i] = state->colour;
	}
	munmap(pixels, size);

	struct wl_shm_pool *pool = wl_shm_create_pool(state->shm, fd,
		(int32_t)size);
	struct wl_buffer *buffer = wl_shm_pool_create_buffer(pool, 0,
		(int32_t)width, (int32_t)height, stride, WL_SHM_FORMAT_ARGB8888);
	wl_buffer_add_listener(buffer, &buffer_listener, NULL);
	wl_shm_pool_destroy(pool);
	close(fd);
	return buffer;
}

static void request_frame(struct lock_screen *screen);
static int draw_screen(struct lock_screen *screen);

static void handle_frame_done(void *data, struct wl_callback *callback,
	uint32_t time)
{
	struct lock_screen *screen = data;
	wl_callback_destroy(callback);
	screen->frame_callback = NULL;
	screen->frames++;
	fprintf(stderr, "frame-done %u\n", screen->frames);
	/* Frame requests are double-buffered: request before the next commit,
	 * and commit again so the accepted locker keeps a presentation chain. */
	if (draw_screen(screen) < 0) {
		screen->draw_failed = true;
	}
}

static const struct wl_callback_listener frame_listener = {
	.done = handle_frame_done,
};

static void request_frame(struct lock_screen *screen)
{
	if (screen->frame_callback == NULL) {
		screen->frame_callback = wl_surface_frame(screen->surface);
		wl_callback_add_listener(screen->frame_callback, &frame_listener, screen);
	}
}

static int draw_screen(struct lock_screen *screen)
{
	struct wl_buffer *buffer = make_buffer(screen->state, screen->width,
		screen->height);
	if (buffer == NULL) {
		return -1;
	}
	/* Only a surface in the compositor's accepted lock map is sent frame
	 * callbacks. Output captures deliberately redact locked frames, so this is
	 * the outside observation that distinguishes a presented survivor from a
	 * refused or superseded surface with the same committed buffer. The request
	 * precedes the commit because frame callbacks are double-buffered state. */
	request_frame(screen);
	wl_surface_attach(screen->surface, buffer, 0, 0);
	wl_surface_damage_buffer(screen->surface, 0, 0, (int32_t)screen->width,
		(int32_t)screen->height);
	wl_surface_commit(screen->surface);
	screen->drawn = true;
	return 0;
}

static void handle_configure(void *data,
	struct ext_session_lock_surface_v1 *lock_surface, uint32_t serial,
	uint32_t width, uint32_t height)
{
	struct lock_screen *screen = data;
	screen->serial = serial;
	screen->width = width;
	screen->height = height;
	screen->configured = true;

	switch (screen->action) {
	case CONFIGURE_DRAW:
		ext_session_lock_surface_v1_ack_configure(lock_surface, serial);
		if (draw_screen(screen) < 0) {
			screen->draw_failed = true;
		}
		break;
	case CONFIGURE_ACK:
		ext_session_lock_surface_v1_ack_configure(lock_surface, serial);
		break;
	case CONFIGURE_MANUAL:
		break;
	case CONFIGURE_NULL_BUFFER:
		ext_session_lock_surface_v1_ack_configure(lock_surface, serial);
		wl_surface_attach(screen->surface, NULL, 0, 0);
		wl_surface_commit(screen->surface);
		break;
	case CONFIGURE_WRONG_SIZE:
		ext_session_lock_surface_v1_ack_configure(lock_surface, serial);
		screen->width++;
		if (draw_screen(screen) < 0) {
			screen->draw_failed = true;
		}
		break;
	case CONFIGURE_BAD_SERIAL:
		ext_session_lock_surface_v1_ack_configure(lock_surface, serial + 1);
		break;
	}
}

static const struct ext_session_lock_surface_v1_listener lock_surface_listener = {
	.configure = handle_configure,
};

static void init_screen(struct state *state, int index,
	enum configure_action action)
{
	struct lock_screen *screen = &state->screens[index];
	screen->state = state;
	screen->action = action;
	screen->surface = wl_compositor_create_surface(state->compositor);
	screen->lock_surface = ext_session_lock_v1_get_lock_surface(state->lock,
		screen->surface, state->outputs[index]);
	ext_session_lock_surface_v1_add_listener(screen->lock_surface,
		&lock_surface_listener, screen);
	if (index + 1 > state->screen_count) {
		state->screen_count = index + 1;
	}
}

static int create_screens(struct state *state, enum configure_action action)
{
	if (state->compositor == NULL || state->shm == NULL ||
			state->output_count == 0) {
		fprintf(stderr, "no wl_compositor, wl_shm, or outputs to draw on\n");
		return -1;
	}
	for (int i = 0; i < state->output_count; i++) {
		init_screen(state, i, action);
	}
	if (sync_display(state) < 0 ||
			sync_display(state) < 0) {
		return -1;
	}
	for (int i = 0; i < state->output_count; i++) {
		if (state->screens[i].draw_failed) {
			fprintf(stderr, "a lock surface could not allocate its buffer\n");
			return -1;
		}
		if (!state->screens[i].configured ||
				(action == CONFIGURE_DRAW && !state->screens[i].drawn)) {
			fprintf(stderr, "a lock surface was never configured\n");
			return -1;
		}
	}
	return 0;
}

/* A refused request is a fatal protocol error for this client. Check the
 * interface and enum value too: dying for an unrelated reason is not a pass. */
static int expect_protocol_error(struct state *state,
	const char *expected_interface, uint32_t expected_code)
{
	if (state->screen_count > 0 && state->screens[0].draw_failed) {
		fprintf(stderr, "the probe could not allocate its buffer\n");
		return 2;
	}
	int64_t deadline = now_ms() + WAIT_MS;
	while (wl_display_get_error(state->display) == 0) {
		/* A configure callback may have queued the offending request on the
		 * previous dispatch. Flush on every pass, not only before the first. */
		short events = POLLIN;
		if (wl_display_flush(state->display) < 0) {
			if (errno != EAGAIN) {
				return 1;
			}
			events |= POLLOUT;
		}
		int remaining = (int)(deadline - now_ms());
		if (remaining <= 0) {
			fprintf(stderr, "compositor neither answered nor killed us\n");
			return 1;
		}
		struct pollfd pfd = {
			.fd = wl_display_get_fd(state->display),
			.events = events,
		};
		int ready = poll(&pfd, 1, remaining);
		if (ready < 0 && errno == EINTR) {
			continue;
		}
		if (ready <= 0) {
			fprintf(stderr, "compositor neither answered nor killed us\n");
			return 1;
		}
		if (wl_display_dispatch(state->display) < 0) {
			break;
		}
	}

	if (wl_display_get_error(state->display) != EPROTO) {
		fprintf(stderr, "connection ended without a protocol error\n");
		return 1;
	}
	const struct wl_interface *interface = NULL;
	uint32_t object_id = 0;
	uint32_t code = wl_display_get_protocol_error(state->display, &interface,
		&object_id);
	const char *name = interface == NULL ? "(none)" : interface->name;
	/* unlock_and_destroy and destroy are destructor requests. libwayland has
	 * already discarded the proxy when their error arrives, so it can report
	 * only "destroyed object" and the enum value, not the interface pointer. */
	bool destroyed_lock = interface == NULL &&
		(expected_code == EXT_SESSION_LOCK_V1_ERROR_INVALID_DESTROY ||
		 expected_code == EXT_SESSION_LOCK_V1_ERROR_INVALID_UNLOCK);
	bool object_matches = true;
	if (!destroyed_lock) {
		struct wl_proxy *expected = strcmp(expected_interface,
			ext_session_lock_v1_interface.name) == 0
			? (struct wl_proxy *)state->lock
			: (struct wl_proxy *)state->screens[0].lock_surface;
		object_matches = object_id == wl_proxy_get_id(expected);
	}
	if (code != expected_code || !object_matches ||
			(!destroyed_lock && strcmp(name, expected_interface) != 0)) {
		fprintf(stderr, "wrong protocol error: %s[%u] code %u; wanted %s %u\n",
			name, object_id, code, expected_interface, expected_code);
		return 1;
	}
	fprintf(stderr, "got %s error %u\n", destroyed_lock ? "destroyed lock" : name,
		code);
	return 0;
}

static bool is_mode(const char *mode)
{
	static const char *const modes[] = {
		"crash", "unlock", "hold", "hold-green", "draw-unlock", "second",
		"second-draw", "unlock-refused", "idle", "idle-surface",
		"stale-unlock", "duplicate-output", "already-attached",
		"already-committed", "role", "destroy-locked", "commit-before-ack",
		"null-buffer", "dimensions-mismatch", "invalid-serial",
	};
	for (size_t i = 0; i < sizeof(modes) / sizeof(modes[0]); i++) {
		if (strcmp(mode, modes[i]) == 0) {
			return true;
		}
	}
	return false;
}

int main(int argc, char *argv[])
{
	if (argc != 2 || !is_mode(argv[1])) {
		fprintf(stderr, "usage: %s MODE (see the mode table at the top)\n",
			argv[0]);
		return 2;
	}
	const char *mode = argv[1];
	bool crash = strcmp(mode, "crash") == 0;
	bool hold = strcmp(mode, "hold") == 0 || strcmp(mode, "hold-green") == 0;
	bool second = strcmp(mode, "second") == 0;
	bool second_draw = strcmp(mode, "second-draw") == 0;
	bool unlock_refused = strcmp(mode, "unlock-refused") == 0;
	bool idle = strcmp(mode, "idle") == 0;
	bool idle_surface = strcmp(mode, "idle-surface") == 0;
	bool stale_unlock = strcmp(mode, "stale-unlock") == 0;
	bool draw_unlock = strcmp(mode, "draw-unlock") == 0;

	struct state state = { .colour = 0xff0000ff };
	if (strcmp(mode, "hold-green") == 0) {
		state.colour = 0xff00ff00;
	} else if (second_draw || idle_surface || stale_unlock) {
		state.colour = 0xffff0000;
	}

	state.display = wl_display_connect(NULL);
	if (state.display == NULL) {
		fprintf(stderr, "cannot connect to WAYLAND_DISPLAY\n");
		return 2;
	}

	struct wl_registry *registry = wl_display_get_registry(state.display);
	wl_registry_add_listener(registry, &registry_listener, &state);
	sync_display(&state);
	/* The seat's capabilities arrive after the registry globals. */
	sync_display(&state);

	if (state.manager == NULL) {
		fprintf(stderr, "compositor does not offer ext-session-lock-v1\n");
		return 2;
	}

	state.lock = ext_session_lock_manager_v1_lock(state.manager);
	ext_session_lock_v1_add_listener(state.lock, &lock_listener, &state);
	if (wait_for_flags(&state, &state.locked, &state.finished) < 0) {
		fprintf(stderr, "disconnected or timed out before the lock answer\n");
		return 2;
	}

	/* A server may send locked and then finished. Settle the ordered events
	 * before calling a second lock granted rather than refused. */
	if ((second || second_draw || unlock_refused) && state.locked &&
			sync_display(&state) < 0) {
		return 2;
	}

	if (second) {
		if (state.finished) {
			fprintf(stderr, "refused, as it should be\n");
			return 0;
		}
		fprintf(stderr, "the compositor granted a second lock\n");
		return 1;
	}

	if (second_draw) {
		if (!state.finished) {
			fprintf(stderr, "a second lock was granted before its hostile draw\n");
			return 1;
		}
		if (state.compositor == NULL || state.shm == NULL ||
				state.output_count == 0) {
			fprintf(stderr, "the refused-draw probe needs compositor, shm, and output\n");
			return 2;
		}
		/* Smithay does not configure a lock it has finished. Try to draw
		 * anyway: the role's own refusal must cost only this connection. */
		init_screen(&state, 0, CONFIGURE_MANUAL);
		struct wl_buffer *buffer = make_buffer(&state, 1, 1);
		if (buffer == NULL) {
			return 2;
		}
		wl_surface_attach(state.screens[0].surface, buffer, 0, 0);
		wl_surface_commit(state.screens[0].surface);
		return expect_protocol_error(&state,
			ext_session_lock_surface_v1_interface.name,
			EXT_SESSION_LOCK_SURFACE_V1_ERROR_COMMIT_BEFORE_FIRST_ACK);
	}

	if (unlock_refused) {
		if (!state.finished) {
			fprintf(stderr, "the lock was not refused before its early unlock\n");
			return 1;
		}
		ext_session_lock_v1_unlock_and_destroy(state.lock);
		return expect_protocol_error(&state, ext_session_lock_v1_interface.name,
			EXT_SESSION_LOCK_V1_ERROR_INVALID_UNLOCK);
	}

	if (state.finished) {
		fprintf(stderr, "the compositor refused the lock\n");
		return 2;
	}

	if (idle) {
		fprintf(stderr, "idle lock ready with no surface\n");
		if (wait_for_flags(&state, &state.finished, NULL) < 0) {
			fprintf(stderr, "the superseded locker never got finished\n");
			return 1;
		}
		return 0;
	}

	if (idle_surface || stale_unlock) {
		if (create_screens(&state, CONFIGURE_ACK) < 0) {
			return 2;
		}
		fprintf(stderr, "idle lock ready with configured, empty surface(s)\n");
		if (wait_for_flags(&state, &state.finished, NULL) < 0) {
			fprintf(stderr, "the superseded locker never got finished\n");
			return 1;
		}
		/* These configures were acknowledged while this lock was accepted,
		 * so this is a legal draw after supersession, not a commit error. */
		for (int i = 0; i < state.screen_count; i++) {
			if (draw_screen(&state.screens[i]) < 0) {
				return 2;
			}
		}
		if (sync_display(&state) < 0) {
			fprintf(stderr, "the stale draw ended the connection\n");
			return 1;
		}
		fprintf(stderr, "superseded surface drew red\n");
		if (stale_unlock) {
			ext_session_lock_v1_unlock_and_destroy(state.lock);
			return expect_protocol_error(&state,
				ext_session_lock_v1_interface.name,
				EXT_SESSION_LOCK_V1_ERROR_INVALID_UNLOCK);
		}
		while (wl_display_dispatch(state.display) >= 0) {
		}
		fprintf(stderr, "the holding locker was disconnected unexpectedly\n");
		return 1;
	}

	if (hold || draw_unlock) {
		if (create_screens(&state, CONFIGURE_DRAW) < 0) {
			return 2;
		}
		fprintf(stderr, "locked and drawing on %d output(s)\n",
			state.output_count);
		if (draw_unlock) {
			ext_session_lock_v1_unlock_and_destroy(state.lock);
			if (sync_display(&state) < 0) {
				fprintf(stderr, "unlock after drawing failed\n");
				return 1;
			}
			fprintf(stderr, "drew and unlocked cleanly\n");
			return 0;
		}
		while (wl_display_dispatch(state.display) >= 0) {
		}
		fprintf(stderr, "the holding locker was disconnected unexpectedly\n");
		return 1;
	}

	if (crash) {
		/* No unlock, no destroy, no flush of a polite goodbye — just go, the
		 * way a crash does. The compositor sees the connection drop. */
		fprintf(stderr, "locked; exiting without unlocking\n");
		_exit(0);
	}

	if (strcmp(mode, "unlock") == 0) {
		ext_session_lock_v1_unlock_and_destroy(state.lock);
		if (sync_display(&state) < 0) {
			return 1;
		}
		fprintf(stderr, "locked and unlocked cleanly\n");
		return 0;
	}

	if (state.output_count == 0 || state.shm == NULL) {
		fprintf(stderr, "the probe needs an output and wl_shm\n");
		return 2;
	}

	if (strcmp(mode, "duplicate-output") == 0) {
		init_screen(&state, 0, CONFIGURE_ACK);
		struct wl_surface *extra = wl_compositor_create_surface(state.compositor);
		ext_session_lock_v1_get_lock_surface(state.lock, extra,
			state.outputs[0]);
		return expect_protocol_error(&state, ext_session_lock_v1_interface.name,
			EXT_SESSION_LOCK_V1_ERROR_DUPLICATE_OUTPUT);
	}

	if (strcmp(mode, "already-attached") == 0 ||
			strcmp(mode, "already-committed") == 0) {
		struct wl_surface *surface = wl_compositor_create_surface(state.compositor);
		struct wl_buffer *buffer = make_buffer(&state, 1, 1);
		if (buffer == NULL) {
			return 2;
		}
		wl_surface_attach(surface, buffer, 0, 0);
		if (strcmp(mode, "already-committed") == 0) {
			/* No roundtrip: role construction in the same batch is the
			 * ordering that used to lose the committed buffer. */
			wl_surface_commit(surface);
		}
		ext_session_lock_v1_get_lock_surface(state.lock, surface,
			state.outputs[0]);
		return expect_protocol_error(&state, ext_session_lock_v1_interface.name,
			EXT_SESSION_LOCK_V1_ERROR_ALREADY_CONSTRUCTED);
	}

	if (strcmp(mode, "role") == 0) {
		if (state.wm_base == NULL) {
			fprintf(stderr, "no xdg_wm_base for the role probe\n");
			return 2;
		}
		struct wl_surface *surface = wl_compositor_create_surface(state.compositor);
		struct xdg_surface *xdg_surface = xdg_wm_base_get_xdg_surface(
			state.wm_base, surface);
		xdg_surface_get_toplevel(xdg_surface);
		ext_session_lock_v1_get_lock_surface(state.lock, surface,
			state.outputs[0]);
		return expect_protocol_error(&state, ext_session_lock_v1_interface.name,
			EXT_SESSION_LOCK_V1_ERROR_ROLE);
	}

	if (strcmp(mode, "destroy-locked") == 0) {
		ext_session_lock_v1_destroy(state.lock);
		return expect_protocol_error(&state, ext_session_lock_v1_interface.name,
			EXT_SESSION_LOCK_V1_ERROR_INVALID_DESTROY);
	}

	if (strcmp(mode, "commit-before-ack") == 0) {
		init_screen(&state, 0, CONFIGURE_MANUAL);
		if (wait_for_flags(&state, &state.screens[0].configured, NULL) < 0 ||
				draw_screen(&state.screens[0]) < 0) {
			return 2;
		}
		return expect_protocol_error(&state,
			ext_session_lock_surface_v1_interface.name,
			EXT_SESSION_LOCK_SURFACE_V1_ERROR_COMMIT_BEFORE_FIRST_ACK);
	}

	if (strcmp(mode, "null-buffer") == 0) {
		init_screen(&state, 0, CONFIGURE_NULL_BUFFER);
		return expect_protocol_error(&state,
			ext_session_lock_surface_v1_interface.name,
			EXT_SESSION_LOCK_SURFACE_V1_ERROR_NULL_BUFFER);
	}

	if (strcmp(mode, "dimensions-mismatch") == 0) {
		init_screen(&state, 0, CONFIGURE_WRONG_SIZE);
		return expect_protocol_error(&state,
			ext_session_lock_surface_v1_interface.name,
			EXT_SESSION_LOCK_SURFACE_V1_ERROR_DIMENSIONS_MISMATCH);
	}

	if (strcmp(mode, "invalid-serial") == 0) {
		init_screen(&state, 0, CONFIGURE_BAD_SERIAL);
		return expect_protocol_error(&state,
			ext_session_lock_surface_v1_interface.name,
			EXT_SESSION_LOCK_SURFACE_V1_ERROR_INVALID_SERIAL);
	}

	fprintf(stderr, "unhandled mode %s\n", mode);
	return 2;
}
