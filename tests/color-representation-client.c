/* SPDX-License-Identifier: MIT
 *
 * Drive wp-color-representation-v1 the way a video player does.
 *
 * A DMA-BUF carries no colour representation, and the compositor used to
 * have to guess the matrix and range from the buffer's height. This protocol
 * is the client saying what its Y'CbCr code words mean instead, so the guess
 * stops being load-bearing. The five modes this client runs assert the five
 * things worth asserting about the conversation, and each one exits 0 when
 * the compositor behaved — including the ones whose correct behaviour is to
 * kill the connection.
 *
 *   advertise        the advertisement: one alpha mode, the six advertised
 *                    coefficient-and-range pairs, then done, all at bind;
 *   declare          set coefficients-and-range and chroma siting and
 *                    commit; the connection lives;
 *   bad-combination  set a matrix that was never advertised (ICtCp); the
 *                    compositor must kill us;
 *   bad-siting       set an H.273 chroma location with no Vulkan equivalent
 *                    (type 4); likewise;
 *   rgb-mismatch     declare a Y'CbCr matrix and commit an shm (RGB)
 *                    buffer; the commit-time format check must kill us.
 *
 * Exits 0 when the compositor did what the mode asks, 2 if the global is
 * missing, 1 if it did not.
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
#include <unistd.h>

#include <wayland-client.h>

#include "color-representation-v1-client-protocol.h"

struct state {
	struct wl_display *display;
	struct wl_registry *registry;
	struct wl_compositor *compositor;
	struct wl_shm *shm;
	struct wp_color_representation_manager_v1 *manager;
	struct wp_color_representation_surface_v1 *representation;
	struct wl_surface *surface;
	struct wl_shm_pool *pool;
	struct wl_buffer *buffer;
	bool got_manager;
	bool done;
	unsigned pair_count;
	/* Bitmasks over the wire enum values: what was advertised, so the
	 * test can check the exact set rather than a count that could be
	 * padded by duplicates. */
	uint64_t seen_pairs;
	uint32_t seen_alpha_modes;
};

/* Eight coefficient values (1..=8) and two range values (1..=2): one bit
 * per pair fits in a u64 with the shift below. */
static uint64_t pair_bit(uint32_t coefficients, uint32_t range)
{
	return 1ull << ((uint64_t)coefficients * 2u + (range - 1u));
}

static void handle_supported_alpha_mode(void *data,
	struct wp_color_representation_manager_v1 *manager, uint32_t alpha_mode)
{
	struct state *state = data;
	(void)manager;

	if (alpha_mode >= 32) {
		fprintf(stderr, "absurd alpha mode %u\n", alpha_mode);
		return;
	}
	if (state->done) {
		fputs("alpha mode advertised after done\n", stderr);
		return;
	}
	state->seen_alpha_modes |= 1u << alpha_mode;
}

static void handle_supported_coefficients_and_ranges(void *data,
	struct wp_color_representation_manager_v1 *manager,
	uint32_t coefficients, uint32_t range)
{
	struct state *state = data;
	(void)manager;

	if (coefficients == 0 || coefficients > 8 || range == 0 || range > 2) {
		fprintf(stderr, "out-of-range advertisement %u/%u\n",
			coefficients, range);
		return;
	}
	if (state->done) {
		fputs("combination advertised after done\n", stderr);
		return;
	}
	state->pair_count++;
	state->seen_pairs |= pair_bit(coefficients, range);
}

static void handle_manager_done(void *data,
	struct wp_color_representation_manager_v1 *manager)
{
	struct state *state = data;
	(void)manager;
	state->done = true;
}

static const struct wp_color_representation_manager_v1_listener manager_listener = {
	.supported_alpha_mode = handle_supported_alpha_mode,
	.supported_coefficients_and_ranges = handle_supported_coefficients_and_ranges,
	.done = handle_manager_done,
};

static void registry_global(void *data, struct wl_registry *registry,
	uint32_t name, const char *interface, uint32_t version)
{
	struct state *state = data;
	(void)version;

	if (strcmp(interface, wl_compositor_interface.name) == 0) {
		state->compositor = wl_registry_bind(registry, name,
			&wl_compositor_interface, 4);
	} else if (strcmp(interface, wl_shm_interface.name) == 0) {
		state->shm = wl_registry_bind(registry, name, &wl_shm_interface, 1);
	} else if (strcmp(interface,
			wp_color_representation_manager_v1_interface.name) == 0) {
		state->manager = wl_registry_bind(registry, name,
			&wp_color_representation_manager_v1_interface, 1);
		wp_color_representation_manager_v1_add_listener(state->manager,
			&manager_listener, state);
		state->got_manager = true;
	}
}

