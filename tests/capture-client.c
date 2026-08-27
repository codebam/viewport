/* SPDX-License-Identifier: MIT
 *
 * Capture one window and say whether what came back is that window.
 *
 * This is the outside view that was missing while window capture was broken:
 * every symptom had to be reported by a person sharing a window in OBS, and
 * every wrong guess cost them their session. The two failures that shipped are
 * both visible from here without a display.
 *
 *   - Capturing the wrong pixels. wlroots renders the whole scene through the
 *     rectangle of whatever node the capture is aimed at, so a capture aimed
 *     into the layout picks up the windows stacked over it and the shell
 *     behind it. The window under test paints one flat colour inside its
 *     window geometry and a different one in the decoration margin around it,
 *     so anything but the first colour, anywhere, is a fail with coordinates.
 *
 *   - Freezing the client. The capture output takes its mode from the node's
 *     extents on every render, and a node inside the layout is re-clipped and
 *     re-scaled every frame, so the size moved constantly and every move
 *     re-sent buffer constraints. A client spending its time renegotiating
 *     never gets a picture. Constraints arriving more than a couple of times
 *     for a window that is not resizing is that bug, and it is counted here
 *     rather than inferred from a video that looks stuck.
 *
 *     Be warned about that second one: it does not currently fail against the
 *     code that shipped the bug. It was tried against it in both layouts, with
 *     the window under test clipped at the edge of a strip being laid out again
 *     on every frame, and the count stayed at zero — the sizes in the session
 *     that led to the fix moved because the shared window itself was being
 *     resized, and nothing here reproduces that. So it guards the mechanism
 *     rather than demonstrating it. The pixel checks below are the ones that
 *     catch the shipped bug, and they do it decisively.
 *
 * Exits 0 if every check passed, 1 if any failed, 2 if it could not run the
 * checks at all. Output is one `ok` or `FAIL` line per check.
 */
#define _GNU_SOURCE

#include <errno.h>
#include <poll.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#include <wayland-client.h>

#include "ext-foreign-toplevel-list-v1-client-protocol.h"
#include "ext-image-capture-source-v1-client-protocol.h"
#include "ext-image-copy-capture-v1-client-protocol.h"

#define MAX_TOPLEVELS 32

struct toplevel_entry {
	struct ext_foreign_toplevel_handle_v1 *handle;
	char *app_id;
};

struct capture_client {
	struct wl_shm *shm;
	struct ext_foreign_toplevel_list_v1 *toplevel_list;
	struct ext_foreign_toplevel_image_capture_source_manager_v1 *source_manager;
	struct ext_image_copy_capture_manager_v1 *capture_manager;
	/* For --output: capturing a whole screen rather than one window, which is
	 * how "what is actually on screen" gets asked from outside. */
	struct ext_output_image_capture_source_manager_v1 *output_source_manager;
	struct wl_output *output;

	struct toplevel_entry toplevels[MAX_TOPLEVELS];
	size_t toplevels_len;

	/* Buffer constraints, and how many times they have been announced. The
	 * count is the freeze detector. */
	int32_t buffer_width, buffer_height;
	uint32_t shm_format;
	bool have_shm_format;
	int constraints_done;
	bool session_stopped;

	/* Per-frame. */
	bool frame_ready;
	bool frame_failed;
	uint32_t frame_failure_reason;
	int frames_ready;
};

static int failures;

static void check(bool passed, const char *fmt, ...)
{
	va_list args;
	va_start(args, fmt);
	char message[256];
	vsnprintf(message, sizeof(message), fmt, args);
	va_end(args);

	if (passed) {
		printf("ok   %s\n", message);
	} else {
		printf("FAIL %s\n", message);
		failures++;
	}
	fflush(stdout);
}

