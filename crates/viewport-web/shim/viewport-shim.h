/* SPDX-License-Identifier: MIT
 *
 * The WPEPlatform subclasses, and nothing else.
 *
 * This is the one part of the rewrite that stays C. WPEPlatform inverts the
 * old libwpe arrangement: rather than handing WebKit a backend object, the
 * embedder subclasses WPEDisplay and WPEView and WebKit calls into them. That
 * is GObject subclassing — a class struct whose vfunc pointers are filled in
 * at class_init — and doing it from Rust means hand-rolling
 * g_type_register_static plus a trampoline per vfunc, where a mistake is a
 * segfault rather than a compile error. The C compiler checks these
 * assignments against WPE's own headers for free.
 *
 * Everything above this layer is Rust. The shim knows nothing about
 * compositors, Wayland or Vulkan: it takes a callback table, and hands back
 * each painted frame as a plain struct of dma-buf fds and metadata.
 */
#ifndef VIEWPORT_SHIM_H
#define VIEWPORT_SHIM_H

#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* A frame WebKit has finished painting.
 *
 * Field for field what a dma-buf import needs. The fds are borrowed for the
 * duration of the callback; anything that outlives it must dup them. */
typedef struct {
	uint32_t width;
	uint32_t height;
	uint32_t format;   /* DRM fourcc */
	uint64_t modifier;

	uint32_t n_planes;
	int32_t fds[4];
	uint32_t offsets[4];
	uint32_t strides[4];

	/* WebKit's rendering may still be in flight. -1 when it is not, or when
	 * the engine could not produce a fence. Borrowed, like the plane fds. */
	int32_t fence_fd;

	/* The WPEBuffer this describes.
	 *
	 * Two separate things have to happen to it, and doing only the first
	 * stalls the engine. Hand it to viewport_shim_frame_done() once the frame
	 * has been presented — that advances WebKit's frame clock, which is what
	 * keeps the shell on vblank rather than free-running. Then, once nothing
	 * samples the memory any more, give the buffer itself back with
	 * WebView::frame_release(); acknowledging alone drains the pool and WebKit
	 * stops painting with the last frame still on screen. */
	void *token;
} ViewportShimFrame;

/* What the shim calls into.
 *
 * `user` is passed back untouched. Every one of these runs on the thread that
 * drives the GLib main context, which is the compositor's thread.
 */
typedef struct {
	void *user;

	/* A frame is ready. Return true if it was accepted; false makes WebKit
	 * treat the frame as failed. */
	bool (*render_frame)(void *user, const ViewportShimFrame *frame);
} ViewportShimCallbacks;

/* The DRM device WebKit should allocate on, and the formats it may pick.
 *
 * The render node must be the one backing the compositor's renderer, or
 * WebKit allocates buffers that cannot then be imported. `formats` is a flat
 * array of (fourcc, modifier) pairs. */
typedef struct {
	/* Both required. wpe_drm_device_new() asserts on a NULL primary node —
	 * the assertion is a GLib CRITICAL rather than a failure, so a NULL one
	 * yields a display with no device and a connect() that fails later for
	 * an unrelated-looking reason. */
	const char *primary_node;
	const char *render_node;

	const uint32_t *format_codes;
	const uint64_t *format_modifiers;
	uint32_t n_formats;
} ViewportShimDisplayConfig;

typedef struct _ViewportShimDisplay ViewportShimDisplay;

/* Create the display. Returns NULL and sets *error_out to a message the
 * caller must free with viewport_shim_string_free(). */
ViewportShimDisplay *viewport_shim_display_new(
	const ViewportShimDisplayConfig *config,
	const ViewportShimCallbacks *callbacks,
	char **error_out);

void viewport_shim_display_free(ViewportShimDisplay *display);

/* The underlying WPEDisplay, for handing to webkit_web_view_new(). Borrowed. */
void *viewport_shim_display_handle(ViewportShimDisplay *display);

/* Acknowledge a frame. `token` comes from ViewportShimFrame.
 *
 * Takes the display because a buffer cannot be asked which view it belongs
 * to; the shim remembers the view it created. */
void viewport_shim_frame_done(ViewportShimDisplay *display, void *token);

/* Tell WebKit the view changed size. */
void viewport_shim_display_resize(ViewportShimDisplay *display,
	uint32_t width, uint32_t height);

/* Map the view and give it focus.
 *
 * An unmapped view is never painted into, so without this the page loads, its
 * scripts run, it talks to the compositor — and no frame ever arrives. */
void viewport_shim_display_show(ViewportShimDisplay *display);

void viewport_shim_string_free(char *string);

#ifdef __cplusplus
}
#endif

#endif /* VIEWPORT_SHIM_H */
