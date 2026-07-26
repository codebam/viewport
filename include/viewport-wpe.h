/* SPDX-License-Identifier: MIT
 *
 * WPEPlatform subclasses.
 *
 * WPEPlatform (WPE WebKit >= 2.44, mandatory from the 2.0 API) inverts the old
 * libwpe arrangement: instead of handing WebKit a `wpe_view_backend` built by
 * an external backend library, we subclass WPEDisplay and WPEView ourselves.
 * WebKit then allocates its own buffers on the DRM device we advertise and
 * hands each finished frame to our `render_buffer` vfunc as a WPEBufferDMABuf.
 *
 * That buffer already carries dma-buf fds, offsets, strides and a modifier, so
 * it maps onto `struct wlr_dmabuf_attributes` field for field — no EGL image
 * export, no gbm device of our own, no pixel ever touching the CPU.
 */
#ifndef VIEWPORT_WPE_H
#define VIEWPORT_WPE_H

#include <glib-object.h>
#include <wpe/wpe-platform.h>

#include "viewport.h"

G_BEGIN_DECLS

/* -------------------------------------------------------------------------
 * Display
 *
 * Advertises the compositor's DRM device and format set to WebKit, and mints
 * our WPEView / WPEToplevel subclasses.
 * ---------------------------------------------------------------------- */

#define VIEWPORT_TYPE_WPE_DISPLAY (viewport_wpe_display_get_type())
G_DECLARE_FINAL_TYPE(ViewportWPEDisplay, viewport_wpe_display,
	VIEWPORT, WPE_DISPLAY, WPEDisplay)

/* `primary_node` may be NULL when running headless; `render_node` is required
 * and must match the node backing the wlr_renderer, or WebKit will allocate
 * buffers the compositor's GPU cannot import. */
ViewportWPEDisplay *viewport_wpe_display_new(struct viewport_server *server,
	const char *primary_node, const char *render_node);

struct viewport_server *viewport_wpe_display_get_server(
	ViewportWPEDisplay *display);

/* -------------------------------------------------------------------------
 * View
 *
 * One view backs the whole shell. `render_buffer` is the hot path: it wraps
 * the incoming WPEBufferDMABuf in a wlr_buffer and hands it to the scene.
 * ---------------------------------------------------------------------- */

#define VIEWPORT_TYPE_WPE_VIEW (viewport_wpe_view_get_type())
G_DECLARE_FINAL_TYPE(ViewportWPEView, viewport_wpe_view,
	VIEWPORT, WPE_VIEW, WPEView)

/* Associates the view with the web layer it renders into. Called by web.c
 * once the WebKitWebView has been constructed. */
void viewport_wpe_view_set_web(ViewportWPEView *view, struct viewport_web *web);

/* -------------------------------------------------------------------------
 * Toplevel
 *
 * WebKit expects a toplevel to exist even though our shell is never framed by
 * anything. This one just tracks size and reports success.
 * ---------------------------------------------------------------------- */

#define VIEWPORT_TYPE_WPE_TOPLEVEL (viewport_wpe_toplevel_get_type())
G_DECLARE_FINAL_TYPE(ViewportWPEToplevel, viewport_wpe_toplevel,
	VIEWPORT, WPE_TOPLEVEL, WPEToplevel)

G_END_DECLS

#endif /* VIEWPORT_WPE_H */
