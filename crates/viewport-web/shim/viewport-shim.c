/* SPDX-License-Identifier: MIT
 *
 * WPEPlatform subclasses. Adapted from src/wpe_display.c and src/wpe_view.c of
 * the C compositor, with everything compositor-specific replaced by the
 * callback table in viewport-shim.h — this file knows nothing about Wayland,
 * wlroots or Vulkan.
 */
#include <glib-object.h>
#include <wpe/wpe-platform.h>

#include "viewport-shim.h"

/* -------------------------------------------------------------------------
 * Toplevel
 *
 * WebKit expects one to exist even though the shell is never framed by
 * anything. It only has to track a size and report success.
 * ---------------------------------------------------------------------- */

#define VIEWPORT_TYPE_SHIM_TOPLEVEL (viewport_shim_toplevel_get_type())
G_DECLARE_FINAL_TYPE(ViewportShimToplevel, viewport_shim_toplevel,
	VIEWPORT, SHIM_TOPLEVEL, WPEToplevel)

struct _ViewportShimToplevel {
	WPEToplevel parent;
};

G_DEFINE_FINAL_TYPE(ViewportShimToplevel, viewport_shim_toplevel,
	WPE_TYPE_TOPLEVEL)

static gboolean viewport_shim_toplevel_resize(WPEToplevel *toplevel,
	int width, int height)
{
	/* Accepting the size is the whole contract: nothing here can refuse it,
	 * because there is no window manager above us to argue with. */
	wpe_toplevel_resized(toplevel, width, height);
	return TRUE;
}

static void viewport_shim_toplevel_class_init(ViewportShimToplevelClass *klass)
{
	WPEToplevelClass *toplevel_class = WPE_TOPLEVEL_CLASS(klass);
	toplevel_class->resize = viewport_shim_toplevel_resize;
}

static void viewport_shim_toplevel_init(ViewportShimToplevel *self)
{
}

/* -------------------------------------------------------------------------
 * View
 *
 * One view backs the whole shell, and render_buffer is the hot path.
 * ---------------------------------------------------------------------- */

#define VIEWPORT_TYPE_SHIM_VIEW (viewport_shim_view_get_type())
G_DECLARE_FINAL_TYPE(ViewportShimView, viewport_shim_view,
	VIEWPORT, SHIM_VIEW, WPEView)

struct _ViewportShimView {
	WPEView parent;
	ViewportShimCallbacks callbacks;
};

G_DEFINE_FINAL_TYPE(ViewportShimView, viewport_shim_view, WPE_TYPE_VIEW)

static gboolean viewport_shim_view_render_buffer(WPEView *view,
	WPEBuffer *buffer, const WPERectangle *damage, guint n_damage,
	GError **error)
{
	ViewportShimView *self = VIEWPORT_SHIM_VIEW(view);

	if (self->callbacks.render_frame == NULL) {
		/* Frames can arrive before anything is listening. Dropping them is
		 * correct; failing the frame would make WebKit give up. */
		wpe_view_buffer_rendered(view, buffer);
		return TRUE;
	}

	if (!WPE_IS_BUFFER_DMA_BUF(buffer)) {
		/* The display advertises a DRM device, so WebKit should always
		 * allocate dma-bufs. A shared-memory buffer here means that
		 * negotiation went wrong, and copying it would hide the problem. */
		g_set_error_literal(error, WPE_VIEW_ERROR, WPE_VIEW_ERROR_RENDER_FAILED,
			"WebKit produced a buffer that is not a dma-buf");
		return FALSE;
	}

	WPEBufferDMABuf *dmabuf = WPE_BUFFER_DMA_BUF(buffer);
	ViewportShimFrame frame = {
		.width = (uint32_t)wpe_buffer_get_width(buffer),
		.height = (uint32_t)wpe_buffer_get_height(buffer),
		.format = wpe_buffer_dma_buf_get_format(dmabuf),
		.modifier = wpe_buffer_dma_buf_get_modifier(dmabuf),
		.n_planes = wpe_buffer_dma_buf_get_n_planes(dmabuf),
		.fence_fd = -1,
		.token = buffer,
	};

	if (frame.n_planes > 4) {
		g_set_error(error, WPE_VIEW_ERROR, WPE_VIEW_ERROR_RENDER_FAILED,
			"a buffer with %u planes, which cannot be represented",
			frame.n_planes);
		return FALSE;
	}

	for (uint32_t i = 0; i < frame.n_planes; i++) {
		frame.fds[i] = wpe_buffer_dma_buf_get_fd(dmabuf, i);
		frame.offsets[i] = wpe_buffer_dma_buf_get_offset(dmabuf, i);
		frame.strides[i] = wpe_buffer_dma_buf_get_stride(dmabuf, i);
	}

	/* WebKit's GPU work may still be in flight. The fence is borrowed for the
	 * duration of the call, like the plane fds. */
	/* On the base buffer rather than the dma-buf subclass. */
	frame.fence_fd = wpe_buffer_get_rendering_fence(buffer);

	if (!self->callbacks.render_frame(self->callbacks.user, &frame)) {
		g_set_error_literal(error, WPE_VIEW_ERROR, WPE_VIEW_ERROR_RENDER_FAILED,
			"the compositor refused the frame");
		return FALSE;
	}

	/* Deliberately not acknowledged here. The caller does that once the frame
	 * has actually been presented, which is what ties the shell's paint rate
	 * to vblank rather than to how fast WebKit can draw. */
	return TRUE;
}

