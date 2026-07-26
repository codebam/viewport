/* SPDX-License-Identifier: MIT
 *
 * The WPEView subclass — the hot path.
 *
 * WebKit calls render_buffer() once per painted frame. We wrap the frame's
 * dma-buf in a wlr_buffer, hand it to the scene node that covers the output,
 * and return. The next vblank composites it.
 *
 * Two handshakes keep this from either tearing or free-running:
 *
 *   rendering fence  WebKit's GPU work may still be in flight when it hands
 *                    us the buffer. It attaches a sync_file; we import it into
 *                    a drm_syncobj timeline and let wlr_scene wait on that
 *                    point instead of blocking the compositor thread.
 *
 *   buffer_rendered  WebKit will not paint frame N+1 until we acknowledge
 *                    frame N. We acknowledge from the output's frame handler,
 *                    so the shell's paint rate is driven by actual vblank
 *                    rather than a timer that drifts against it.
 */
#define _POSIX_C_SOURCE 200809L

#include <pixman.h>
#include <unistd.h>

#include <wlr/render/drm_syncobj.h>
#include <wlr/types/wlr_output.h>
#include <wlr/types/wlr_scene.h>
#include <wlr/util/log.h>

#include "viewport-wpe.h"
#include "web-internal.h"

struct _ViewportWPEView {
	WPEView parent;
	struct viewport_web *web;
};

G_DEFINE_FINAL_TYPE(ViewportWPEView, viewport_wpe_view, WPE_TYPE_VIEW)

/* ------------------------------------------------------------------------
 * Frame submission
 * --------------------------------------------------------------------- */

static bool import_rendering_fence(struct viewport_web *web,
	WPEBuffer *buffer, uint64_t *point_out)
{
	if (web->timeline == NULL) {
		return false;
	}

	int fence_fd = wpe_buffer_get_rendering_fence(buffer);
	if (fence_fd < 0) {
		return false;
	}

	uint64_t point = ++web->timeline_point;
	if (!wlr_drm_syncobj_timeline_import_sync_file(web->timeline, point,
			fence_fd)) {
		wlr_log(WLR_ERROR, "failed to import WPE rendering fence");
		return false;
	}

	*point_out = point;
	return true;
}

void viewport_web_present_buffer(struct viewport_web *web, WPEBuffer *buffer,
	const pixman_region32_t *damage)
{
	struct wlr_buffer *wlr_buffer = viewport_web_buffer_from_wpe(web, buffer);
	if (wlr_buffer == NULL) {
		/* Nothing to show, but WebKit is still blocked on an acknowledgement
		 * for this frame — release it or the shell freezes permanently. */
		wpe_view_buffer_rendered(web->wpe_view, buffer);
		wpe_view_buffer_released(web->wpe_view, buffer);
		return;
	}

	/* Damage rides along with the buffer. Setting it in a second call would
	 * mean passing a NULL buffer, which hides the node rather than annotating
	 * it — the scene treats "no buffer" as "do not display". */
	struct wlr_scene_buffer_set_buffer_options options = {
		.damage = damage,
	};
	uint64_t wait_point;
	if (import_rendering_fence(web, buffer, &wait_point)) {
		options.wait_timeline = web->timeline;
		options.wait_point = wait_point;
	}

	wlr_scene_buffer_set_buffer_with_options(web->scene_buffer, wlr_buffer,
		&options);

	/* The scene took its own reference; drop ours. */
	wlr_buffer_drop(wlr_buffer);

	/* An unacknowledged frame here means WebKit outran the display, which it
	 * should not be able to do. Acknowledge the stale one so we cannot wedge. */
	if (web->pending != NULL) {
		wpe_view_buffer_rendered(web->wpe_view, web->pending);
		g_object_unref(web->pending);
	}
	web->pending = g_object_ref(buffer);

	/* Guarantee a frame event follows, because the acknowledgement WebKit is
	 * waiting for is only sent from an output's frame handler.
	 *
	 * viewport_web_create() runs before the backend is started, so WebKit
	 * paints its first frame while there are still no outputs at all. That
	 * frame is stored as pending, nothing ever acknowledges it, and WebKit
	 * blocks forever — the shell is stuck on its first frame until some
	 * unrelated input damages the scene and produces a frame event by
	 * accident. Scheduling here closes that gap, and also covers the case
	 * where the scene finds nothing to repaint. */
	int scheduled = 0;
	struct viewport_output *output;
	wl_list_for_each(output, &web->server->outputs, link) {
		wlr_output_schedule_frame(output->wlr_output);
		scheduled++;
	}

	/* Watchdog.
	 *
	 * The acknowledgement WebKit is waiting for is normally sent from an
	 * output's frame handler, which means the shell only keeps painting for as
	 * long as the backend keeps delivering frame events. That is not
	 * guaranteed: scheduling a frame after a commit that had nothing to repaint
	 * does not reliably produce one, and at startup WebKit paints before any
	 * output exists at all. When it fails the shell wedges permanently on its
	 * current frame, and the user sees a desktop that only updates when
	 * something unrelated — moving the mouse — happens to damage the scene.
	 *
	 * Rather than depend on the backend to rescue us, acknowledge unilaterally
	 * if the frame has not been confirmed shortly. Acknowledging late costs a
	 * frame of latency; not acknowledging costs the whole session. */
	(void)scheduled;

	if (web->ack_watchdog_id != 0) {
		g_source_remove(web->ack_watchdog_id);
	}
	web->ack_watchdog_id = g_timeout_add(50, viewport_web_ack_watchdog, web);
}

