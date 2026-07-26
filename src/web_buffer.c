/* SPDX-License-Identifier: MIT
 *
 * WPEBufferDMABuf -> wlr_buffer.
 *
 * This is the join where the browser's output becomes a compositor texture,
 * and it is deliberately boring: a struct copy. WebKit allocated the frame on
 * the DRM render node we advertised, so the fds it hands back are already
 * importable by our Vulkan renderer. Nothing is mapped, read back, or blitted
 * — the compositor samples the same VRAM the web engine painted into.
 *
 * Lifetime is the only thing worth care. The fds belong to the WPEBuffer, not
 * to us, so we take a GObject reference for as long as the wlr_buffer lives
 * and never close them ourselves. When the scene finally drops the last
 * reference, we tell WebKit the frame is reusable.
 */
#define _POSIX_C_SOURCE 200809L

#include <stdlib.h>

#include <wlr/interfaces/wlr_buffer.h>
#include <wlr/render/dmabuf.h>
#include <wlr/util/log.h>

#include "web-internal.h"

struct viewport_web_buffer {
	struct wlr_buffer base;
	/* NULL once viewport_web is gone; see viewport_web.buffers. */
	struct viewport_web *web;
	struct wl_list link;
	WPEBuffer *wpe_buffer;
	struct wlr_dmabuf_attributes dmabuf;
};

static const struct wlr_buffer_impl buffer_impl;

static struct viewport_web_buffer *web_buffer_from_buffer(
	struct wlr_buffer *buffer)
{
	if (buffer->impl != &buffer_impl) {
		return NULL;
	}
	return (struct viewport_web_buffer *)buffer;
}

static void buffer_destroy(struct wlr_buffer *wlr_buffer)
{
	struct viewport_web_buffer *buffer = web_buffer_from_buffer(wlr_buffer);
	if (buffer == NULL) {
		return;
	}

	/* The compositor is done sampling this frame. Handing it back lets WebKit
	 * recycle it instead of allocating a fresh dma-buf every frame.
	 *
	 * `web` is NULL when this buffer outlived the engine — which happens on
	 * every DRM shutdown, because the backend holds scanout framebuffers and
	 * releases them from wlr_backend_destroy(), long after viewport_web_destroy()
	 * freed the struct. Testing buffer->web->wpe_view without checking `web`
	 * first reads freed memory and segfaults. */
	if (buffer->web != NULL) {
		wl_list_remove(&buffer->link);
		if (buffer->web->wpe_view != NULL) {
			wpe_view_buffer_released(buffer->web->wpe_view,
				buffer->wpe_buffer);
		}
	}

	/* Do NOT run wlr_dmabuf_attributes_finish() here: those fds are owned by
	 * the WPEBuffer and closing them would leave WebKit with a dangling
	 * handle. Dropping the reference is the whole cleanup. */
	g_clear_object(&buffer->wpe_buffer);
	free(buffer);
}

static bool buffer_get_dmabuf(struct wlr_buffer *wlr_buffer,
	struct wlr_dmabuf_attributes *attribs)
{
	struct viewport_web_buffer *buffer = web_buffer_from_buffer(wlr_buffer);
	if (buffer == NULL) {
		return false;
	}
	*attribs = buffer->dmabuf;
	return true;
}

static bool buffer_begin_data_ptr_access(struct wlr_buffer *wlr_buffer,
	uint32_t flags, void **data, uint32_t *format, size_t *stride)
{
	/* Refusing this is the point. Any caller that wants a CPU pointer — a
	 * screenshotter, a software cursor fallback — has to go the long way
	 * round instead of silently stalling the pipeline on a readback. */
	return false;
}

static void buffer_end_data_ptr_access(struct wlr_buffer *wlr_buffer)
{
	/* Unreachable: wlroots only calls this after a successful begin, and ours
	 * always fails. It must still be present — wlr_buffer_init() asserts that
	 * the two are supplied as a pair, and omitting this one aborts the
	 * compositor the first time WebKit hands us a frame. */
}

static const struct wlr_buffer_impl buffer_impl = {
	.destroy = buffer_destroy,
	.get_dmabuf = buffer_get_dmabuf,
	.begin_data_ptr_access = buffer_begin_data_ptr_access,
	.end_data_ptr_access = buffer_end_data_ptr_access,
};

void viewport_web_buffers_detach(struct viewport_web *web)
{
	struct viewport_web_buffer *buffer, *tmp;
	wl_list_for_each_safe(buffer, tmp, &web->buffers, link) {
		buffer->web = NULL;
		wl_list_remove(&buffer->link);
		wl_list_init(&buffer->link);
	}
}

struct wlr_buffer *viewport_web_buffer_from_wpe(struct viewport_web *web,
	WPEBuffer *wpe_buffer)
{
	if (!WPE_IS_BUFFER_DMA_BUF(wpe_buffer)) {
		/* WebKit fell back to shared memory — typically because the DRM
		 * device we advertised could not be opened, or no format was agreed.
		 * Compositing that would mean a CPU copy per frame, which defeats the
		 * entire design, so refuse loudly rather than quietly crawl. */
		wlr_log(WLR_ERROR,
			"WPE produced a non-dma-buf frame; check the advertised render node");
		return NULL;
	}

	WPEBufferDMABuf *dmabuf = WPE_BUFFER_DMA_BUF(wpe_buffer);
	uint32_t n_planes = wpe_buffer_dma_buf_get_n_planes(dmabuf);
	if (n_planes > WLR_DMABUF_MAX_PLANES) {
		wlr_log(WLR_ERROR, "WPE frame has %u planes, max is %d", n_planes,
			WLR_DMABUF_MAX_PLANES);
		return NULL;
	}

	struct viewport_web_buffer *buffer = calloc(1, sizeof(*buffer));
	if (buffer == NULL) {
		return NULL;
	}

	buffer->web = web;
	buffer->wpe_buffer = g_object_ref(wpe_buffer);
	wl_list_insert(&web->buffers, &buffer->link);

	buffer->dmabuf = (struct wlr_dmabuf_attributes){
		.width = wpe_buffer_get_width(wpe_buffer),
		.height = wpe_buffer_get_height(wpe_buffer),
		.format = wpe_buffer_dma_buf_get_format(dmabuf),
		.modifier = wpe_buffer_dma_buf_get_modifier(dmabuf),
		.n_planes = n_planes,
	};

	for (uint32_t i = 0; i < n_planes; i++) {
		buffer->dmabuf.fd[i] = wpe_buffer_dma_buf_get_fd(dmabuf, i);
		buffer->dmabuf.offset[i] = wpe_buffer_dma_buf_get_offset(dmabuf, i);
		buffer->dmabuf.stride[i] = wpe_buffer_dma_buf_get_stride(dmabuf, i);
	}

	wlr_buffer_init(&buffer->base, &buffer_impl, buffer->dmabuf.width,
		buffer->dmabuf.height);

	return &buffer->base;
}
