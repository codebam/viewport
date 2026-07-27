/* SPDX-License-Identifier: MIT
 *
 * A window that is easy to recognise in a capture.
 *
 * The point of this client is the gap between its surface and its window. Like
 * every real client that draws its own decorations, it paints a surface larger
 * than the window it appears to be and declares the window inside it with
 * xdg_surface.set_window_geometry. The two regions get different colours, so a
 * capture that includes the margin, or that misses the window, says so in its
 * pixels rather than in a judgement call about whether the picture looks right.
 *
 * It ignores the size in the configure it is sent. A client is allowed to
 * commit a size of its own choosing, and a test that resized on request would
 * be measuring the shell's layout instead of the capture.
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

#include "xdg-shell-client-protocol.h"

struct paint_client {
	struct wl_compositor *compositor;
	struct wl_shm *shm;
	struct xdg_wm_base *wm_base;

	struct wl_surface *surface;
	struct xdg_surface *xdg_surface;
	struct xdg_toplevel *xdg_toplevel;

	const char *app_id;
	int width, height, margin;
	uint32_t body, edge;

	/* Change size on every frame, to keep the shell relaying out the whole
	 * strip. A window that is being captured must not be disturbed by another
	 * window moving, and nothing else in this harness produces that motion. */
	bool pulse;
	int pulse_phase;

	bool drawing;
	bool closed;
};

/* One buffer per frame, freed when the compositor is done with it. Reusing a
 * single buffer would mean painting over pixels the compositor is still
 * reading, and this window's whole job is to be the colour it claims. */
static void handle_buffer_release(void *data, struct wl_buffer *buffer)
{
	wl_buffer_destroy(buffer);
}

