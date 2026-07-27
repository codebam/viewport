/* SPDX-License-Identifier: MIT
 *
 * Graphics tablets.
 *
 * A tablet is not a mouse, and treating it as one loses most of what it is for.
 * The stylus reports pressure, tilt and distance; it knows when it is hovering
 * rather than touching; it has its own buttons and often an eraser at the other
 * end. Drawing applications read all of that, and a compositor that forwards
 * only x and y turns a pressure-sensitive brush into a pen that draws one width
 * of line.
 *
 * So tablet-v2 is spoken properly, with one caveat that is also what makes this
 * usable rather than pedantic: not every application supports it. A tablet that
 * only ever spoke tablet-v2 would be inert in every program that expects a
 * pointer. The stylus therefore drives the cursor as well, so it works
 * everywhere, and the richer protocol is offered on top for the programs that
 * can use it.
 *
 * Tablets are absolute devices: the stylus points at a place on the tablet
 * surface, which maps to a place on the screen. That is why the cursor jumps to
 * where the pen lands rather than moving relative to where it was.
 */
#define _POSIX_C_SOURCE 200809L

#include <linux/input-event-codes.h>
#include <math.h>
#include <stdlib.h>

#include <wlr/types/wlr_cursor.h>
#include <wlr/types/wlr_seat.h>
#include <wlr/types/wlr_tablet_pad.h>
#include <wlr/types/wlr_tablet_tool.h>
#include <wlr/types/wlr_tablet_v2.h>
#include <wlr/util/log.h>

#include "viewport-view.h"
#include "viewport-input.h"

/* The stylus currently in proximity, and the tablet it belongs to. A tool is
 * created lazily, because a tablet may be plugged in long before a pen is
 * brought near it, and the pen's identity is only known then. */
struct viewport_tablet_tool {
	struct viewport_server *server;
	struct wlr_tablet_v2_tablet_tool *tool_v2;
	struct wlr_tablet_tool *tool;

	struct wl_listener destroy;
};

static struct viewport_tablet_tool *tool_for(struct viewport_server *server,
	struct wlr_tablet_tool *tool, struct wlr_tablet_v2_tablet *tablet_v2);

/* Where the stylus is pointing, in layout coordinates, and what is under it. */
static struct wlr_surface *tablet_surface_at(struct viewport_server *server,
	double *sx, double *sy, struct viewport_toplevel **toplevel_out)
{
	return viewport_surface_at(server, server->cursor->x, server->cursor->y,
		sx, sy, toplevel_out);
}

static void handle_tool_destroy(struct wl_listener *listener, void *data)
{
	struct viewport_tablet_tool *tool =
		wl_container_of(listener, tool, destroy);

	wl_list_remove(&tool->destroy.link);
	if (tool->server->tablet_tool == tool) {
		tool->server->tablet_tool = NULL;
	}
	free(tool);
}

static struct viewport_tablet_tool *tool_for(struct viewport_server *server,
	struct wlr_tablet_tool *tool, struct wlr_tablet_v2_tablet *tablet_v2)
{
	if (tool->data != NULL) {
		return tool->data;
	}

	struct viewport_tablet_tool *own = calloc(1, sizeof(*own));
	if (own == NULL) {
		return NULL;
	}
	own->server = server;
	own->tool = tool;
	own->tool_v2 = wlr_tablet_tool_create(server->tablet_manager, server->seat,
		tool);
	if (own->tool_v2 == NULL) {
		free(own);
		return NULL;
	}

	own->destroy.notify = handle_tool_destroy;
	wl_signal_add(&tool->events.destroy, &own->destroy);

	tool->data = own;
	server->tablet_tool = own;
	return own;
}

