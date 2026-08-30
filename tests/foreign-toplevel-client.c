/* SPDX-License-Identifier: MIT
 *
 * Drive wlr-foreign-toplevel-management-v1 from a client and watch it act.
 *
 * wlr-foreign-toplevel-management-v1 is what a taskbar or `rofi -show
 * window` uses to list the windows and act on them. The listing is the
 * read-only half (also published by ext-foreign-toplevel-list); acting is
 * specific to this protocol, so this drives the requests a taskbar would and
 * checks the compositor carries them out.
 *
 *   foreign-toplevel-client   list, activate, maximize, fullscreen, close; pass
 *
 * The script that runs it starts a compositor and a paint client first, so
 * there is one window to see. This client:
 *
 *   - binds zwlr_foreign_toplevel_manager_v1 v3 and waits for a `toplevel`
 *     handle — the listing part;
 *   - sends maximize/fullscreen requests and checks each state is published
 *     back, then sends activate;
 *   - sends close() and waits for the `closed` event on the handle — the
 *     compositor acting: it forwarded close to the window client, and once
 *     that window goes away the handle must be told.
 *
 * Exits 0 on success, 2 if the compositor does not offer the global, 1 if
 * the compositor failed to act within the timeout.
 */
#define _GNU_SOURCE

#include <errno.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <poll.h>
#include <time.h>
#include <unistd.h>

#include <wayland-client.h>

#include "wlr-foreign-toplevel-management-unstable-v1-client-protocol.h"

struct state {
	struct zwlr_foreign_toplevel_manager_v1 *manager;
	struct zwlr_foreign_toplevel_handle_v1 *toplevel;
	struct wl_seat *seat;
	bool toplevel_seen;
	bool closed;
	bool state_seen;
	bool maximized;
	bool fullscreen;
};

static void handle_title(void *data,
	struct zwlr_foreign_toplevel_handle_v1 *toplevel, const char *title)
{
	(void)data;
	(void)toplevel;
	(void)title;
}

static void handle_app_id(void *data,
	struct zwlr_foreign_toplevel_handle_v1 *toplevel, const char *app_id)
{
	(void)data;
	(void)toplevel;
	(void)app_id;
}

static void handle_output_enter(void *data,
	struct zwlr_foreign_toplevel_handle_v1 *toplevel, struct wl_output *output)
{
	(void)data;
	(void)toplevel;
	(void)output;
}

static void handle_output_leave(void *data,
	struct zwlr_foreign_toplevel_handle_v1 *toplevel, struct wl_output *output)
{
	(void)data;
	(void)toplevel;
	(void)output;
}

static void handle_state(void *data,
	struct zwlr_foreign_toplevel_handle_v1 *toplevel, struct wl_array *states)
{
	struct state *state = data;
	uint32_t *value;
	(void)toplevel;
	state->maximized = false;
	state->fullscreen = false;
	wl_array_for_each(value, states) {
		if (*value == ZWLR_FOREIGN_TOPLEVEL_HANDLE_V1_STATE_MAXIMIZED) {
			state->maximized = true;
		} else if (*value == ZWLR_FOREIGN_TOPLEVEL_HANDLE_V1_STATE_FULLSCREEN) {
			state->fullscreen = true;
		}
	}
	state->state_seen = true;
}

static void handle_done(void *data,
	struct zwlr_foreign_toplevel_handle_v1 *toplevel)
{
	(void)data;
	(void)toplevel;
}

static void handle_closed(void *data,
	struct zwlr_foreign_toplevel_handle_v1 *toplevel)
{
	struct state *state = data;
	(void)toplevel;
	state->closed = true;
}

static void handle_parent(void *data,
	struct zwlr_foreign_toplevel_handle_v1 *toplevel,
	struct zwlr_foreign_toplevel_handle_v1 *parent)
{
	(void)data;
	(void)toplevel;
	(void)parent;
}

static const struct zwlr_foreign_toplevel_handle_v1_listener handle_listener = {
	.title = handle_title,
	.app_id = handle_app_id,
	.output_enter = handle_output_enter,
	.output_leave = handle_output_leave,
	.state = handle_state,
	.done = handle_done,
	.closed = handle_closed,
	.parent = handle_parent,
};

static void handle_toplevel(void *data,
	struct zwlr_foreign_toplevel_manager_v1 *manager,
	struct zwlr_foreign_toplevel_handle_v1 *toplevel)
{
	struct state *state = data;
	(void)manager;
	if (state->toplevel_seen) {
		return;
	}
	state->toplevel_seen = true;
	state->toplevel = toplevel;
	zwlr_foreign_toplevel_handle_v1_add_listener(toplevel, &handle_listener,
		data);
}

static void handle_finished(void *data,
	struct zwlr_foreign_toplevel_manager_v1 *manager)
{
	(void)data;
	(void)manager;
}

static const struct zwlr_foreign_toplevel_manager_v1_listener manager_listener = {
	.toplevel = handle_toplevel,
	.finished = handle_finished,
};