static const struct wl_buffer_listener buffer_listener = {
	.release = handle_buffer_release,
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
	struct paint_client *client = data;

	if (strcmp(interface, wl_compositor_interface.name) == 0) {
		client->compositor = wl_registry_bind(registry, name,
			&wl_compositor_interface, 4);
	} else if (strcmp(interface, wl_shm_interface.name) == 0) {
		client->shm = wl_registry_bind(registry, name, &wl_shm_interface, 1);
	} else if (strcmp(interface, xdg_wm_base_interface.name) == 0) {
		client->wm_base = wl_registry_bind(registry, name,
			&xdg_wm_base_interface, 1);
		xdg_wm_base_add_listener(client->wm_base, &wm_base_listener, client);
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

/* An ARGB buffer: `edge` everywhere, `body` over the window geometry. */
static struct wl_buffer *make_buffer(struct paint_client *client)
{
	int width = client->width + client->margin * 2;
	int height = client->height + client->margin * 2;
	int stride = width * 4;
	size_t size = (size_t)stride * height;

	int fd = memfd_create("paint-client", MFD_CLOEXEC);
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

	for (int y = 0; y < height; y++) {
		for (int x = 0; x < width; x++) {
			bool inside = x >= client->margin &&
				x < client->margin + client->width &&
				y >= client->margin &&
				y < client->margin + client->height;
			pixels[y * width + x] = inside ? client->body : client->edge;
		}
	}
	munmap(pixels, size);

	struct wl_shm_pool *pool = wl_shm_create_pool(client->shm, fd, (int32_t)size);
	struct wl_buffer *buffer = wl_shm_pool_create_buffer(pool, 0, width, height,
		stride, WL_SHM_FORMAT_ARGB8888);
	wl_shm_pool_destroy(pool);
	close(fd);
	if (buffer != NULL) {
		wl_buffer_add_listener(buffer, &buffer_listener, NULL);
	}
	return buffer;
}

static void draw(struct paint_client *client);

/* Redraw on every frame callback.
 *
 * Not for animation — the picture is identical each time — but to keep the
 * scene damaged. An idle window damages nothing, and a capture that never
 * renders cannot be caught rendering badly: the per-frame work this window
 * provokes in the compositor is exactly the condition the capture has to
 * survive. */
static void handle_frame_done(void *data, struct wl_callback *callback,
	uint32_t time)
{
	struct paint_client *client = data;
	wl_callback_destroy(callback);
	draw(client);
}

static const struct wl_callback_listener frame_listener = {
	.done = handle_frame_done,
};

static void draw(struct paint_client *client)
{
	if (client->pulse) {
		client->pulse_phase = !client->pulse_phase;
		client->width += client->pulse_phase ? 40 : -40;
		xdg_surface_set_window_geometry(client->xdg_surface, client->margin,
			client->margin, client->width, client->height);
	}

	struct wl_buffer *buffer = make_buffer(client);
	if (buffer == NULL) {
		client->closed = true;
		return;
	}

	struct wl_callback *callback = wl_surface_frame(client->surface);
	wl_callback_add_listener(callback, &frame_listener, client);

	wl_surface_attach(client->surface, buffer, 0, 0);
	wl_surface_damage_buffer(client->surface, 0, 0, INT32_MAX, INT32_MAX);
	wl_surface_commit(client->surface);
}

static void handle_xdg_surface_configure(void *data,
	struct xdg_surface *xdg_surface, uint32_t serial)
{
	struct paint_client *client = data;
	xdg_surface_ack_configure(xdg_surface, serial);

	/* Only the first configure starts the draw loop. Starting a second one
	 * would leave two chains of frame callbacks running, each committing over
	 * the other. */
	if (!client->drawing) {
		client->drawing = true;
		draw(client);
	}
}

static const struct xdg_surface_listener xdg_surface_listener = {
	.configure = handle_xdg_surface_configure,
};

static void handle_toplevel_configure(void *data,
	struct xdg_toplevel *xdg_toplevel, int32_t width, int32_t height,
	struct wl_array *states)
{
	/* Deliberately ignored; see the file comment. */
}

static void handle_toplevel_close(void *data, struct xdg_toplevel *xdg_toplevel)
{
	struct paint_client *client = data;
	client->closed = true;
}

static const struct xdg_toplevel_listener xdg_toplevel_listener = {
	.configure = handle_toplevel_configure,
	.close = handle_toplevel_close,
};

int main(int argc, char *argv[])
{
	if (argc != 7 && argc != 8) {
		fprintf(stderr,
			"usage: %s APP_ID WIDTH HEIGHT MARGIN BODY_ARGB EDGE_ARGB "
			"[pulse]\n"
			"\n"
			"Paints a WIDTH x HEIGHT window in BODY_ARGB inside a surface\n"
			"grown by MARGIN on every side and painted EDGE_ARGB, the way a\n"
			"client with its own shadows does.\n"
			"\n"
			"With `pulse`, changes its own width every frame, so the shell\n"
			"has to lay the whole workspace out again each time.\n", argv[0]);
		return 2;
	}

	struct paint_client client = {
		.app_id = argv[1],
		.width = atoi(argv[2]),
		.height = atoi(argv[3]),
		.margin = atoi(argv[4]),
		.body = (uint32_t)strtoul(argv[5], NULL, 16),
		.edge = (uint32_t)strtoul(argv[6], NULL, 16),
		.pulse = argc == 8 && strcmp(argv[7], "pulse") == 0,
	};

	if (client.width <= 0 || client.height <= 0 || client.margin < 0) {
		fprintf(stderr, "bad geometry\n");
		return 2;
	}

	struct wl_display *display = wl_display_connect(NULL);
	if (display == NULL) {
		fprintf(stderr, "cannot connect to WAYLAND_DISPLAY\n");
		return 1;
	}

	struct wl_registry *registry = wl_display_get_registry(display);
	wl_registry_add_listener(registry, &registry_listener, &client);
	wl_display_roundtrip(display);

	if (client.compositor == NULL || client.shm == NULL ||
			client.wm_base == NULL) {
		fprintf(stderr, "compositor is missing wl_compositor, wl_shm or "
			"xdg_wm_base\n");
		return 1;
	}

	client.surface = wl_compositor_create_surface(client.compositor);
	client.xdg_surface = xdg_wm_base_get_xdg_surface(client.wm_base,
		client.surface);
	xdg_surface_add_listener(client.xdg_surface, &xdg_surface_listener,
		&client);
	client.xdg_toplevel = xdg_surface_get_toplevel(client.xdg_surface);
	xdg_toplevel_add_listener(client.xdg_toplevel, &xdg_toplevel_listener,
		&client);
	xdg_toplevel_set_app_id(client.xdg_toplevel, client.app_id);
	xdg_toplevel_set_title(client.xdg_toplevel, client.app_id);

	/* The window inside the surface. Without this the compositor has no way
	 * to know the margin is not part of the window. */
	xdg_surface_set_window_geometry(client.xdg_surface, client.margin,
		client.margin, client.width, client.height);

	wl_surface_commit(client.surface);

	while (!client.closed && wl_display_dispatch(display) != -1) {
		/* Keep the window up until killed; the harness drives the timing. */
	}

	wl_display_disconnect(display);
	return 0;
}