static void handle_tablet_axis(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, tablet_axis);
	struct wlr_tablet_tool_axis_event *event = data;

	viewport_idle_activity(server);

	/* The cursor follows the pen, so every program sees something sensible
	 * whether or not it speaks tablet-v2. Only the axes the event actually
	 * carries are applied: a stylus that reports x without y would otherwise
	 * have its y silently reset to zero. */
	if ((event->updated_axes & (WLR_TABLET_TOOL_AXIS_X | WLR_TABLET_TOOL_AXIS_Y))
			!= 0) {
		double x = (event->updated_axes & WLR_TABLET_TOOL_AXIS_X)
			? event->x : NAN;
		double y = (event->updated_axes & WLR_TABLET_TOOL_AXIS_Y)
			? event->y : NAN;
		wlr_cursor_warp_absolute(server->cursor, &event->tablet->base, x, y);
		viewport_cursor_refresh(server, event->time_msec);
	}

	struct viewport_tablet_tool *tool = server->tablet_tool;
	if (tool == NULL) {
		return;
	}

	double sx, sy;
	struct wlr_surface *surface = tablet_surface_at(server, &sx, &sy, NULL);
	if (surface != NULL && !server->locked) {
		wlr_tablet_v2_tablet_tool_notify_motion(tool->tool_v2, sx, sy);

		/* Everything a drawing program actually wants. Sent only when the event
		 * says it changed, because a tablet reports what moved and inventing the
		 * rest would be noise. */
		if (event->updated_axes & WLR_TABLET_TOOL_AXIS_PRESSURE) {
			wlr_tablet_v2_tablet_tool_notify_pressure(tool->tool_v2,
				event->pressure);
		}
		if (event->updated_axes & WLR_TABLET_TOOL_AXIS_DISTANCE) {
			wlr_tablet_v2_tablet_tool_notify_distance(tool->tool_v2,
				event->distance);
		}
		if (event->updated_axes &
				(WLR_TABLET_TOOL_AXIS_TILT_X | WLR_TABLET_TOOL_AXIS_TILT_Y)) {
			wlr_tablet_v2_tablet_tool_notify_tilt(tool->tool_v2, event->tilt_x,
				event->tilt_y);
		}
		if (event->updated_axes & WLR_TABLET_TOOL_AXIS_ROTATION) {
			wlr_tablet_v2_tablet_tool_notify_rotation(tool->tool_v2,
				event->rotation);
		}
		if (event->updated_axes & WLR_TABLET_TOOL_AXIS_SLIDER) {
			wlr_tablet_v2_tablet_tool_notify_slider(tool->tool_v2, event->slider);
		}
		if (event->updated_axes & WLR_TABLET_TOOL_AXIS_WHEEL) {
			wlr_tablet_v2_tablet_tool_notify_wheel(tool->tool_v2, event->wheel_delta,
				0);
		}
	}
}

static void handle_tablet_proximity(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, tablet_proximity);
	struct wlr_tablet_tool_proximity_event *event = data;

	struct wlr_tablet_v2_tablet *tablet_v2 = event->tablet->base.data;
	if (tablet_v2 == NULL) {
		return;
	}

	struct viewport_tablet_tool *tool = tool_for(server, event->tool,
		tablet_v2);
	if (tool == NULL) {
		return;
	}

	if (event->state == WLR_TABLET_TOOL_PROXIMITY_OUT) {
		wlr_tablet_v2_tablet_tool_notify_proximity_out(tool->tool_v2);
		return;
	}

	viewport_idle_activity(server);
	wlr_cursor_warp_absolute(server->cursor, &event->tablet->base, event->x,
		event->y);
	viewport_cursor_refresh(server, event->time_msec);

	double sx, sy;
	struct wlr_surface *surface = tablet_surface_at(server, &sx, &sy, NULL);
	if (surface != NULL) {
		wlr_tablet_v2_tablet_tool_notify_proximity_in(tool->tool_v2, tablet_v2,
			surface);
		wlr_tablet_v2_tablet_tool_notify_motion(tool->tool_v2, sx, sy);
	}
}

