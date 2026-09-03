/* SPDX-License-Identifier: MIT
 *
 * A sharp two-colour backdrop and a transparent layer requesting blur over a
 * bounded region. Output capture can then distinguish real blur from a global
 * that was merely advertised.
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

#include "ext-background-effect-v1-client-protocol.h"
#include "wlr-layer-shell-unstable-v1-client-protocol.h"

struct client;

struct layer {
	struct client *client;
	struct wl_surface *surface;
	struct zwlr_layer_surface_v1 *role;
	struct ext_background_effect_surface_v1 *effect;
	bool transparent;
	bool painted;
};

struct client {
	struct wl_display *display;
	struct wl_compositor *compositor;
	struct wl_shm *shm;
	struct zwlr_layer_shell_v1 *layer_shell;
	struct ext_background_effect_manager_v1 *effect_manager;
	bool blur_capability;
	bool closed;
	struct layer background;
	struct layer overlay;
};

static void handle_buffer_release(void *data, struct wl_buffer *buffer)
{
	wl_buffer_destroy(buffer);
}

static const struct wl_buffer_listener buffer_listener = {
	.release = handle_buffer_release,
};

static struct wl_buffer *make_buffer(struct layer *layer, int32_t width,
	int32_t height)
{
	int32_t stride = width * 4;
	size_t size = (size_t)stride * height;
	int fd = memfd_create("background-effect-client", MFD_CLOEXEC);
	if (fd < 0 || ftruncate(fd, (off_t)size) < 0) {
		fprintf(stderr, "allocating buffer: %s\n", strerror(errno));
		if (fd >= 0) {
			close(fd);
		}
		return NULL;
	}

	uint32_t *pixels = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED,
		fd, 0);
	if (pixels == MAP_FAILED) {
		fprintf(stderr, "mapping buffer: %s\n", strerror(errno));
		close(fd);
		return NULL;
	}

	for (int32_t y = 0; y < height; y++) {
		for (int32_t x = 0; x < width; x++) {
			pixels[y * width + x] = layer->transparent
				? 0x00000000
				: (x < 192 ? 0xffff0000 : 0xff0000ff);
		}
	}
	munmap(pixels, size);

	struct wl_shm_pool *pool = wl_shm_create_pool(layer->client->shm, fd,
		(int32_t)size);
	struct wl_buffer *buffer = wl_shm_pool_create_buffer(pool, 0, width,
		height, stride, WL_SHM_FORMAT_ARGB8888);
	wl_shm_pool_destroy(pool);
	close(fd);
	if (buffer != NULL) {
		wl_buffer_add_listener(buffer, &buffer_listener, NULL);
	}
	return buffer;
}

static void handle_layer_configure(void *data,
	struct zwlr_layer_surface_v1 *role, uint32_t serial, uint32_t width,
	uint32_t height)
{
	struct layer *layer = data;
	zwlr_layer_surface_v1_ack_configure(role, serial);
	if (width == 0 || height == 0) {
		fprintf(stderr, "layer configured without a size\n");
		layer->client->closed = true;
		return;
	}

	struct wl_buffer *buffer = make_buffer(layer, (int32_t)width,
		(int32_t)height);
	if (buffer == NULL) {
		layer->client->closed = true;
		return;
	}
	wl_surface_attach(layer->surface, buffer, 0, 0);
	wl_surface_damage_buffer(layer->surface, 0, 0, (int32_t)width,
		(int32_t)height);
	wl_surface_commit(layer->surface);
	layer->painted = true;
}

static void handle_layer_closed(void *data,
	struct zwlr_layer_surface_v1 *role)
{
	struct layer *layer = data;
	layer->client->closed = true;
}

static const struct zwlr_layer_surface_v1_listener layer_listener = {
	.configure = handle_layer_configure,
	.closed = handle_layer_closed,
};

static void handle_capabilities(void *data,
	struct ext_background_effect_manager_v1 *manager, uint32_t capabilities)
{
	struct client *client = data;
	client->blur_capability =
		(capabilities & EXT_BACKGROUND_EFFECT_MANAGER_V1_CAPABILITY_BLUR) != 0;
}

static const struct ext_background_effect_manager_v1_listener effect_listener = {
	.capabilities = handle_capabilities,
};

static void handle_global(void *data, struct wl_registry *registry,
	uint32_t name, const char *interface, uint32_t version)
{
	struct client *client = data;
	if (strcmp(interface, wl_compositor_interface.name) == 0) {
		client->compositor = wl_registry_bind(registry, name,
			&wl_compositor_interface, version < 4 ? version : 4);
	} else if (strcmp(interface, wl_shm_interface.name) == 0) {
		client->shm = wl_registry_bind(registry, name, &wl_shm_interface, 1);
	} else if (strcmp(interface, zwlr_layer_shell_v1_interface.name) == 0) {
		client->layer_shell = wl_registry_bind(registry, name,
			&zwlr_layer_shell_v1_interface, 1);
	} else if (strcmp(interface,
			ext_background_effect_manager_v1_interface.name) == 0) {
		client->effect_manager = wl_registry_bind(registry, name,
			&ext_background_effect_manager_v1_interface, 1);
		ext_background_effect_manager_v1_add_listener(
			client->effect_manager, &effect_listener, client);
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

static void create_layer(struct client *client, struct layer *layer,
	uint32_t level, const char *namespace)
{
	layer->client = client;
	layer->surface = wl_compositor_create_surface(client->compositor);
	layer->role = zwlr_layer_shell_v1_get_layer_surface(client->layer_shell,
		layer->surface, NULL, level, namespace);
	zwlr_layer_surface_v1_add_listener(layer->role, &layer_listener, layer);
	zwlr_layer_surface_v1_set_size(layer->role, 0, 0);
	zwlr_layer_surface_v1_set_anchor(layer->role,
		ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP |
		ZWLR_LAYER_SURFACE_V1_ANCHOR_RIGHT |
		ZWLR_LAYER_SURFACE_V1_ANCHOR_BOTTOM |
		ZWLR_LAYER_SURFACE_V1_ANCHOR_LEFT);
	zwlr_layer_surface_v1_set_exclusive_zone(layer->role, -1);
	wl_surface_commit(layer->surface);
}

int main(void)
{
	struct client client = {0};
	client.display = wl_display_connect(NULL);
	if (client.display == NULL) {
		fprintf(stderr, "cannot connect to WAYLAND_DISPLAY\n");
		return 1;
	}

	struct wl_registry *registry = wl_display_get_registry(client.display);
	wl_registry_add_listener(registry, &registry_listener, &client);
	wl_display_roundtrip(client.display);
	wl_display_roundtrip(client.display);
	if (client.compositor == NULL || client.shm == NULL ||
			client.layer_shell == NULL || client.effect_manager == NULL) {
		fprintf(stderr, "compositor is missing a required background-effect "
			"test global\n");
		return 1;
	}
	if (!client.blur_capability) {
		fprintf(stderr, "compositor did not advertise blur capability\n");
		return 1;
	}

	create_layer(&client, &client.background,
		ZWLR_LAYER_SHELL_V1_LAYER_BACKGROUND, "blur-test-background");
	client.overlay.transparent = true;
	create_layer(&client, &client.overlay, ZWLR_LAYER_SHELL_V1_LAYER_OVERLAY,
		"blur-test-overlay");

	client.overlay.effect =
		ext_background_effect_manager_v1_get_background_effect(
			client.effect_manager, client.overlay.surface);
	struct wl_region *region = wl_compositor_create_region(client.compositor);
	wl_region_add(region, 128, 64, 256, 256);
	wl_region_subtract(region, 304, 160, 64, 64);
	ext_background_effect_surface_v1_set_blur_region(client.overlay.effect,
		region);
	wl_region_destroy(region);

	while (!client.closed &&
			(!client.background.painted || !client.overlay.painted)) {
		if (wl_display_dispatch(client.display) < 0) {
			return 1;
		}
	}
	if (client.closed || wl_display_roundtrip(client.display) < 0) {
		return 1;
	}

	puts("ready");
	fflush(stdout);
	while (!client.closed && wl_display_dispatch(client.display) >= 0) {
	}
	wl_display_disconnect(client.display);
	return client.closed ? 1 : 0;
}
