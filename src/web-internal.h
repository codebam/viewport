/* SPDX-License-Identifier: MIT */
#ifndef VIEWPORT_WEB_INTERNAL_H
#define VIEWPORT_WEB_INTERNAL_H

#include <gio/gio.h>
#include <glib.h>
#include <pixman.h>
#include <wpe/webkit.h>
#include <wpe/wpe-platform.h>

#include <wlr/render/drm_syncobj.h>
#include <wlr/types/wlr_scene.h>

#include "viewport-wpe.h"
#include "viewport.h"

struct viewport_web {
	struct viewport_server *server;

	ViewportWPEDisplay *display;
	WebKitWebView *web_view;
	WPEView *wpe_view;
	WebKitUserContentManager *ucm;

	/* The shell's node in the scene graph — bottom layer, full output. */
	struct wlr_scene_buffer *scene_buffer;
	int width, height;

	/* Timeline used to translate WebKit's per-frame rendering fence (a
	 * sync_file fd) into something wlr_scene can wait on. Monotonic point
	 * counter; a fresh point per frame. */
	struct wlr_drm_syncobj_timeline *timeline;
	uint64_t timeline_point;

	/* Frame handed to the scene but not yet confirmed on screen. WebKit will
	 * not paint the next frame until we acknowledge this one. */
	WPEBuffer *pending;

	/* Every live wlr_buffer wrapping one of our frames.
	 *
	 * These can outlive this struct: with direct scan-out the DRM backend
	 * keeps them as scanout framebuffers and only releases them from
	 * wlr_backend_destroy(), which runs after viewport_web_destroy(). Their
	 * back-pointer has to be cleared rather than left dangling. */
	struct wl_list buffers;

	bool first_frame_seen;
	bool using_fallback;
	guint load_timeout_id;

	/* Live reload of a file:// shell while --debug is on. */
	GFileMonitor *watch;
	guint watch_debounce_id;

	/* Safety net for the frame handshake; see viewport_web_present_buffer(). */
	guint ack_watchdog_id;

	/* DRM node paths advertised to WebKit; owned. */
	char *primary_node;
	char *render_node;
};

gboolean viewport_web_ack_watchdog(gpointer user_data);

/* wpe_view.c — called by the render_buffer vfunc. `damage` may be NULL,
 * meaning the whole buffer changed. */
void viewport_web_present_buffer(struct viewport_web *web, WPEBuffer *buffer,
	const pixman_region32_t *damage);

/* web_buffer.c */
struct wlr_buffer *viewport_web_buffer_from_wpe(struct viewport_web *web,
	WPEBuffer *buffer);

/* Clears the back-pointer on every live buffer, so ones that outlive the
 * engine cannot dereference it. Must be called before freeing viewport_web. */
void viewport_web_buffers_detach(struct viewport_web *web);

#endif /* VIEWPORT_WEB_INTERNAL_H */