static void viewport_shim_view_class_init(ViewportShimViewClass *klass)
{
	WPEViewClass *view_class = WPE_VIEW_CLASS(klass);
	view_class->render_buffer = viewport_shim_view_render_buffer;
}

static void viewport_shim_view_init(ViewportShimView *self)
{
}

/* -------------------------------------------------------------------------
 * Display
 * ---------------------------------------------------------------------- */

#define VIEWPORT_TYPE_SHIM_DISPLAY (viewport_shim_display_get_type())
G_DECLARE_FINAL_TYPE(ViewportShimDisplay_, viewport_shim_display,
	VIEWPORT, SHIM_DISPLAY, WPEDisplay)

struct _ViewportShimDisplay_ {
	WPEDisplay parent;
	WPEDRMDevice *drm_device;
	WPEBufferFormats *formats;
	ViewportShimCallbacks callbacks;
	/* The view this display minted. WPE offers no way to ask a buffer which
	 * view produced it, and acknowledging a frame needs one. */
	WPEView *view;
};

G_DEFINE_FINAL_TYPE(ViewportShimDisplay_, viewport_shim_display,
	WPE_TYPE_DISPLAY)

static gboolean viewport_shim_display_connect(WPEDisplay *display,
	GError **error)
{
	ViewportShimDisplay_ *self = VIEWPORT_SHIM_DISPLAY(display);
	if (self->drm_device == NULL) {
		g_set_error_literal(error, WPE_DISPLAY_ERROR,
			WPE_DISPLAY_ERROR_CONNECTION_FAILED,
			"no DRM render node was given to the display");
		return FALSE;
	}
	return TRUE;
}

static WPEView *viewport_shim_display_create_view(WPEDisplay *display)
{
	ViewportShimDisplay_ *self = VIEWPORT_SHIM_DISPLAY(display);
	ViewportShimView *view = g_object_new(VIEWPORT_TYPE_SHIM_VIEW,
		"display", display, NULL);
	view->callbacks = self->callbacks;
	/* Borrowed: the view is owned by WPE, and outlives nothing this cares
	 * about. Only the most recent one is kept — the shell is a single view. */
	self->view = WPE_VIEW(view);
	return WPE_VIEW(view);
}

static WPEToplevel *viewport_shim_display_create_toplevel(WPEDisplay *display,
	guint max_views)
{
	return WPE_TOPLEVEL(g_object_new(VIEWPORT_TYPE_SHIM_TOPLEVEL,
		"display", display, NULL));
}

static WPEDRMDevice *viewport_shim_display_get_drm_device(WPEDisplay *display)
{
	return VIEWPORT_SHIM_DISPLAY(display)->drm_device;
}

static WPEBufferFormats *viewport_shim_display_get_preferred_buffer_formats(
	WPEDisplay *display)
{
	/* Transfer-full: WPE wraps the result in adoptGRef(), so returning the
	 * cached pointer bare while also releasing it in dispose unrefs it twice.
	 * Hand back a fresh reference and keep ours. */
	ViewportShimDisplay_ *self = VIEWPORT_SHIM_DISPLAY(display);
	return self->formats != NULL ? g_object_ref(self->formats) : NULL;
}

