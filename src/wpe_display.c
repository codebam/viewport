/* SPDX-License-Identifier: MIT
 *
 * The WPEDisplay subclass.
 *
 * This object is how the compositor introduces itself to WebKit. Two of its
 * vfuncs carry the whole zero-copy contract:
 *
 *   get_drm_device                names the DRM node WebKit must allocate on.
 *                                 Get this wrong — advertise a different GPU
 *                                 than the one backing wlr_renderer — and every
 *                                 frame either fails to import or silently
 *                                 round-trips through system memory.
 *
 *   get_preferred_buffer_formats  the exact fourcc + modifier pairs our
 *                                 renderer can sample. WebKit intersects this
 *                                 with what its own GPU path can produce, so a
 *                                 too-generous list here becomes an import
 *                                 failure later rather than a negotiation
 *                                 failure now.
 *
 * Both are answered straight from wlr_renderer, so they cannot drift.
 */
#define _POSIX_C_SOURCE 200809L

#include <stdlib.h>
#include <string.h>
#include <xf86drm.h>

#include <wlr/render/drm_format_set.h>
#include <wlr/render/wlr_renderer.h>
#include <wlr/types/wlr_output.h>
#include <wlr/types/wlr_buffer.h>
#include <wlr/util/log.h>

#include "viewport-wpe.h"

/* ------------------------------------------------------------------------
 * Toplevel
 * --------------------------------------------------------------------- */

struct _ViewportWPEToplevel {
	WPEToplevel parent;
};

G_DEFINE_FINAL_TYPE(ViewportWPEToplevel, viewport_wpe_toplevel,
	WPE_TYPE_TOPLEVEL)

static gboolean viewport_wpe_toplevel_resize(WPEToplevel *toplevel, int width,
	int height)
{
	/* The shell is always exactly the size of the output layout; WebKit only
	 * learns about size changes through wpe_toplevel_resized(), which
	 * viewport_web_resize() drives. Accepting here keeps WebKit's internal
	 * bookkeeping consistent. */
	wpe_toplevel_resized(toplevel, width, height);
	return TRUE;
}

static gboolean viewport_wpe_toplevel_set_fullscreen(WPEToplevel *toplevel,
	gboolean fullscreen)
{
	/* Already the full output — report success so the Fullscreen API works
	 * from JS without the shell needing to special-case it. */
	return TRUE;
}

static void viewport_wpe_toplevel_set_title(WPEToplevel *toplevel,
	const char *title)
{
}

static void viewport_wpe_toplevel_class_init(ViewportWPEToplevelClass *klass)
{
	WPEToplevelClass *toplevel_class = WPE_TOPLEVEL_CLASS(klass);
	toplevel_class->resize = viewport_wpe_toplevel_resize;
	toplevel_class->set_fullscreen = viewport_wpe_toplevel_set_fullscreen;
	toplevel_class->set_title = viewport_wpe_toplevel_set_title;
}

static void viewport_wpe_toplevel_init(ViewportWPEToplevel *self)
{
}

/* ------------------------------------------------------------------------
 * Display
 * --------------------------------------------------------------------- */

struct _ViewportWPEDisplay {
	WPEDisplay parent;

	struct viewport_server *server;
	WPEDRMDevice *drm_device;
	WPEBufferFormats *formats;
	WPEKeymap *keymap;
};

G_DEFINE_FINAL_TYPE(ViewportWPEDisplay, viewport_wpe_display, WPE_TYPE_DISPLAY)

static gboolean viewport_wpe_display_connect(WPEDisplay *display,
	GError **error)
{
	ViewportWPEDisplay *self = VIEWPORT_WPE_DISPLAY(display);

	if (self->drm_device == NULL) {
		g_set_error_literal(error, WPE_DISPLAY_ERROR,
			WPE_DISPLAY_ERROR_CONNECTION_FAILED,
			"no DRM render node available for the compositor's renderer");
		return FALSE;
	}
	return TRUE;
}

static WPEView *viewport_wpe_display_create_view(WPEDisplay *display)
{
	return WPE_VIEW(g_object_new(VIEWPORT_TYPE_WPE_VIEW, "display", display,
		NULL));
}