/* Fires when an output frame has not confirmed the pending frame in time. */
gboolean viewport_web_ack_watchdog(gpointer user_data)
{
	struct viewport_web *web = user_data;
	web->ack_watchdog_id = 0;

	if (web->pending != NULL) {
		wlr_log(WLR_DEBUG, "frame not confirmed by a vblank; acknowledging");
		viewport_web_notify_presented(web);
	}

	return G_SOURCE_REMOVE;
}

void viewport_web_notify_presented(struct viewport_web *web)
{
	if (web->ack_watchdog_id != 0) {
		g_source_remove(web->ack_watchdog_id);
		web->ack_watchdog_id = 0;
	}

	if (web->pending == NULL) {
		return;
	}

	WPEBuffer *buffer = web->pending;
	web->pending = NULL;

	wpe_view_buffer_rendered(web->wpe_view, buffer);
	g_object_unref(buffer);
}

/* ------------------------------------------------------------------------
 * vfuncs
 * --------------------------------------------------------------------- */

static gboolean viewport_wpe_view_render_buffer(WPEView *wpe_view,
	WPEBuffer *buffer, const WPERectangle *damage_rects, guint n_damage_rects,
	GError **error)
{
	ViewportWPEView *self = VIEWPORT_WPE_VIEW(wpe_view);

	if (self->web == NULL || self->web->scene_buffer == NULL) {
		/* Frames can arrive before the scene node exists. Swallow them
		 * cleanly so WebKit keeps its buffer pool balanced. */
		wpe_view_buffer_rendered(wpe_view, buffer);
		wpe_view_buffer_released(wpe_view, buffer);
		return TRUE;
	}

	self->web->first_frame_seen = true;

	/* Damage is advisory but worth forwarding: with no region the scene
	 * repaints the whole output every frame, so a blinking caret in a text
	 * field would cost a full-screen composite. A NULL region means exactly
	 * that "repaint everything", which is the correct conservative answer
	 * when WebKit reports no rects. */
	if (n_damage_rects == 0) {
		viewport_web_present_buffer(self->web, buffer, NULL);
		return TRUE;
	}

	pixman_region32_t damage;
	pixman_region32_init(&damage);
	for (guint i = 0; i < n_damage_rects; i++) {
		pixman_region32_union_rect(&damage, &damage, damage_rects[i].x,
			damage_rects[i].y, damage_rects[i].width, damage_rects[i].height);
	}

	/* Bounded diagnostics: the first frames are what matter, and logging every
	 * frame at 120Hz floods the log faster than it can be read. */
	static int logged;
	if (self->web->server->config.debug && logged < 40) {
		logged++;
		const pixman_box32_t *extents = pixman_region32_extents(&damage);
		wlr_log(WLR_DEBUG,
			"wpe frame: %u rect(s), extents %d,%d %dx%d, buffer %dx%d",
			n_damage_rects, extents->x1, extents->y1,
			extents->x2 - extents->x1, extents->y2 - extents->y1,
			wpe_buffer_get_width(buffer), wpe_buffer_get_height(buffer));
	}

	viewport_web_present_buffer(self->web, buffer, &damage);
	pixman_region32_fini(&damage);

	return TRUE;
}

static gboolean viewport_wpe_view_can_be_mapped(WPEView *view)
{
	return TRUE;
}

static void viewport_wpe_view_set_cursor_from_name(WPEView *view,
	const char *name)
{
	/* The shell styles its own cursor with CSS; the compositor's xcursor is
	 * only used over client windows. Nothing to do. */
}

static void viewport_wpe_view_class_init(ViewportWPEViewClass *klass)
{
	WPEViewClass *view_class = WPE_VIEW_CLASS(klass);
	view_class->render_buffer = viewport_wpe_view_render_buffer;
	view_class->can_be_mapped = viewport_wpe_view_can_be_mapped;
	view_class->set_cursor_from_name = viewport_wpe_view_set_cursor_from_name;
}

static void viewport_wpe_view_init(ViewportWPEView *self)
{
}

void viewport_wpe_view_set_web(ViewportWPEView *view, struct viewport_web *web)
{
	view->web = web;
}
