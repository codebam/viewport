/* SPDX-License-Identifier: MIT
 *
 * Touchpad gestures.
 *
 * Two audiences want these events and only one can have them at a time. A
 * client — a browser pinching to zoom, a map panning — binds
 * pointer-gestures-v1 and expects them forwarded. The compositor wants some of
 * them for itself: a three-finger swipe scrolls the strip, which is the motion
 * the scrolling layout is built around.
 *
 * The split is by finger count. Three fingers are the compositor's, because no
 * toolkit binds them for anything; everything else is forwarded untouched. A
 * gesture that starts as the compositor's stays that way until it ends, so a
 * finger lifted mid-swipe cannot hand a half-finished gesture to a client.
 *
 * The scroll itself is live rather than a flick that resolves at the end.
 * Content that follows your fingers is most of why a touchpad feels good, and
 * the shell already knows how to render the strip at an arbitrary offset.
 */
#define _POSIX_C_SOURCE 200809L

#include <math.h>
#include <stdio.h>
#include <stdlib.h>

#include <wlr/types/wlr_cursor.h>
#include <wlr/types/wlr_pointer.h>
#include <wlr/types/wlr_pointer_gestures_v1.h>
#include <wlr/util/log.h>

#include "viewport.h"

/* The count the compositor claims. Two is pinch-to-zoom territory and four is
 * rare enough that clients which do use it should keep it. */
#define GESTURE_FINGERS 3

/* How far a vertical three-finger swipe must travel to count as a workspace
 * change. Horizontal has no threshold — it scrolls continuously. */
#define SWIPE_WORKSPACE_PX 120.0

static void handle_swipe_begin(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, swipe_begin);
	struct wlr_pointer_swipe_begin_event *event = data;

	if (event->fingers == GESTURE_FINGERS) {
		server->gesture_active = true;
		server->gesture_dx = 0.0;
		server->gesture_dy = 0.0;
		return;
	}

	wlr_pointer_gestures_v1_send_swipe_begin(server->pointer_gestures,
		server->seat, event->time_msec, event->fingers);
}

static void handle_swipe_update(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, swipe_update);
	struct wlr_pointer_swipe_update_event *event = data;

	if (!server->gesture_active) {
		wlr_pointer_gestures_v1_send_swipe_update(server->pointer_gestures,
			server->seat, event->time_msec, event->dx, event->dy);
		return;
	}

	server->gesture_dx += event->dx;
	server->gesture_dy += event->dy;

	/* Horizontal moves the strip under the fingers. Sent as a delta rather
	 * than an absolute offset so the shell stays the one deciding where the
	 * limits are. */
	char command[96];
	snprintf(command, sizeof(command), "gesture.scroll %d",
		(int)(-event->dx));
	viewport_ipc_notify_shell_command(server, command);
}

static void handle_swipe_end(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, swipe_end);
	struct wlr_pointer_swipe_end_event *event = data;

	if (!server->gesture_active) {
		wlr_pointer_gestures_v1_send_swipe_end(server->pointer_gestures,
			server->seat, event->time_msec, event->cancelled);
		return;
	}
	server->gesture_active = false;

	/* A mostly-vertical swipe was never a scroll: it changes workspace, once,
	 * on release. Comparing against the horizontal travel keeps a slightly
	 * crooked horizontal swipe from being read as one. */
	if (!event->cancelled &&
			fabs(server->gesture_dy) > SWIPE_WORKSPACE_PX &&
			fabs(server->gesture_dy) > fabs(server->gesture_dx)) {
		viewport_ipc_notify_shell_command(server,
			server->gesture_dy < 0 ? "workspace.step 1" : "workspace.step -1");
		return;
	}

	/* Otherwise the strip is left wherever the fingers put it, and the shell
	 * settles on whichever column that turned out to be. */
	viewport_ipc_notify_shell_command(server, "gesture.settle");
}

static void handle_pinch_begin(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, pinch_begin);
	struct wlr_pointer_pinch_begin_event *event = data;

	wlr_pointer_gestures_v1_send_pinch_begin(server->pointer_gestures,
		server->seat, event->time_msec, event->fingers);
}

static void handle_pinch_update(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, pinch_update);
	struct wlr_pointer_pinch_update_event *event = data;

	wlr_pointer_gestures_v1_send_pinch_update(server->pointer_gestures,
		server->seat, event->time_msec, event->dx, event->dy, event->scale,
		event->rotation);
}

static void handle_pinch_end(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, pinch_end);
	struct wlr_pointer_pinch_end_event *event = data;

	wlr_pointer_gestures_v1_send_pinch_end(server->pointer_gestures,
		server->seat, event->time_msec, event->cancelled);
}

static void handle_hold_begin(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, hold_begin);
	struct wlr_pointer_hold_begin_event *event = data;

	wlr_pointer_gestures_v1_send_hold_begin(server->pointer_gestures,
		server->seat, event->time_msec, event->fingers);
}

static void handle_hold_end(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, hold_end);
	struct wlr_pointer_hold_end_event *event = data;

	wlr_pointer_gestures_v1_send_hold_end(server->pointer_gestures,
		server->seat, event->time_msec, event->cancelled);
}

void viewport_gestures_init(struct viewport_server *server)
{
	server->pointer_gestures =
		wlr_pointer_gestures_v1_create(server->wl_display);
	if (server->pointer_gestures == NULL) {
		wlr_log(WLR_ERROR, "pointer gestures unavailable");
		return;
	}

	server->swipe_begin.notify = handle_swipe_begin;
	wl_signal_add(&server->cursor->events.swipe_begin, &server->swipe_begin);
	server->swipe_update.notify = handle_swipe_update;
	wl_signal_add(&server->cursor->events.swipe_update, &server->swipe_update);
	server->swipe_end.notify = handle_swipe_end;
	wl_signal_add(&server->cursor->events.swipe_end, &server->swipe_end);

	server->pinch_begin.notify = handle_pinch_begin;
	wl_signal_add(&server->cursor->events.pinch_begin, &server->pinch_begin);
	server->pinch_update.notify = handle_pinch_update;
	wl_signal_add(&server->cursor->events.pinch_update, &server->pinch_update);
	server->pinch_end.notify = handle_pinch_end;
	wl_signal_add(&server->cursor->events.pinch_end, &server->pinch_end);

	server->hold_begin.notify = handle_hold_begin;
	wl_signal_add(&server->cursor->events.hold_begin, &server->hold_begin);
	server->hold_end.notify = handle_hold_end;
	wl_signal_add(&server->cursor->events.hold_end, &server->hold_end);
}