static WPEToplevel *viewport_wpe_display_create_toplevel(WPEDisplay *display,
	guint max_views)
{
	return WPE_TOPLEVEL(g_object_new(VIEWPORT_TYPE_WPE_TOPLEVEL, "display",
		display, NULL));
}

static WPEDRMDevice *viewport_wpe_display_get_drm_device(WPEDisplay *display)
{
	return VIEWPORT_WPE_DISPLAY(display)->drm_device;
}

static WPEBufferFormats *viewport_wpe_display_get_preferred_buffer_formats(
	WPEDisplay *display)
{
	/* Ownership: WPE wraps the return value in adoptGRef(), i.e. this vfunc is
	 * transfer-full and the caller consumes a reference. Returning the cached
	 * pointer bare — while also releasing it in dispose — unrefs it twice:
	 *   g_object_unref: assertion 'G_IS_OBJECT (object)' failed
	 * from inside ~_WPEDisplayPrivate. So hand back a fresh reference every
	 * time and keep ours for dispose. (get_keymap is transfer-none by
	 * contrast — WPE assigns it to a GRefPtr without adopting — so that one
	 * correctly returns a borrowed pointer.) */
	ViewportWPEDisplay *self = VIEWPORT_WPE_DISPLAY(display);
	if (self->formats != NULL) {
		return g_object_ref(self->formats);
	}

	const struct wlr_drm_format_set *set = wlr_renderer_get_texture_formats(
		self->server->renderer, WLR_BUFFER_CAP_DMABUF);
	if (set == NULL || self->drm_device == NULL) {
		return NULL;
	}

	/* What the display hardware can put on a plane, as opposed to what the
	 * renderer can sample from. The two are not the same set — a render target
	 * may be tiled or colour-compressed in a way the display engine cannot read
	 * — and WebKit picks its allocation from whichever group it is offered.
	 *
	 * Offered only rendering formats, it allocates a buffer the scanout test
	 * then rejects, so the shell is composited on every output it appears on,
	 * every frame. That is the whole desktop at full refresh while nothing is
	 * happening, and a second monitor showing nothing but the shell repainting
	 * at 240Hz beside a game that wants the GPU.
	 *
	 * VIEWPORT_NO_SCANOUT_FORMATS=1 turns this off without a rebuild. It is
	 * here because an earlier version of this change was followed by a session
	 * that froze — the compositor's event loop stayed responsive while the
	 * screen stopped updating — and that was never pinned on it either way. A
	 * switch that can be flipped from a TTY is worth more than being sure. */
	const struct wlr_drm_format_set *scanout = NULL;
	const char *why = NULL;
	if (getenv("VIEWPORT_NO_SCANOUT_FORMATS") != NULL) {
		why = "disabled by VIEWPORT_NO_SCANOUT_FORMATS";
	} else if (wl_list_empty(&self->server->outputs)) {
		/* Nothing to ask. This was the state for the whole session until the
		 * backend was started before the web engine rather than after it. */
		why = "no output exists yet";
	} else {
		struct viewport_output *first =
			wl_container_of(self->server->outputs.next, first, link);
		scanout = wlr_output_get_primary_formats(first->wlr_output,
			WLR_BUFFER_CAP_DMABUF);
		if (scanout == NULL) {
			/* Documented to mean "no constraint, every format works" rather
			 * than "none do" — which is what the headless backend reports, and
			 * why this cannot be tested without real display hardware. */
			why = "backend has no scanout format constraint";
		} else if (scanout->len == 0) {
			why = "backend supports no scanout formats";
		}
	}

	WPEBufferFormatsBuilder *builder =
		wpe_buffer_formats_builder_new(self->drm_device);

	/* Scanout first: the order is the preference order, and a buffer that can
	 * be flipped is worth more than one that can only be sampled. */
	size_t offered = 0;
	if (scanout != NULL && scanout->len > 0) {
		wpe_buffer_formats_builder_append_group(builder, self->drm_device,
			WPE_BUFFER_FORMAT_USAGE_SCANOUT);
		for (size_t i = 0; i < scanout->len; i++) {
			const struct wlr_drm_format *format = &scanout->formats[i];
			/* Only formats the renderer can also sample: the shell is
			 * composited rather than flipped whenever anything overlaps it,
			 * which is most of the time, and a buffer we could not texture
			 * from would be useless then. */
			if (!wlr_drm_format_set_get(set, format->format)) {
				continue;
			}
			for (size_t j = 0; j < format->len; j++) {
				wpe_buffer_formats_builder_append_format(builder,
					format->format, format->modifiers[j]);
				offered++;
			}
		}
	}

	/* One rendering group on our own device. WebKit uses the usage hint to
	 * decide whether a format is a candidate for direct scanout. */
	wpe_buffer_formats_builder_append_group(builder, self->drm_device,
		WPE_BUFFER_FORMAT_USAGE_RENDERING);

	for (size_t i = 0; i < set->len; i++) {
		const struct wlr_drm_format *format = &set->formats[i];
		for (size_t j = 0; j < format->len; j++) {
			wpe_buffer_formats_builder_append_format(builder, format->format,
				format->modifiers[j]);
		}
	}

	WPEBufferFormats *formats = wpe_buffer_formats_builder_end(builder);

	wlr_log(WLR_INFO, "shell buffer formats: %zu scanout, rendering always%s%s%s",
		offered, why != NULL ? " (" : "", why != NULL ? why : "",
		why != NULL ? ")" : "");

	/* Only cached once an output existed to ask. This runs during startup, and
	 * on the first call there may be no output yet — caching that answer would
	 * pin the shell to rendering-only formats for the life of the session. */
	if (scanout != NULL) {
		self->formats = formats;
		return g_object_ref(self->formats);
	}
	return formats;
}