static void viewport_shim_display_dispose(GObject *object)
{
	ViewportShimDisplay_ *self = VIEWPORT_SHIM_DISPLAY(object);
	g_clear_object(&self->formats);
	g_clear_pointer(&self->drm_device, wpe_drm_device_unref);
	G_OBJECT_CLASS(viewport_shim_display_parent_class)->dispose(object);
}

static void viewport_shim_display_class_init(ViewportShimDisplay_Class *klass)
{
	GObjectClass *object_class = G_OBJECT_CLASS(klass);
	object_class->dispose = viewport_shim_display_dispose;

	WPEDisplayClass *display_class = WPE_DISPLAY_CLASS(klass);
	display_class->connect = viewport_shim_display_connect;
	display_class->create_view = viewport_shim_display_create_view;
	display_class->create_toplevel = viewport_shim_display_create_toplevel;
	display_class->get_drm_device = viewport_shim_display_get_drm_device;
	display_class->get_preferred_buffer_formats =
		viewport_shim_display_get_preferred_buffer_formats;
}

static void viewport_shim_display_init(ViewportShimDisplay_ *self)
{
}

/* -------------------------------------------------------------------------
 * The C API Rust calls
 * ---------------------------------------------------------------------- */

struct _ViewportShimDisplay {
	ViewportShimDisplay_ *display;
};

ViewportShimDisplay *viewport_shim_display_new(
	const ViewportShimDisplayConfig *config,
	const ViewportShimCallbacks *callbacks,
	char **error_out)
{
	if (config == NULL || config->render_node == NULL
			|| config->primary_node == NULL) {
		if (error_out != NULL) {
			*error_out = g_strdup(
				"both a primary and a render node are required");
		}
		return NULL;
	}

	ViewportShimDisplay_ *display = g_object_new(VIEWPORT_TYPE_SHIM_DISPLAY,
		NULL);
	display->drm_device = wpe_drm_device_new(config->primary_node,
		config->render_node);
	if (callbacks != NULL) {
		display->callbacks = *callbacks;
	}

	if (config->n_formats > 0 && config->format_codes != NULL
			&& config->format_modifiers != NULL) {
		/* One group, rendering usage: WebKit picks its allocation from what
		 * it is offered, and offering something the importer cannot take is
		 * how a shell ends up composited every frame. */
		WPEBufferFormatsBuilder *builder =
			wpe_buffer_formats_builder_new(display->drm_device);
		wpe_buffer_formats_builder_append_group(builder,
			display->drm_device, WPE_BUFFER_FORMAT_USAGE_RENDERING);
		for (uint32_t i = 0; i < config->n_formats; i++) {
			wpe_buffer_formats_builder_append_format(builder,
				config->format_codes[i], config->format_modifiers[i]);
		}
		display->formats = wpe_buffer_formats_builder_end(builder);
	}

	ViewportShimDisplay *handle = g_new0(ViewportShimDisplay, 1);
	handle->display = display;
	return handle;
}

void viewport_shim_display_free(ViewportShimDisplay *display)
{
	if (display == NULL) {
		return;
	}
	g_clear_object(&display->display);
	g_free(display);
}

void *viewport_shim_display_handle(ViewportShimDisplay *display)
{
	return display != NULL ? display->display : NULL;
}

void viewport_shim_frame_done(ViewportShimDisplay *display, void *token)
{
	if (display == NULL || display->display == NULL || token == NULL) {
		return;
	}
	WPEView *view = display->display->view;
	if (view != NULL) {
		wpe_view_buffer_rendered(view, WPE_BUFFER(token));
	}
}

void viewport_shim_display_resize(ViewportShimDisplay *display,
	uint32_t width, uint32_t height)
{
	if (display == NULL || display->display == NULL) {
		return;
	}
	WPEView *view = display->display->view;
	if (view != NULL) {
		wpe_view_resized(view, (int)width, (int)height);
	}
}

void viewport_shim_display_show(ViewportShimDisplay *display)
{
	if (display == NULL || display->display == NULL) {
		return;
	}
	WPEView *view = display->display->view;
	if (view == NULL) {
		return;
	}
	/* Both are required before WebKit paints: an unmapped view produces no
	 * frames at all, and an unfocused one behaves as though the desktop is in
	 * the background. */
	wpe_view_map(view);
	wpe_view_focus_in(view);
}