static void handle_tablet_tip(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, tablet_tip);
	struct wlr_tablet_tool_tip_event *event = data;

	viewport_idle_activity(server);

	struct viewport_tablet_tool *tool = server->tablet_tool;
	double sx, sy;
	struct viewport_toplevel *toplevel = NULL;
	struct wlr_surface *surface = tablet_surface_at(server, &sx, &sy, &toplevel);

	/* Touching the tablet focuses what is under the pen, the same as clicking
	 * would — otherwise drawing in a window would require clicking it with a
	 * mouse first. */
	if (event->state == WLR_TABLET_TOOL_TIP_DOWN && toplevel != NULL &&
			server->focused != toplevel) {
		viewport_toplevel_focus(toplevel);
	}

	if (tool != NULL && surface != NULL) {
		if (event->state == WLR_TABLET_TOOL_TIP_DOWN) {
			wlr_send_tablet_v2_tablet_tool_down(tool->tool_v2);
		} else {
			wlr_send_tablet_v2_tablet_tool_up(tool->tool_v2);
		}
		return;
	}

	/* Nothing under the pen that speaks tablet-v2, or the shell: fall back to a
	 * left click so the stylus still works as a pointer. */
	viewport_pointer_button(server, event->time_msec, BTN_LEFT,
		event->state == WLR_TABLET_TOOL_TIP_DOWN);
}

static void handle_tablet_button(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, tablet_button);
	struct wlr_tablet_tool_button_event *event = data;

	viewport_idle_activity(server);

	struct viewport_tablet_tool *tool = server->tablet_tool;
	if (tool == NULL) {
		return;
	}

	wlr_tablet_v2_tablet_tool_notify_button(tool->tool_v2, event->button,
		event->state == WLR_BUTTON_PRESSED
			? ZWP_TABLET_PAD_V2_BUTTON_STATE_PRESSED
			: ZWP_TABLET_PAD_V2_BUTTON_STATE_RELEASED);
}

void viewport_tablet_add(struct viewport_server *server,
	struct wlr_input_device *device)
{
	if (server->tablet_manager == NULL) {
		return;
	}

	if (device->type == WLR_INPUT_DEVICE_TABLET) {
		struct wlr_tablet *tablet = wlr_tablet_from_input_device(device);
		struct wlr_tablet_v2_tablet *tablet_v2 = wlr_tablet_create(
			server->tablet_manager, server->seat, device);
		device->data = tablet_v2;

		/* Attached to the cursor as well, which is what maps the tablet's
		 * surface onto the screen and what makes the stylus work in programs
		 * that only understand a pointer. */
		wlr_cursor_attach_input_device(server->cursor, device);
		wlr_log(WLR_INFO, "tablet %s", tablet->base.name);
		return;
	}

	if (device->type == WLR_INPUT_DEVICE_TABLET_PAD) {
		/* The buttons and rings on the tablet itself. Published so a client can
		 * bind them; nothing here interprets them, because what they should do
		 * is the application's business and there is no sensible default. */
		wlr_tablet_pad_create(server->tablet_manager, server->seat, device);
		wlr_log(WLR_INFO, "tablet pad %s", device->name);
	}
}

void viewport_tablet_init(struct viewport_server *server)
{
	server->tablet_manager = wlr_tablet_v2_create(server->wl_display);
	if (server->tablet_manager == NULL) {
		wlr_log(WLR_ERROR, "tablet-v2 unavailable; styluses act as pointers");
		return;
	}

	server->tablet_axis.notify = handle_tablet_axis;
	wl_signal_add(&server->cursor->events.tablet_tool_axis,
		&server->tablet_axis);
	server->tablet_proximity.notify = handle_tablet_proximity;
	wl_signal_add(&server->cursor->events.tablet_tool_proximity,
		&server->tablet_proximity);
	server->tablet_tip.notify = handle_tablet_tip;
	wl_signal_add(&server->cursor->events.tablet_tool_tip, &server->tablet_tip);
	server->tablet_button.notify = handle_tablet_button;
	wl_signal_add(&server->cursor->events.tablet_tool_button,
		&server->tablet_button);
}