static WPEKeymap *viewport_wpe_display_get_keymap(WPEDisplay *display)
{
	ViewportWPEDisplay *self = VIEWPORT_WPE_DISPLAY(display);
	if (self->keymap == NULL) {
		/* The compositor owns the real keymap and resolves keysyms before
		 * dispatching, so this only backs WebKit's own translation table. */
		self->keymap = wpe_keymap_xkb_new();
	}
	return self->keymap;
}

static gboolean viewport_wpe_display_use_explicit_sync(WPEDisplay *display)
{
	/* We import WebKit's per-frame rendering fence into a drm_syncobj
	 * timeline and let wlr_scene wait on it, so we can honour this. */
	return TRUE;
}

static void viewport_wpe_display_dispose(GObject *object)
{
	ViewportWPEDisplay *self = VIEWPORT_WPE_DISPLAY(object);

	/* WPEDRMDevice is a boxed type with its own ref/unref; WPEBufferFormats
	 * and WPEKeymap are GObjects. Mixing the two up compiles into a call to a
	 * symbol that does not exist. */
	g_clear_pointer(&self->drm_device, wpe_drm_device_unref);
	g_clear_object(&self->formats);
	g_clear_object(&self->keymap);

	G_OBJECT_CLASS(viewport_wpe_display_parent_class)->dispose(object);
}

static void viewport_wpe_display_class_init(ViewportWPEDisplayClass *klass)
{
	GObjectClass *object_class = G_OBJECT_CLASS(klass);
	object_class->dispose = viewport_wpe_display_dispose;

	WPEDisplayClass *display_class = WPE_DISPLAY_CLASS(klass);
	display_class->connect = viewport_wpe_display_connect;
	display_class->create_view = viewport_wpe_display_create_view;
	display_class->create_toplevel = viewport_wpe_display_create_toplevel;
	display_class->get_drm_device = viewport_wpe_display_get_drm_device;
	display_class->get_preferred_buffer_formats =
		viewport_wpe_display_get_preferred_buffer_formats;
	display_class->get_keymap = viewport_wpe_display_get_keymap;
	display_class->use_explicit_sync = viewport_wpe_display_use_explicit_sync;
}

static void viewport_wpe_display_init(ViewportWPEDisplay *self)
{
}

ViewportWPEDisplay *viewport_wpe_display_new(struct viewport_server *server,
	const char *primary_node, const char *render_node)
{
	ViewportWPEDisplay *self = g_object_new(VIEWPORT_TYPE_WPE_DISPLAY, NULL);
	self->server = server;

	if (render_node != NULL) {
		self->drm_device = wpe_drm_device_new(primary_node, render_node);
	} else {
		wlr_log(WLR_ERROR, "no render node; WPE cannot allocate dma-bufs");
	}

	return self;
}

struct viewport_server *viewport_wpe_display_get_server(
	ViewportWPEDisplay *display)
{
	return display->server;
}