static void registry_global_remove(void *data, struct wl_registry *registry,
	uint32_t name)
{
	(void)data;
	(void)registry;
	(void)name;
}

static const struct wl_registry_listener registry_listener = {
	.global = registry_global,
	.global_remove = registry_global_remove,
};

/* An RGB shm buffer, which is the wrong family for any Y'CbCr declaration
 * and is exactly what the commit-time format check has to catch. */
static int attach_rgb_buffer(struct state *state)
{
	const int32_t width = 16, height = 16, stride = width * 4;
	char path[] = "/tmp/color-representation-shm-XXXXXX";
	int fd = mkstemp(path);
	if (fd < 0) {
		perror("mkstemp");
		return -1;
	}
	if (ftruncate(fd, (size_t)stride * height) != 0) {
		perror("ftruncate");
		close(fd);
		return -1;
	}
	unlink(path);
	void *memory = mmap(NULL, (size_t)stride * height,
		PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	if (memory == MAP_FAILED) {
		perror("mmap");
		close(fd);
		return -1;
	}
	state->pool = wl_shm_create_pool(state->shm, fd, (int32_t)stride * height);
	state->buffer = wl_shm_pool_create_buffer(state->pool, 0,
		width, height, stride, WL_SHM_FORMAT_XRGB8888);
	wl_surface_attach(state->surface, state->buffer, 0, 0);
	close(fd);
	munmap(memory, (size_t)stride * height);
	return 0;
}

/* The connection must die: a refused request is a fatal protocol error, and
 * a compositor that accepted it and stayed up is the one failing here. */
static int expect_killed(struct state *state, const char *what)
{
	wl_display_flush(state->display);
	while (wl_display_get_error(state->display) == 0) {
		struct pollfd pfd = {
			.fd = wl_display_get_fd(state->display),
			.events = POLLIN,
		};
		int ready = poll(&pfd, 1, 2000);
		if (ready <= 0) {
			fprintf(stderr, "%s: compositor neither answered nor killed us (errno %d)\n",
				what, errno);
			return 1;
		}
		if (wl_display_dispatch(state->display) < 0)
			break;
	}
	if (wl_display_get_error(state->display) == 0) {
		fprintf(stderr, "%s: compositor kept the connection open\n", what);
		return 1;
	}
	return 0;
}

static int need_surface_and_representation(struct state *state)
{
	if (!state->compositor) {
		fputs("no wl_compositor\n", stderr);
		return -1;
	}
	state->surface = wl_compositor_create_surface(state->compositor);
	state->representation =
		wp_color_representation_manager_v1_get_surface(state->manager,
			state->surface);
	if (!state->representation) {
		fputs("get_surface did not return an object\n", stderr);
		return -1;
	}
	return 0;
}

static int run_advertise(struct state *state)
{
	static const uint32_t matrices[3] = {
		WP_COLOR_REPRESENTATION_SURFACE_V1_COEFFICIENTS_BT709,
		WP_COLOR_REPRESENTATION_SURFACE_V1_COEFFICIENTS_BT601,
		WP_COLOR_REPRESENTATION_SURFACE_V1_COEFFICIENTS_BT2020,
	};
	uint64_t expected = 0;

	for (unsigned m = 0; m < 3; m++) {
		expected |= pair_bit(matrices[m],
			WP_COLOR_REPRESENTATION_SURFACE_V1_RANGE_FULL);
		expected |= pair_bit(matrices[m],
			WP_COLOR_REPRESENTATION_SURFACE_V1_RANGE_LIMITED);
	}

	if (!state->done) {
		fputs("the advertisement never finished with done\n", stderr);
		return 1;
	}
	if (state->pair_count != 6 || state->seen_pairs != expected) {
		fprintf(stderr, "advertisements %u/%016llx, wanted 6/%016llx\n",
			state->pair_count, (unsigned long long)state->seen_pairs,
			(unsigned long long)expected);
		return 1;
	}
	if (state->seen_alpha_modes != (1u <<
			WP_COLOR_REPRESENTATION_SURFACE_V1_ALPHA_MODE_PREMULTIPLIED_ELECTRICAL)) {
		fprintf(stderr, "alpha modes 0x%x, wanted premultiplied_electrical only\n",
			state->seen_alpha_modes);
		return 1;
	}
	return 0;
}

static int run_declare(struct state *state)
{
	if (need_surface_and_representation(state) < 0)
		return 1;

	/* A legal declaration, both fields, committed without a buffer: the
	 * commit applies state and the format check has nothing to check. */
	wp_color_representation_surface_v1_set_coefficients_and_range(
		state->representation,
		WP_COLOR_REPRESENTATION_SURFACE_V1_COEFFICIENTS_BT601,
		WP_COLOR_REPRESENTATION_SURFACE_V1_RANGE_FULL);
	wp_color_representation_surface_v1_set_chroma_location(
		state->representation,
		WP_COLOR_REPRESENTATION_SURFACE_V1_CHROMA_LOCATION_TYPE_0);
	wl_surface_commit(state->surface);
	if (wl_display_roundtrip(state->display) < 0) {
		fputs("a legal declaration killed the connection\n", stderr);
		return 1;
	}

	/* And a second commit after the destroy, still without a buffer:
	 * unset is double-buffered too, and unsetting must not be fatal. */
	wp_color_representation_surface_v1_destroy(state->representation);
	state->representation = NULL;
	if (wl_display_roundtrip(state->display) < 0) {
		fputs("destroying the object killed the connection\n", stderr);
		return 1;
	}
	wl_surface_commit(state->surface);
	if (wl_display_roundtrip(state->display) < 0) {
		fputs("the commit after the unset killed the connection\n", stderr);
		return 1;
	}
	return 0;
}

static int run_bad_combination(struct state *state)
{
	if (need_surface_and_representation(state) < 0)
		return 1;
	wp_color_representation_surface_v1_set_coefficients_and_range(
		state->representation,
		WP_COLOR_REPRESENTATION_SURFACE_V1_COEFFICIENTS_ICTCP,
		WP_COLOR_REPRESENTATION_SURFACE_V1_RANGE_LIMITED);
	return expect_killed(state, "ictcp coefficients");
}

static int run_bad_siting(struct state *state)
{
	if (need_surface_and_representation(state) < 0)
		return 1;
	wp_color_representation_surface_v1_set_chroma_location(
		state->representation,
		WP_COLOR_REPRESENTATION_SURFACE_V1_CHROMA_LOCATION_TYPE_4);
	return expect_killed(state, "chroma location type 4");
}

static int run_rgb_mismatch(struct state *state)
{
	if (need_surface_and_representation(state) < 0)
		return 1;
	if (!state->shm) {
		fputs("no wl_shm\n", stderr);
		return 1;
	}
	wp_color_representation_surface_v1_set_coefficients_and_range(
		state->representation,
		WP_COLOR_REPRESENTATION_SURFACE_V1_COEFFICIENTS_BT709,
		WP_COLOR_REPRESENTATION_SURFACE_V1_RANGE_LIMITED);
	if (attach_rgb_buffer(state) < 0)
		return 1;
	wl_surface_commit(state->surface);
	return expect_killed(state, "y'cbcr declaration on an rgb buffer");
}

int main(int argc, char *argv[])
{
	struct state state = { 0 };
	const char *mode = argc > 1 ? argv[1] : "advertise";

	state.display = wl_display_connect(NULL);
	if (!state.display) {
		fputs("cannot connect\n", stderr);
		return 2;
	}
	state.registry = wl_display_get_registry(state.display);
	wl_registry_add_listener(state.registry, &registry_listener, &state);
	wl_display_roundtrip(state.display);
	/* The advertisement rides out on the manager object from inside bind,
	 * which the roundtrip above processed; one more pass makes sure
	 * nothing that arrived afterwards is left unread. */
	wl_display_roundtrip(state.display);
	if (!state.got_manager) {
		fputs("no wp_color_representation_manager_v1 global\n", stderr);
		return 2;
	}

	if (strcmp(mode, "advertise") == 0)
		return run_advertise(&state);
	if (strcmp(mode, "declare") == 0)
		return run_declare(&state);
	if (strcmp(mode, "bad-combination") == 0)
		return run_bad_combination(&state);
	if (strcmp(mode, "bad-siting") == 0)
		return run_bad_siting(&state);
	if (strcmp(mode, "rgb-mismatch") == 0)
		return run_rgb_mismatch(&state);
	fprintf(stderr, "unknown mode %s\n", mode);
	return 2;
}
