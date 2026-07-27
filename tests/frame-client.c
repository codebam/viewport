/* SPDX-License-Identifier: MIT
 *
 * Report when the compositor stops presenting.
 *
 * The session freezes in a way that nothing so far can see. The event loop
 * stays responsive — a control-socket round trip still answers in a fifth of a
 * millisecond — and the log keeps recording keystrokes, while the screen shows
 * the same picture for a minute at a time. Probing the loop therefore proves
 * nothing: it is alive, and presentation is what has stopped.
 *
 * A frame callback is the one thing that measures presentation from outside.
 * The compositor sends it when it has finished with a surface's buffer and the
 * client may draw again, which happens once per composited frame and stops
 * dead the moment output frames stop. So: attach a tiny surface, ask for a
 * callback, draw, ask again, and print whenever the gap between callbacks is
 * longer than it should be.
 *
 *   frame-client                    report gaps over 250ms, first output
 *   frame-client 1000               report gaps over 1s
 *   frame-client 500 DP-3           watch a named output
 *
 * The surface is a layer-shell overlay pinned to one output, not an ordinary
 * window, and that is the whole point. The first version of this was an
 * xdg_toplevel, which a fullscreen game covers — the compositor then correctly
 * stops sending it frame callbacks, and the probe reported a two-minute gap
 * during a benchmark that was visibly running at 246fps. It was measuring its
 * own occlusion. On the overlay layer it stays composited, and pinned to the
 * monitor the game is not on it keeps reporting throughout.
 *
 * Runs until killed. One line per gap, plus a summary every ten seconds, so a
 * session can be left running beside it and the output read afterwards.
 */
#define _GNU_SOURCE

#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#include <wayland-client.h>

#include "wlr-layer-shell-unstable-v1-client-protocol.h"
#include "xdg-shell-client-protocol.h"

#define MAX_OUTPUTS 8

struct output_entry {
	struct wl_output *output;
	char name[64];
};

struct state {
	struct wl_compositor *compositor;
	struct wl_shm *shm;
	struct zwlr_layer_shell_v1 *layer_shell;
	struct wl_surface *surface;
	struct wl_buffer *buffer;
	struct output_entry outputs[MAX_OUTPUTS];
	size_t outputs_len;
	const char *want_output;
	bool configured;
	bool running;

	int64_t last_frame_us;
	int64_t worst_gap_us;
	int64_t report_us;
	unsigned frames;
	unsigned gaps;
};

static int64_t now_us(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (int64_t)ts.tv_sec * 1000000 + ts.tv_nsec / 1000;
}

static void handle_output_geometry(void *data, struct wl_output *output,
	int32_t x, int32_t y, int32_t pw, int32_t ph, int32_t subpixel,
	const char *make, const char *model, int32_t transform)
{
}

static void handle_output_mode(void *data, struct wl_output *output,
	uint32_t flags, int32_t w, int32_t h, int32_t refresh)
{
}

static void handle_output_done(void *data, struct wl_output *output)
{
}

static void handle_output_scale(void *data, struct wl_output *output,
	int32_t factor)
{
}

/* wl_output version 4 reports the connector name, which is what makes
 * "watch DP-3" possible rather than guessing at an index. */
static void handle_output_name(void *data, struct wl_output *output,
	const char *name)
{
	struct output_entry *entry = data;
	snprintf(entry->name, sizeof(entry->name), "%s", name);
}

static void handle_output_description(void *data, struct wl_output *output,
	const char *description)
{
}