static void handle_global(void *data, struct wl_registry *registry,
	uint32_t name, const char *interface, uint32_t version)
{
	struct state *state = data;

	if (strcmp(interface,
			zwlr_foreign_toplevel_manager_v1_interface.name) == 0) {
		state->manager = wl_registry_bind(registry, name,
			&zwlr_foreign_toplevel_manager_v1_interface, 3);
		zwlr_foreign_toplevel_manager_v1_add_listener(state->manager,
			&manager_listener, state);
	} else if (strcmp(interface, wl_seat_interface.name) == 0) {
		state->seat = wl_registry_bind(registry, name,
			&wl_seat_interface, 1);
	}
}

static void handle_global_remove(void *data, struct wl_registry *registry,
	uint32_t name)
{
	(void)data;
	(void)registry;
	(void)name;
}

static const struct wl_registry_listener registry_listener = {
	.global = handle_global,
	.global_remove = handle_global_remove,
};

/* Dispatch with a deadline, rather than blocking on a quiet socket: nothing
 * more arrives once the interesting events have been read. Returns 0 if the
 * deadline was reached without error, -1 on a connection error. */
static int dispatch_until(struct wl_display *display, int timeout_ms)
{
	struct timespec start;
	clock_gettime(CLOCK_MONOTONIC, &start);

	/* Events may already be queued from the last read. */
	if (wl_display_dispatch_pending(display) < 0) {
		return -1;
	}
	if (wl_display_flush(display) < 0) {
		return -1;
	}

	while (true) {
		struct timespec now;
		clock_gettime(CLOCK_MONOTONIC, &now);
		long elapsed_ms = (now.tv_sec - start.tv_sec) * 1000
			+ (now.tv_nsec - start.tv_nsec) / 1000000;
		int remain = timeout_ms - (int)elapsed_ms;
		if (remain <= 0) {
			return 0;
		}

		if (wl_display_prepare_read(display) != 0) {
			if (wl_display_dispatch_pending(display) < 0) {
				return -1;
			}
			continue;
		}
		struct pollfd fds[1];
		fds[0].fd = wl_display_get_fd(display);
		fds[0].events = POLLIN;
		fds[0].revents = 0;

		int r = poll(fds, 1, remain);
		if (r < 0) {
			if (errno == EINTR) {
				wl_display_cancel_read(display);
				continue;
			}
			wl_display_cancel_read(display);
			return -1;
		}
		if (r == 0) {
			/* Deadline in this poll — the caller checks its flags. */
			wl_display_cancel_read(display);
			return 0;
		}
		if (wl_display_read_events(display) < 0) {
			return -1;
		}
		if (wl_display_dispatch_pending(display) < 0) {
			return -1;
		}
	}
}

int main(void)
{
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
		fprintf(stderr,
			"compositor does not offer zwlr_foreign_toplevel_manager_v1\n");
		wl_display_disconnect(display);
		return 2;
	}

	/* The script has already started a paint client, so the listing must
	 * contain at least one handle. Binding publishes existing windows, and
	 * a window that maps after us is published too. */
	dispatch_until(display, 5000);
	if (!state.toplevel_seen) {
		fprintf(stderr, "no toplevel was listed by the compositor\n");
		wl_display_disconnect(display);
		return 1;
	}
	printf("ok   the compositor listed a toplevel\n");

	/* Requests a taskbar makes. Each state must reach both the managed client
	 * and every observer, including this handle. */
	zwlr_foreign_toplevel_handle_v1_set_fullscreen(state.toplevel, NULL);
	wl_display_roundtrip(display);
	if (!state.fullscreen) {
		fprintf(stderr, "fullscreen state was not published\n");
		wl_display_disconnect(display);
		return 1;
	}
	zwlr_foreign_toplevel_handle_v1_unset_fullscreen(state.toplevel);
	wl_display_roundtrip(display);
	if (state.fullscreen) {
		fprintf(stderr, "fullscreen state was not cleared\n");
		wl_display_disconnect(display);
		return 1;
	}
	zwlr_foreign_toplevel_handle_v1_set_maximized(state.toplevel);
	wl_display_roundtrip(display);
	if (!state.maximized) {
		fprintf(stderr, "maximized state was not published\n");
		wl_display_disconnect(display);
		return 1;
	}
	zwlr_foreign_toplevel_handle_v1_unset_maximized(state.toplevel);
	wl_display_roundtrip(display);
	if (state.maximized) {
		fprintf(stderr, "maximized state was not cleared\n");
		wl_display_disconnect(display);
		return 1;
	}
	/* A NULL seat marshals as an invalid argument, so bind a real one from
	 * the registry rather than passing NULL — activate(seat) must carry a
	 * valid seat handle. */
	zwlr_foreign_toplevel_handle_v1_activate(state.toplevel, state.seat);
	dispatch_until(display, 500);
	printf("ok   maximize and fullscreen state changes were published\n");
	printf("ok   activate request was accepted\n");

	/* The observable act: close() must reach the window, and when it goes
	 * the handle must be told with `closed`. */
	zwlr_foreign_toplevel_handle_v1_close(state.toplevel);
	wl_display_flush(display);
	dispatch_until(display, 5000);
	if (!state.closed) {
		fprintf(stderr,
			"the compositor did not report the toplevel as closed\n");
		wl_display_disconnect(display);
		return 1;
	}
	printf("ok   close() closed the window and the handle said closed\n");

	wl_display_disconnect(display);
	return 0;
}
