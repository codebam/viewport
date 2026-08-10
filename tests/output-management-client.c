/* SPDX-License-Identifier: MIT
 *
 * Drive wlr-output-management-v1 the way wlr-randr and kanshi do.
 *
 * wlr-output-management-v1 is the protocol monitor-configuration tools speak.
 * The compositor advertises every head and mode as its own object and stamps
 * the set with a serial; a client builds a configuration against that serial
 * and asks to test or apply it.
 *
 *   output-management-client   advertise, configure, apply, test; pass
 *
 * This client:
 *
 *   - binds zwlr_output_manager_v1 v4 and waits for a `head` and the
 *     `done(serial)` that ends the advertisement — the serial a
 *     configuration must be built against;
 *   - create_configuration(serial) -> enable_head(head) -> apply, and
 *     requires `succeeded` (not failed or cancelled);
 *   - does the same again with `test`, which must also `succeed` — a test
 *     applies nothing but still validates the request path end to end.
 *
 * Exits 0 on success, 2 if the compositor does not offer the global, 1 on
 * any protocol failure.
 */
#define _GNU_SOURCE

#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <wayland-client.h>

#include "wlr-output-management-unstable-v1-client-protocol.h"

struct state {
	struct zwlr_output_manager_v1 *manager;
	struct zwlr_output_head_v1 *head;
	bool head_seen;
	uint32_t serial;
	bool done;
	uint32_t config_result; /* 0 none, 1 succeeded, 2 failed, 3 cancelled */
};

/* Dispatch until `flag` is set, with a deadline so a compositor that never
 * answers fails the test instead of hanging it. */
static int dispatch_until(struct wl_display *display, bool *flag, int timeout_ms)
{
	struct timespec start;
	clock_gettime(CLOCK_MONOTONIC, &start);
	while (!*flag) {
		if (wl_display_dispatch(display) < 0) {
			return -1;
		}
		struct timespec now;
		clock_gettime(CLOCK_MONOTONIC, &now);
		long elapsed_ms = (now.tv_sec - start.tv_sec) * 1000
			+ (now.tv_nsec - start.tv_nsec) / 1000000;
		if (elapsed_ms > timeout_ms) {
			return -1;
		}
	}
	return 0;
}

static void handle_config_succeeded(void *data,
	struct zwlr_output_configuration_v1 *configuration)
{
	struct state *state = data;
	(void)configuration;
	state->config_result = 1;
}

static void handle_config_failed(void *data,
	struct zwlr_output_configuration_v1 *configuration)
{
	struct state *state = data;
	(void)configuration;
	state->config_result = 2;
}

static void handle_config_cancelled(void *data,
	struct zwlr_output_configuration_v1 *configuration)
{
	struct state *state = data;
	(void)configuration;
	state->config_result = 3;
}

static const struct zwlr_output_configuration_v1_listener config_listener = {
	.succeeded = handle_config_succeeded,
	.failed = handle_config_failed,
	.cancelled = handle_config_cancelled,
};

/* One configuration round: enable the first advertised head, then finish with
 * apply or test, and require `succeeded`. */
static int configure(struct wl_display *display, struct state *state,
	bool test_only)
{
	state->config_result = 0;

	struct zwlr_output_configuration_v1 *config =
		zwlr_output_manager_v1_create_configuration(state->manager,
			state->serial);
	zwlr_output_configuration_v1_add_listener(config, &config_listener,
		state);

	struct zwlr_output_configuration_head_v1 *config_head =
		zwlr_output_configuration_v1_enable_head(config, state->head);
	if (config_head == NULL) {
		fprintf(stderr, "enable_head did not create a config head\n");
		return 1;
	}

	if (test_only) {
		zwlr_output_configuration_v1_test(config);
	} else {
		zwlr_output_configuration_v1_apply(config);
	}

	/* Loop the ordinary way: `succeeded`/`failed`/`cancelled` is the
	 * compositor's answer and it must arrive within the deadline. */
	struct timespec start;
	clock_gettime(CLOCK_MONOTONIC, &start);
	while (state->config_result == 0) {
		if (wl_display_dispatch(display) < 0) {
			fprintf(stderr, "disconnected waiting for the answer\n");
			return 1;
		}
		struct timespec now;
		clock_gettime(CLOCK_MONOTONIC, &now);
		long elapsed_ms = (now.tv_sec - start.tv_sec) * 1000
			+ (now.tv_nsec - start.tv_nsec) / 1000000;
		if (elapsed_ms > 5000) {
			fprintf(stderr, "no answer to the configuration\n");
			return 1;
		}
	}

	if (state->config_result == 2) {
		fprintf(stderr, "the compositor refused the configuration\n");
		return 1;
	}
	if (state->config_result == 3) {
		fprintf(stderr, "the configuration was cancelled\n");
		return 1;
	}
	return 0;
}

static void handle_manager_head(void *data,
	struct zwlr_output_manager_v1 *manager,
	struct zwlr_output_head_v1 *head)
{
	struct state *state = data;
	(void)manager;
	if (!state->head_seen) {
		state->head_seen = true;
		state->head = head;
	}
}

static void handle_manager_done(void *data,
	struct zwlr_output_manager_v1 *manager, uint32_t serial)
{
	struct state *state = data;
	(void)manager;
	state->serial = serial;
	state->done = true;
}

static void handle_manager_finished(void *data,
	struct zwlr_output_manager_v1 *manager)
{
	(void)data;
	(void)manager;
}

static const struct zwlr_output_manager_v1_listener manager_listener = {
	.head = handle_manager_head,
	.done = handle_manager_done,
	.finished = handle_manager_finished,
};

static void handle_global(void *data, struct wl_registry *registry,
	uint32_t name, const char *interface, uint32_t version)
{
	struct state *state = data;

	if (strcmp(interface, zwlr_output_manager_v1_interface.name) == 0) {
		state->manager = wl_registry_bind(registry, name,
			&zwlr_output_manager_v1_interface, 4);
		zwlr_output_manager_v1_add_listener(state->manager,
			&manager_listener, state);
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
			"compositor does not offer zwlr_output_manager_v1\n");
		wl_display_disconnect(display);
		return 2;
	}

	if (dispatch_until(display, &state.done, 5000) < 0) {
		fprintf(stderr, "no done(serial) arrived from the compositor\n");
		wl_display_disconnect(display);
		return 1;
	}
	if (!state.head_seen) {
		fprintf(stderr, "no head was advertised\n");
		wl_display_disconnect(display);
		return 1;
	}
	if (state.serial == 0) {
		fprintf(stderr, "the compositor sent serial 0\n");
		wl_display_disconnect(display);
		return 1;
	}
	printf("ok   advertised a head and done(serial=%u)\n", state.serial);

	if (configure(display, &state, false) != 0) {
		wl_display_disconnect(display);
		return 1;
	}
	printf("ok   apply of enable_head succeeded\n");

	if (configure(display, &state, true) != 0) {
		wl_display_disconnect(display);
		return 1;
	}
	printf("ok   test of enable_head succeeded\n");

	wl_display_disconnect(display);
	return 0;
}