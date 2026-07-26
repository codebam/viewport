/* SPDX-License-Identifier: MIT
 *
 * Marrying two event loops.
 *
 * WebKit is GLib all the way down — it needs a GMainContext spinning or the
 * network process, the timers and the compositing thread all stall. wlroots
 * has its own wl_event_loop. Only one of them can own the process.
 *
 * We make GLib the outer loop. wl_event_loop is backed by a single epoll fd
 * (`wl_event_loop_get_fd`), so it nests cleanly inside GLib as a GSource: poll
 * that fd, and dispatch the Wayland loop when it signals. The one subtlety is
 * that queued client output has to be flushed *before* we block in poll,
 * otherwise a client waiting on a reply and a compositor waiting on that
 * client's request deadlock. GSource's prepare phase runs at exactly that
 * point, so the flush goes there.
 *
 * The reverse arrangement — wl_event_loop outer, pumping GLib from a timer —
 * either busy-loops or adds latency to every WebKit timer. Don't.
 */
#define _POSIX_C_SOURCE 200809L

#include <glib.h>
#include <stdlib.h>

#include <wayland-server-core.h>
#include <wlr/util/log.h>

#include "viewport.h"

struct viewport_glib_loop {
	GMainLoop *main_loop;
	GMainContext *context;
	GSource *source;
	struct wl_display *display;
	struct wl_event_loop *event_loop;
};

/* GSource subclass. The first member must be the GSource itself. */
struct wayland_source {
	GSource base;
	gpointer fd_tag;
	struct wl_display *display;
	struct wl_event_loop *event_loop;
};

static gboolean wayland_source_prepare(GSource *source, gint *timeout)
{
	struct wayland_source *self = (struct wayland_source *)source;

	/* Run the Wayland loop's idle callbacks before blocking.
	 *
	 * This is the subtle part of nesting wl_event_loop inside another loop.
	 * wlroots defers work to wl_event_loop_add_idle() — notably
	 * wlr_xdg_surface_schedule_configure(), which is how a client is told to
	 * resize. Those callbacks only run inside wl_event_loop_dispatch(), and
	 * dispatch only happens here when the loop's epoll fd signals.
	 *
	 * Anything that schedules Wayland work from a *GLib* source therefore
	 * strands it. That is the normal path in this compositor: the shell's
	 * layout message arrives through WebKit's script handler, calls
	 * wlr_xdg_toplevel_set_size(), and the resulting configure then sits in
	 * the idle queue with no Wayland fd traffic to flush it. The window keeps
	 * its old size until some unrelated client activity or input happens to
	 * dispatch the loop — which looks exactly like "the window only appears
	 * after you move the mouse".
	 *
	 * Draining idles every iteration costs nothing when the queue is empty. */
	wl_event_loop_dispatch_idle(self->event_loop);

	/* Push pending events out to clients before we block. */
	wl_display_flush_clients(self->display);

	*timeout = -1;
	return FALSE;
}

static gboolean wayland_source_check(GSource *source)
{
	struct wayland_source *self = (struct wayland_source *)source;
	GIOCondition revents = g_source_query_unix_fd(source, self->fd_tag);
	return (revents & (G_IO_IN | G_IO_ERR | G_IO_HUP)) != 0;
}

static gboolean wayland_source_dispatch(GSource *source, GSourceFunc callback,
	gpointer user_data)
{
	struct wayland_source *self = (struct wayland_source *)source;
	GIOCondition revents = g_source_query_unix_fd(source, self->fd_tag);

	if (revents & (G_IO_ERR | G_IO_HUP)) {
		wlr_log(WLR_ERROR, "wayland event loop fd hung up");
		return G_SOURCE_REMOVE;
	}

	/* Zero timeout: the fd is already readable, so this drains without
	 * blocking and returns control to GLib for WebKit's sources. */
	if (wl_event_loop_dispatch(self->event_loop, 0) < 0) {
		wlr_log(WLR_ERROR, "wl_event_loop_dispatch failed");
		return G_SOURCE_REMOVE;
	}

	wl_event_loop_dispatch_idle(self->event_loop);
	wl_display_flush_clients(self->display);
	return G_SOURCE_CONTINUE;
}

static GSourceFuncs wayland_source_funcs = {
	.prepare = wayland_source_prepare,
	.check = wayland_source_check,
	.dispatch = wayland_source_dispatch,
	.finalize = NULL,
};

struct viewport_glib_loop *viewport_glib_loop_create(struct wl_display *display)
{
	struct viewport_glib_loop *loop = calloc(1, sizeof(*loop));
	if (loop == NULL) {
		return NULL;
	}

	loop->display = display;
	loop->event_loop = wl_display_get_event_loop(display);
	loop->context = g_main_context_default();
	loop->main_loop = g_main_loop_new(loop->context, FALSE);

	GSource *source = g_source_new(&wayland_source_funcs,
		sizeof(struct wayland_source));
	struct wayland_source *self = (struct wayland_source *)source;
	self->display = display;
	self->event_loop = loop->event_loop;
	self->fd_tag = g_source_add_unix_fd(source,
		wl_event_loop_get_fd(loop->event_loop), G_IO_IN | G_IO_ERR | G_IO_HUP);

	g_source_set_name(source, "wayland-event-loop");
	/* Ahead of WebKit's default-priority work: input latency beats layout. */
	g_source_set_priority(source, G_PRIORITY_DEFAULT - 10);
	g_source_attach(source, loop->context);

	loop->source = source;
	return loop;
}

void viewport_glib_loop_run(struct viewport_glib_loop *loop)
{
	g_main_loop_run(loop->main_loop);
}

void viewport_glib_loop_quit(struct viewport_glib_loop *loop)
{
	if (loop != NULL && loop->main_loop != NULL) {
		g_main_loop_quit(loop->main_loop);
	}
}

void viewport_glib_loop_destroy(struct viewport_glib_loop *loop)
{
	if (loop == NULL) {
		return;
	}
	if (loop->source != NULL) {
		g_source_destroy(loop->source);
		g_source_unref(loop->source);
	}
	if (loop->main_loop != NULL) {
		g_main_loop_unref(loop->main_loop);
	}
	free(loop);
}