static const struct wl_output_listener output_listener = {
	.geometry = handle_output_geometry,
	.mode = handle_output_mode,
	.done = handle_output_done,
	.scale = handle_output_scale,
	.name = handle_output_name,
	.description = handle_output_description,
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
	} else if (strcmp(interface, zwlr_layer_shell_v1_interface.name) == 0) {
		state->layer_shell = wl_registry_bind(registry, name,
			&zwlr_layer_shell_v1_interface, 1);
	} else if (strcmp(interface, wl_output_interface.name) == 0) {
		if (state->outputs_len >= MAX_OUTPUTS) {
			return;
		}
		struct output_entry *entry = &state->outputs[state->outputs_len++];
		uint32_t want = version < 4 ? version : 4;
		entry->output = wl_registry_bind(registry, name, &wl_output_interface,
			want);
		if (want >= 4) {
			wl_output_add_listener(entry->output, &output_listener, entry);
		}
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

static void handle_layer_configure(void *data,
	struct zwlr_layer_surface_v1 *surface, uint32_t serial,
	uint32_t width, uint32_t height)
{
	struct state *state = data;
	zwlr_layer_surface_v1_ack_configure(surface, serial);
	state->configured = true;
}

static void handle_layer_closed(void *data,
	struct zwlr_layer_surface_v1 *surface)
{
	struct state *state = data;
	state->running = false;
}

static const struct zwlr_layer_surface_v1_listener layer_listener = {
	.configure = handle_layer_configure,
	.closed = handle_layer_closed,
};

static const struct wl_callback_listener frame_listener;

static void handle_frame(void *data, struct wl_callback *callback,
	uint32_t time)
{
	struct state *state = data;
	wl_callback_destroy(callback);

	int64_t now = now_us();
	if (state->last_frame_us != 0) {
		int64_t gap = now - state->last_frame_us;
		if (gap > state->report_us) {
			state->gaps++;
			if (gap > state->worst_gap_us) {
				state->worst_gap_us = gap;
			}
			struct timespec rt;
			clock_gettime(CLOCK_REALTIME, &rt);
			time_t t = rt.tv_sec;
			struct tm tm;
			localtime_r(&t, &tm);
			char stamp[32];
			strftime(stamp, sizeof(stamp), "%H:%M:%S", &tm);
			printf("%s  NO FRAME for %.2fs\n", stamp, gap / 1e6);
			fflush(stdout);
		}
	}
	state->last_frame_us = now;
	state->frames++;

	/* Ask again and commit, so there is always one outstanding. */
	struct wl_callback *next = wl_surface_frame(state->surface);
	wl_callback_add_listener(next, &frame_listener, state);
	wl_surface_damage_buffer(state->surface, 0, 0, 1, 1);
	wl_surface_commit(state->surface);
}

static const struct wl_callback_listener frame_listener = {
	.done = handle_frame,
};

int main(int argc, char *argv[])
{
	struct state state = {
		.running = true,
		.report_us = (argc > 1 ? atoi(argv[1]) : 250) * 1000,
		.want_output = argc > 2 ? argv[2] : NULL,
	};

	struct wl_display *display = wl_display_connect(NULL);
	if (display == NULL) {
		fprintf(stderr, "cannot connect to WAYLAND_DISPLAY\n");
		return 2;
	}

	struct wl_registry *registry = wl_display_get_registry(display);
	wl_registry_add_listener(registry, &registry_listener, &state);
	/* Twice: the second settles the wl_output name events. */
	wl_display_roundtrip(display);
	wl_display_roundtrip(display);

	if (state.compositor == NULL || state.shm == NULL ||
			state.layer_shell == NULL) {
		fprintf(stderr, "compositor is missing wl_compositor, wl_shm or "
			"zwlr_layer_shell_v1\n");
		return 2;
	}

	struct wl_output *target = NULL;
	if (state.want_output != NULL) {
		for (size_t i = 0; i < state.outputs_len; i++) {
			if (strcmp(state.outputs[i].name, state.want_output) == 0) {
				target = state.outputs[i].output;
				break;
			}
		}
		if (target == NULL) {
			fprintf(stderr, "no output named '%s'; saw:", state.want_output);
			for (size_t i = 0; i < state.outputs_len; i++) {
				fprintf(stderr, " %s", state.outputs[i].name[0]
					? state.outputs[i].name : "(unnamed)");
			}
			fprintf(stderr, "\n");
			return 2;
		}
	}

	/* One small opaque buffer, drawn once. What is in it does not matter —
	 * only that the surface is mapped, so the compositor keeps sending frame
	 * callbacks for it. */
	int width = 64, height = 64, stride = width * 4;
	int size = stride * height;
	int fd = memfd_create("frame-client", MFD_CLOEXEC);
	if (fd < 0 || ftruncate(fd, size) < 0) {
		fprintf(stderr, "cannot allocate a buffer\n");
		return 2;
	}
	uint32_t *pixels = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED,
		fd, 0);
	if (pixels == MAP_FAILED) {
		fprintf(stderr, "cannot map the buffer\n");
		return 2;
	}
	for (int i = 0; i < width * height; i++) {
		pixels[i] = 0xFF101010;
	}
	struct wl_shm_pool *pool = wl_shm_create_pool(state.shm, fd, size);
	state.buffer = wl_shm_pool_create_buffer(pool, 0, width, height, stride,
		WL_SHM_FORMAT_ARGB8888);
	wl_shm_pool_destroy(pool);

	state.surface = wl_compositor_create_surface(state.compositor);

	/* Overlay layer, pinned to one output. Above app windows, so a fullscreen
	 * game does not occlude it and the callbacks keep coming; on the output
	 * named rather than wherever the compositor would have put it, so it can
	 * be kept off the monitor under test. */
	struct zwlr_layer_surface_v1 *layer = zwlr_layer_shell_v1_get_layer_surface(
		state.layer_shell, state.surface, target,
		ZWLR_LAYER_SHELL_V1_LAYER_OVERLAY, "viewport-frame-probe");
	zwlr_layer_surface_v1_add_listener(layer, &layer_listener, &state);
	zwlr_layer_surface_v1_set_size(layer, width, height);
	zwlr_layer_surface_v1_set_anchor(layer,
		ZWLR_LAYER_SURFACE_V1_ANCHOR_BOTTOM |
		ZWLR_LAYER_SURFACE_V1_ANCHOR_RIGHT);
	/* No exclusive zone: the probe must not shrink the desktop it is
	 * measuring. */
	zwlr_layer_surface_v1_set_exclusive_zone(layer, -1);
	zwlr_layer_surface_v1_set_keyboard_interactivity(layer, 0);
	wl_surface_commit(state.surface);

	while (!state.configured && wl_display_dispatch(display) >= 0) {
	}

	wl_surface_attach(state.surface, state.buffer, 0, 0);
	struct wl_callback *first = wl_surface_frame(state.surface);
	wl_callback_add_listener(first, &frame_listener, &state);
	wl_surface_commit(state.surface);

	fprintf(stderr, "watching frame callbacks on %s; gaps over %lldms\n",
		state.want_output ? state.want_output : "the compositor's choice",
		(long long)(state.report_us / 1000));

	int64_t last_report = now_us();
	while (state.running) {
		if (wl_display_dispatch(display) < 0) {
			break;
		}
		int64_t now = now_us();
		if (now - last_report > 10 * 1000000) {
			double secs = (now - last_report) / 1e6;
			fprintf(stderr, "  %.0f fps over %.0fs, %u gaps this interval, "
				"worst %.2fs\n", state.frames / secs, secs, state.gaps,
				state.worst_gap_us / 1e6);
			state.frames = 0;
			state.gaps = 0;
			state.worst_gap_us = 0;
			last_report = now;
		}
	}

	wl_display_disconnect(display);
	return 0;
}