static int64_t now_ms(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

/* Dispatch until `*flag` goes true or the deadline passes.
 *
 * Every wait in this program is bounded. A capture that never answers is one
 * of the failures being tested for, and a test that hangs on it reports
 * nothing at all. */
static bool wait_until(struct wl_display *display, const bool *flag,
	int timeout_ms)
{
	int64_t deadline = now_ms() + timeout_ms;

	while (!*flag) {
		if (wl_display_dispatch_pending(display) < 0) {
			return false;
		}
		if (*flag) {
			break;
		}
		if (wl_display_flush(display) < 0 && errno != EAGAIN) {
			return false;
		}

		int remaining = (int)(deadline - now_ms());
		if (remaining <= 0) {
			return false;
		}

		struct pollfd pfd = {
			.fd = wl_display_get_fd(display),
			.events = POLLIN,
		};
		int ready = poll(&pfd, 1, remaining);
		if (ready < 0) {
			if (errno == EINTR) {
				continue;
			}
			return false;
		}
		if (ready == 0) {
			return false;
		}
		if (wl_display_dispatch(display) < 0) {
			return false;
		}
	}
	return true;
}

/* Dispatch for a fixed stretch, whatever happens. Used to watch an idle
 * capture for the churn that used to freeze it. */
static void pump_for(struct wl_display *display, int duration_ms)
{
	int64_t deadline = now_ms() + duration_ms;

	for (;;) {
		int remaining = (int)(deadline - now_ms());
		if (remaining <= 0) {
			return;
		}
		if (wl_display_flush(display) < 0 && errno != EAGAIN) {
			return;
		}

		struct pollfd pfd = {
			.fd = wl_display_get_fd(display),
			.events = POLLIN,
		};
		int ready = poll(&pfd, 1, remaining);
		if (ready <= 0) {
			if (ready < 0 && errno == EINTR) {
				continue;
			}
			return;
		}
		if (wl_display_dispatch(display) < 0) {
			return;
		}
	}
}

/* ------------------------------------------------------------------------ */
/* Finding the window                                                        */
/* ------------------------------------------------------------------------ */

static void handle_handle_closed(void *data,
	struct ext_foreign_toplevel_handle_v1 *handle)
{
}

static void handle_handle_done(void *data,
	struct ext_foreign_toplevel_handle_v1 *handle)
{
}

static void handle_handle_title(void *data,
	struct ext_foreign_toplevel_handle_v1 *handle, const char *title)
{
}

static void handle_handle_app_id(void *data,
	struct ext_foreign_toplevel_handle_v1 *handle, const char *app_id)
{
	struct toplevel_entry *entry = data;
	free(entry->app_id);
	entry->app_id = strdup(app_id);
}

static void handle_handle_identifier(void *data,
	struct ext_foreign_toplevel_handle_v1 *handle, const char *identifier)
{
}

static const struct ext_foreign_toplevel_handle_v1_listener handle_listener = {
	.closed = handle_handle_closed,
	.done = handle_handle_done,
	.title = handle_handle_title,
	.app_id = handle_handle_app_id,
	.identifier = handle_handle_identifier,
};

static void handle_list_toplevel(void *data,
	struct ext_foreign_toplevel_list_v1 *list,
	struct ext_foreign_toplevel_handle_v1 *handle)
{
	struct capture_client *client = data;
	if (client->toplevels_len >= MAX_TOPLEVELS) {
		ext_foreign_toplevel_handle_v1_destroy(handle);
		return;
	}

	struct toplevel_entry *entry = &client->toplevels[client->toplevels_len++];
	entry->handle = handle;
	ext_foreign_toplevel_handle_v1_add_listener(handle, &handle_listener,
		entry);
}

static void handle_list_finished(void *data,
	struct ext_foreign_toplevel_list_v1 *list)
{
}

static const struct ext_foreign_toplevel_list_v1_listener list_listener = {
	.toplevel = handle_list_toplevel,
	.finished = handle_list_finished,
};

/* ------------------------------------------------------------------------ */
/* The capture session                                                       */
/* ------------------------------------------------------------------------ */

static void handle_session_buffer_size(void *data,
	struct ext_image_copy_capture_session_v1 *session, uint32_t width,
	uint32_t height)
{
	struct capture_client *client = data;
	client->buffer_width = (int32_t)width;
	client->buffer_height = (int32_t)height;
}

static void handle_session_shm_format(void *data,
	struct ext_image_copy_capture_session_v1 *session, uint32_t format)
{
	struct capture_client *client = data;
	/* Either will do; both are four bytes with the colour in the low three. */
	if (format == WL_SHM_FORMAT_XRGB8888 || format == WL_SHM_FORMAT_ARGB8888) {
		client->shm_format = format;
		client->have_shm_format = true;
	}
}

static void handle_session_dmabuf_device(void *data,
	struct ext_image_copy_capture_session_v1 *session, struct wl_array *device)
{
}

static void handle_session_dmabuf_format(void *data,
	struct ext_image_copy_capture_session_v1 *session, uint32_t format,
	struct wl_array *modifiers)
{
}

static void handle_session_done(void *data,
	struct ext_image_copy_capture_session_v1 *session)
{
	struct capture_client *client = data;
	client->constraints_done++;
}

static void handle_session_stopped(void *data,
	struct ext_image_copy_capture_session_v1 *session)
{
	struct capture_client *client = data;
	client->session_stopped = true;
}

static const struct ext_image_copy_capture_session_v1_listener
		session_listener = {
	.buffer_size = handle_session_buffer_size,
	.shm_format = handle_session_shm_format,
	.dmabuf_device = handle_session_dmabuf_device,
	.dmabuf_format = handle_session_dmabuf_format,
	.done = handle_session_done,
	.stopped = handle_session_stopped,
};

static void handle_frame_transform(void *data,
	struct ext_image_copy_capture_frame_v1 *frame, uint32_t transform)
{
}

static void handle_frame_damage(void *data,
	struct ext_image_copy_capture_frame_v1 *frame, int32_t x, int32_t y,
	int32_t width, int32_t height)
{
}

static void handle_frame_presentation_time(void *data,
	struct ext_image_copy_capture_frame_v1 *frame, uint32_t tv_sec_hi,
	uint32_t tv_sec_lo, uint32_t tv_nsec)
{
}

static void handle_frame_ready(void *data,
	struct ext_image_copy_capture_frame_v1 *frame)
{
	struct capture_client *client = data;
	client->frame_ready = true;
	client->frames_ready++;
}

static void handle_frame_failed(void *data,
	struct ext_image_copy_capture_frame_v1 *frame, uint32_t reason)
{
	struct capture_client *client = data;
	client->frame_failed = true;
	client->frame_failure_reason = reason;
}

static const struct ext_image_copy_capture_frame_v1_listener frame_listener = {
	.transform = handle_frame_transform,
	.damage = handle_frame_damage,
	.presentation_time = handle_frame_presentation_time,
	.ready = handle_frame_ready,
	.failed = handle_frame_failed,
};

/* ------------------------------------------------------------------------ */

static void handle_global(void *data, struct wl_registry *registry,
	uint32_t name, const char *interface, uint32_t version)
{
	struct capture_client *client = data;

	if (strcmp(interface, wl_shm_interface.name) == 0) {
		client->shm = wl_registry_bind(registry, name, &wl_shm_interface, 1);
	} else if (strcmp(interface,
			ext_foreign_toplevel_list_v1_interface.name) == 0) {
		client->toplevel_list = wl_registry_bind(registry, name,
			&ext_foreign_toplevel_list_v1_interface, 1);
		ext_foreign_toplevel_list_v1_add_listener(client->toplevel_list,
			&list_listener, client);
	} else if (strcmp(interface,
			ext_foreign_toplevel_image_capture_source_manager_v1_interface.name)
			== 0) {
		client->source_manager = wl_registry_bind(registry, name,
			&ext_foreign_toplevel_image_capture_source_manager_v1_interface, 1);
	} else if (strcmp(interface,
			ext_image_copy_capture_manager_v1_interface.name) == 0) {
		client->capture_manager = wl_registry_bind(registry, name,
			&ext_image_copy_capture_manager_v1_interface, 1);
	} else if (strcmp(interface,
			ext_output_image_capture_source_manager_v1_interface.name) == 0) {
		client->output_source_manager = wl_registry_bind(registry, name,
			&ext_output_image_capture_source_manager_v1_interface, 1);
	} else if (strcmp(interface, wl_output_interface.name) == 0) {
		/* The first output is enough: the tests run headless with one. */
		if (client->output == NULL) {
			client->output = wl_registry_bind(registry, name,
				&wl_output_interface, 1);
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

struct shm_buffer {
	struct wl_buffer *buffer;
	uint32_t *pixels;
	size_t size;
	int32_t width, height;
};

static bool shm_buffer_create(struct shm_buffer *out, struct wl_shm *shm,
	int32_t width, int32_t height, uint32_t format)
{
	int stride = width * 4;
	size_t size = (size_t)stride * height;

	int fd = memfd_create("capture-client", MFD_CLOEXEC);
	if (fd < 0) {
		return false;
	}
	if (ftruncate(fd, (off_t)size) < 0) {
		close(fd);
		return false;
	}

	uint32_t *pixels = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED,
		fd, 0);
	if (pixels == MAP_FAILED) {
		close(fd);
		return false;
	}

	/* A colour nothing under test paints, so a capture that writes nothing at
	 * all fails loudly instead of matching by accident. */
	for (size_t i = 0; i < size / 4; i++) {
		pixels[i] = 0xFF00FF00;
	}

	struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, (int32_t)size);
	struct wl_buffer *buffer = wl_shm_pool_create_buffer(pool, 0, width, height,
		stride, format);
	wl_shm_pool_destroy(pool);
	close(fd);

	if (buffer == NULL) {
		munmap(pixels, size);
		return false;
	}

	*out = (struct shm_buffer){
		.buffer = buffer,
		.pixels = pixels,
		.size = size,
		.width = width,
		.height = height,
	};
	return true;
}

/* Grab one frame into `buffer`. The frame object is short-lived by protocol:
 * a session refuses a second create_frame before the first is destroyed. */
static bool capture_one_frame(struct capture_client *client,
	struct wl_display *display,
	struct ext_image_copy_capture_session_v1 *session,
	struct shm_buffer *buffer, int timeout_ms)
{
	client->frame_ready = false;
	client->frame_failed = false;

	struct ext_image_copy_capture_frame_v1 *frame =
		ext_image_copy_capture_session_v1_create_frame(session);
	ext_image_copy_capture_frame_v1_add_listener(frame, &frame_listener,
		client);
	ext_image_copy_capture_frame_v1_attach_buffer(frame, buffer->buffer);
	ext_image_copy_capture_frame_v1_damage_buffer(frame, 0, 0, buffer->width,
		buffer->height);
	ext_image_copy_capture_frame_v1_capture(frame);

	bool settled = wait_until(display, &client->frame_ready, timeout_ms);
	if (!settled && client->frame_failed) {
		settled = false;
	}
	ext_image_copy_capture_frame_v1_destroy(frame);
	return settled;
}

int main(int argc, char *argv[])
{
	bool output_mode = argc >= 2 && strcmp(argv[1], "--output") == 0;
	bool output_has = argc >= 2 && strcmp(argv[1], "--output-has") == 0;
	bool output_not = argc >= 2 && strcmp(argv[1], "--output-not") == 0;
	output_mode = output_mode || output_has || output_not;

	if (!output_mode && argc != 6) {
		fprintf(stderr,
			"usage: %s APP_ID EXPECT_ARGB EXPECT_WIDTH EXPECT_HEIGHT "
			"WATCH_MS\n"
			"       %s --output EXPECT_ARGB\n"
			"       %s --output-has EXPECT_ARGB\n"
			"       %s --output-not EXPECT_ARGB\n"
			"\n"
			"Captures the window with APP_ID and checks that the frame is\n"
			"EXPECT_WIDTH x EXPECT_HEIGHT and every pixel EXPECT_ARGB, then\n"
			"watches the session for WATCH_MS for buffer-constraint churn.\n"
			"\n"
			"--output captures a whole screen instead and checks every pixel\n"
			"is EXPECT_ARGB. That is how the screen-lock tests ask what is\n"
			"actually being displayed, which is the only place the answer\n"
			"lives: a compositor whose lock layer has been emptied still\n"
			"reports itself locked.\n",
			argv[0], argv[0], argv[0], argv[0]);
		return 2;
	}
	if (output_mode && argc != 3) {
		fprintf(stderr, "usage: %s --output EXPECT_ARGB\n", argv[0]);
		return 2;
	}

	const char *want_app_id = output_mode ? NULL : argv[1];
	uint32_t want_colour = (uint32_t)strtoul(
		output_mode ? argv[2] : argv[2], NULL, 16) & 0x00FFFFFF;
	int32_t want_width = output_mode ? 0 : atoi(argv[3]);
	int32_t want_height = output_mode ? 0 : atoi(argv[4]);
	int watch_ms = output_mode ? 0 : atoi(argv[5]);

	struct capture_client client = {0};

	struct wl_display *display = wl_display_connect(NULL);
	if (display == NULL) {
		fprintf(stderr, "cannot connect to WAYLAND_DISPLAY\n");
		return 2;
	}

	struct wl_registry *registry = wl_display_get_registry(display);
	wl_registry_add_listener(registry, &registry_listener, &client);
	/* Twice: the first settles the globals, the second the toplevel handles
	 * and their app ids, which only arrive once the list is bound. */
	wl_display_roundtrip(display);
	wl_display_roundtrip(display);

	if (output_mode) {
		if (client.shm == NULL || client.output == NULL ||
				client.output_source_manager == NULL ||
				client.capture_manager == NULL) {
			fprintf(stderr, "compositor is missing wl_shm (%p), wl_output (%p), "
				"ext_output_image_capture_source_manager_v1 (%p) or "
				"ext_image_copy_capture_manager_v1 (%p)\n",
				(void *)client.shm, (void *)client.output,
				(void *)client.output_source_manager,
				(void *)client.capture_manager);
			return 2;
		}
	} else if (client.shm == NULL || client.toplevel_list == NULL ||
			client.source_manager == NULL || client.capture_manager == NULL) {
		fprintf(stderr, "compositor is missing wl_shm (%p), "
			"ext_foreign_toplevel_list_v1 (%p), "
			"ext_foreign_toplevel_image_capture_source_manager_v1 (%p) or "
			"ext_image_copy_capture_manager_v1 (%p)\n",
			(void *)client.shm, (void *)client.toplevel_list,
			(void *)client.source_manager, (void *)client.capture_manager);
		return 2;
	}

	struct ext_image_capture_source_v1 *source;
	if (output_mode) {
		source = ext_output_image_capture_source_manager_v1_create_source(
			client.output_source_manager, client.output);
		goto have_source;
	}

	struct ext_foreign_toplevel_handle_v1 *target = NULL;
	for (size_t i = 0; i < client.toplevels_len; i++) {
		if (client.toplevels[i].app_id != NULL &&
				strcmp(client.toplevels[i].app_id, want_app_id) == 0) {
			target = client.toplevels[i].handle;
			break;
		}
	}
	check(target != NULL, "the window under test is published as a toplevel");
	if (target == NULL) {
		fprintf(stderr, "no toplevel with app_id '%s' among %zu\n", want_app_id,
			client.toplevels_len);
		return 2;
	}

	source = ext_foreign_toplevel_image_capture_source_manager_v1_create_source(
		client.source_manager, target);

have_source:;
	struct ext_image_copy_capture_session_v1 *session =
		ext_image_copy_capture_manager_v1_create_session(
			client.capture_manager, source, 0);
	ext_image_copy_capture_session_v1_add_listener(session, &session_listener,
		&client);

	/* A compositor that publishes the manager but ignores the request kills
	 * the session here rather than answering, which is what reaches a browser
	 * as a bare NotAllowedError. */
	bool got_constraints = false;
	int64_t deadline = now_ms() + 5000;
	while (client.constraints_done == 0 && now_ms() < deadline) {
		if (!wait_until(display, &client.session_stopped, 200) &&
				client.constraints_done > 0) {
			break;
		}
		if (client.session_stopped) {
			break;
		}
	}
	got_constraints = client.constraints_done > 0 && !client.session_stopped;
	check(got_constraints, "the compositor accepts a capture of that window");
	if (!got_constraints) {
		return 2;
	}

	if (!output_mode) {
		check(client.buffer_width == want_width &&
				client.buffer_height == want_height,
			"the capture is the window, not the surface: got %dx%d, want %dx%d",
			client.buffer_width, client.buffer_height, want_width, want_height);
	}

	check(client.have_shm_format, "the capture offers a shm format we can read");
	if (!client.have_shm_format) {
		return 2;
	}

	struct shm_buffer buffer;
	if (!shm_buffer_create(&buffer, client.shm, client.buffer_width,
			client.buffer_height, client.shm_format)) {
		fprintf(stderr, "cannot allocate a %dx%d shm buffer: %s\n",
			client.buffer_width, client.buffer_height, strerror(errno));
		return 2;
	}

	bool captured = capture_one_frame(&client, display, session, &buffer, 5000);
	check(captured, "a frame arrives%s",
		client.frame_failed ? " (the compositor reported failure)" : "");
	if (!captured) {
		return 1;
	}

	/* Every pixel, not a sample: the failure this catches is a region of the
	 * frame belonging to something else, and a sample can miss a region. */
	int64_t wrong = 0, matching = 0;
	int32_t first_x = -1, first_y = -1;
	uint32_t first_value = 0;
	for (int32_t y = 0; y < buffer.height; y++) {
		for (int32_t x = 0; x < buffer.width; x++) {
			uint32_t pixel = buffer.pixels[y * buffer.width + x] & 0x00FFFFFF;
			if (pixel == want_colour) {
				matching++;
			}
			if (pixel != want_colour) {
				if (wrong == 0) {
					first_x = x;
					first_y = y;
					first_value = pixel;
				}
				wrong++;
			}
		}
	}

	const char *what = output_mode
		? "every pixel of the screen is the colour it should be"
		: "every pixel is the window's own colour";
	bool pixels_ok = wrong == 0;
	if (output_has) {
		what = "the screen contains the expected colour";
		pixels_ok = matching > 0;
	} else if (output_not) {
		what = "the screen contains none of the protected colour";
		pixels_ok = matching == 0;
	}
	if (pixels_ok) {
		check(true, "%s", what);
	} else if (output_has || output_not) {
		check(false, "%s: %lld of %lld pixels matched %06x", what,
			(long long)matching, (long long)buffer.width * buffer.height,
			want_colour);
	} else {
		check(false, "%s: %lld of %lld wrong, first at %d,%d is %06x not %06x",
			what, (long long)wrong, (long long)buffer.width * buffer.height,
			first_x, first_y, first_value, want_colour);
	}

	if (output_mode) {
		ext_image_copy_capture_session_v1_destroy(session);
		ext_image_capture_source_v1_destroy(source);
		wl_display_roundtrip(display);
		wl_display_disconnect(display);
		printf("\n%s\n", failures == 0 ? "all checks passed" : "checks FAILED");
		return failures == 0 ? 0 : 1;
	}

	/* Now leave the session running against a window that is redrawing but
	 * never resizing. Constraints should be settled; a stream of them is the
	 * renegotiation that used to stall the client. */
	int constraints_before = client.constraints_done;
	int64_t watch_deadline = now_ms() + watch_ms;
	while (now_ms() < watch_deadline) {
		capture_one_frame(&client, display, session, &buffer, 1000);
		pump_for(display, 50);
	}
	int churn = client.constraints_done - constraints_before;

	check(churn <= 1,
		"the capture size holds still while the window does: %d further "
		"constraint announcements in %dms", churn, watch_ms);

	check(client.frames_ready >= 2,
		"frames keep arriving: %d in total", client.frames_ready);

	ext_image_copy_capture_session_v1_destroy(session);
	ext_image_capture_source_v1_destroy(source);
	wl_display_roundtrip(display);
	wl_display_disconnect(display);

	printf("\n%s\n", failures == 0 ? "all checks passed" : "checks FAILED");
	return failures == 0 ? 0 : 1;
}