/* -------------------------------------------------------------------------
 * Input
 *
 * Ported from src/web.c of the C compositor. The shell is a page, and a page
 * with no input is a page whose every button is decoration: the taskbar, the
 * notification actions, the overview's drag-and-drop and the screen-share
 * chooser are all ordinary DOM handlers waiting for events that have to come
 * from here.
 *
 * Coordinates are the layout's own, which is also the page's: the shell is one
 * document spanning every monitor, so a point on the second screen is a point
 * near the right-hand edge of the page and needs no translation.
 * ---------------------------------------------------------------------- */

static WPEView *viewport_shim_view(ViewportShimDisplay *display)
{
	if (display == NULL || display->display == NULL) {
		return NULL;
	}
	return display->display->view;
}

void viewport_shim_pointer_motion(ViewportShimDisplay *display,
	uint32_t time_msec, double x, double y, uint32_t modifiers)
{
	WPEView *view = viewport_shim_view(display);
	if (view == NULL) {
		return;
	}

	/* Negative coordinates mean the pointer moved onto a client window.
	 * WebKit needs the leave or a :hover state sticks — a button left
	 * highlighted under a window the pointer has moved on to. */
	WPEEventType type = (x < 0 || y < 0)
		? WPE_EVENT_POINTER_LEAVE : WPE_EVENT_POINTER_MOVE;

	WPEEvent *event = wpe_event_pointer_move_new(type, view,
		WPE_INPUT_SOURCE_MOUSE, time_msec, modifiers, x, y, 0, 0);
	wpe_view_event(view, event);
	wpe_event_unref(event);
}

void viewport_shim_pointer_button(ViewportShimDisplay *display,
	uint32_t time_msec, double x, double y, uint32_t button, bool pressed,
	uint32_t modifiers)
{
	WPEView *view = viewport_shim_view(display);
	if (view == NULL) {
		return;
	}

	/* evdev BTN_LEFT/RIGHT/MIDDLE are 0x110-0x112; WPE numbers buttons from
	 * one, in the order left, middle, right. */
	guint wpe_button;
	switch (button) {
	case 0x110: wpe_button = 1; break;
	case 0x112: wpe_button = 2; break;
	case 0x111: wpe_button = 3; break;
	default: wpe_button = button - 0x10f; break;
	}

	WPEEvent *event = wpe_event_pointer_button_new(
		pressed ? WPE_EVENT_POINTER_DOWN : WPE_EVENT_POINTER_UP,
		view, WPE_INPUT_SOURCE_MOUSE, time_msec, modifiers, wpe_button,
		x, y, pressed ? 1 : 0);
	wpe_view_event(view, event);
	wpe_event_unref(event);
}

void viewport_shim_pointer_axis(ViewportShimDisplay *display,
	uint32_t time_msec, double x, double y, double dx, double dy,
	bool precise, uint32_t modifiers)
{
	WPEView *view = viewport_shim_view(display);
	if (view == NULL) {
		return;
	}

	/* Negated: Wayland reports the direction the surface moves and WPE wants
	 * the direction the content does. */
	WPEEvent *event = wpe_event_scroll_new(view, WPE_INPUT_SOURCE_MOUSE,
		time_msec, modifiers, -dx, -dy, precise, FALSE, x, y);
	wpe_view_event(view, event);
	wpe_event_unref(event);
}

void viewport_shim_keyboard_key(ViewportShimDisplay *display,
	uint32_t time_msec, uint32_t keycode, uint32_t keysym, bool pressed,
	uint32_t modifiers)
{
	WPEView *view = viewport_shim_view(display);
	if (view == NULL) {
		return;
	}

	WPEEvent *event = wpe_event_keyboard_new(
		pressed ? WPE_EVENT_KEYBOARD_KEY_DOWN : WPE_EVENT_KEYBOARD_KEY_UP,
		view, WPE_INPUT_SOURCE_KEYBOARD, time_msec, modifiers, keycode,
		keysym);
	wpe_view_event(view, event);
	wpe_event_unref(event);
}

void viewport_shim_string_free(char *string)
{
	g_free(string);
}
