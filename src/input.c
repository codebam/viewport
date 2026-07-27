/* SPDX-License-Identifier: MIT
 *
 * Input routing.
 *
 * There is exactly one rule: whatever the scene graph says is topmost under
 * the cursor gets the event. Because the shell's buffer sits at the bottom
 * layer and client windows are stacked above it in the rects JS asked for,
 * `wlr_scene_node_at` already encodes "pointer over an app window" versus
 * "pointer over a titlebar, the dock, or the desktop" — no separate hit
 * geometry to keep in sync with the DOM, and nothing to get stale when the
 * shell animates a window.
 */
#define _POSIX_C_SOURCE 200809L

#include <linux/input-event-codes.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

#include <wlr/types/wlr_cursor_shape_v1.h>
#include <wlr/types/wlr_data_device.h>
#include <wlr/types/wlr_keyboard_shortcuts_inhibit_v1.h>
#include <wlr/types/wlr_primary_selection.h>
#include <wlr/types/wlr_pointer.h>
#include <wlr/types/wlr_scene.h>
#include <wlr/types/wlr_touch.h>
#include <wlr/util/log.h>
#include <xkbcommon/xkbcommon.h>

#include <wlr/types/wlr_virtual_keyboard_v1.h>

#include <wpe/wpe-platform.h>

#include "viewport.h"

/* ------------------------------------------------------------------------
 * Hit-testing
 * --------------------------------------------------------------------- */

struct wlr_surface *viewport_surface_at(struct viewport_server *server,
	double lx, double ly, double *sx, double *sy,
	struct viewport_toplevel **toplevel_out)
{
	if (toplevel_out != NULL) {
		*toplevel_out = NULL;
	}

	/* In the overview every window on screen is a miniature the shell is
	 * arranging, and a click on one means "take me there". Reporting no surface
	 * sends the click to the web layer, which is the thing that can answer
	 * that. */
	if (server->overview) {
		return NULL;
	}

	struct wlr_scene_node *node =
		wlr_scene_node_at(&server->scene->tree.node, lx, ly, sx, sy);
	if (node == NULL || node->type != WLR_SCENE_NODE_BUFFER) {
		return NULL;
	}

	/* While the session is locked, only the locker may be pointed at.
	 *
	 * Keyboard focus was already being held by the lock surface, but nothing
	 * was stopping the pointer: a click landed on whatever window happened to
	 * be under it, behind the lock screen. Buttons were being pressed in
	 * applications nobody could see. */
	if (server->locked) {
		bool under_lock = false;
		for (struct wlr_scene_tree *tree = node->parent; tree != NULL;
				tree = tree->node.parent) {
			if (tree == server->layer_lock) {
				under_lock = true;
				break;
			}
		}
		if (!under_lock) {
			return NULL;
		}
	}

	struct wlr_scene_buffer *scene_buffer = wlr_scene_buffer_from_node(node);
	struct wlr_scene_surface *scene_surface =
		wlr_scene_surface_try_from_buffer(scene_buffer);
	if (scene_surface == NULL) {
		/* A buffer node that is not a client surface: the shell itself. */
		return NULL;
	}

	if (toplevel_out != NULL) {
		/* Climb until we find a tagged node. Layer surfaces also set data, so
		 * the tag has to be checked — blindly casting whatever is found to a
		 * toplevel crashes the moment the cursor crosses a launcher. */
		for (struct wlr_scene_tree *tree = node->parent; tree != NULL;
				tree = tree->node.parent) {
			struct viewport_node *tagged = tree->node.data;
			if (tagged == NULL) {
				continue;
			}
			if (tagged->type == VIEWPORT_NODE_TOPLEVEL) {
				struct viewport_toplevel *found =
					(struct viewport_toplevel *)tagged;
				/* An X11 menu is not a window to focus. Reporting it as one
				 * had a click on a menu item run the whole focus path against
				 * it — including activating the surface, which a menu never
				 * asked for and which is enough to stop it responding. Steam's
				 * menus opened and then ignored everything. Left NULL, the
				 * click is delivered to the surface and nothing else. */
				if (!viewport_view_is_unmanaged(found)) {
					*toplevel_out = found;
				}
			}
			/* A layer surface is not focusable as a window: leave the output
			 * NULL so callers route the event to the surface directly. */
			break;
		}
	}

	return scene_surface->surface;
}

/* Hand the keyboard back after something transient — a launcher, an X11 menu —
 * has finished with it.
 *
 * Not simply to whatever server->focused points at. That is the last window to
 * hold focus, and it may not be on screen any more: the shell parks windows on
 * other workspaces and marks them invisible, so focusing one is a request for
 * the shell to go to it. Switching to an empty workspace and dismissing a
 * launcher there would drag the session back to whichever workspace the
 * previously focused window happened to be on.
 *
 * The shell is the right answer in that case. It owns the notion of a current
 * workspace, and giving it the keyboard changes nothing about where you are. */
void viewport_focus_restore(struct viewport_server *server)
{
	struct viewport_toplevel *previous = server->focused;

	if (previous == NULL || !previous->mapped || !previous->visible) {
		viewport_focus_web(server);
		return;
	}

	/* Cleared first, so viewport_toplevel_focus() does not take its early
	 * return for a window it believes is already focused. */
	server->focused = NULL;
	viewport_toplevel_focus(previous);
}

void viewport_focus_web(struct viewport_server *server)
{
	if (server->focused != NULL) {
		viewport_view_set_activated(server->focused, false);
		server->focused = NULL;
	}

	wlr_seat_keyboard_notify_clear_focus(server->seat);

	if (server->web != NULL) {
		viewport_web_focus(server->web, true);
	}
	viewport_ipc_notify_focus(server, 0);
}

/* ------------------------------------------------------------------------
 * Pointer
 * --------------------------------------------------------------------- */

static void process_cursor_motion(struct viewport_server *server,
	uint32_t time_msec)
{
	/* Moving a floating window works the same way: the shell owns the position,
	 * so the compositor only reports how far the pointer travelled. */
	if (server->moving != NULL) {
		double dx = server->cursor->x - server->resize_start_x;
		double dy = server->cursor->y - server->resize_start_y;
		if (dx != 0.0 || dy != 0.0) {
			server->resize_start_x = server->cursor->x;
			server->resize_start_y = server->cursor->y;

			char command[96];
			snprintf(command, sizeof(command), "layout.move.delta %u %d %d",
				server->moving->id, (int)dx, (int)dy);
			viewport_ipc_notify_shell_command(server, command);
		}
		return;
	}

	/* An interactive resize owns the pointer: report the delta to the shell,
	 * which turns it into split weights, and deliver nothing to clients. */
	if (server->resizing != NULL) {
		double dx = server->cursor->x - server->resize_start_x;
		double dy = server->cursor->y - server->resize_start_y;

		/* Only report whole pixels, and only when there is something to
		 * report, so a still pointer does not spam the shell. */
		if ((int)dx != 0 || (int)dy != 0) {
			server->resize_start_x = server->cursor->x;
			server->resize_start_y = server->cursor->y;

			char command[96];
			snprintf(command, sizeof(command), "layout.resize.delta %u %d %d",
				server->resizing->id, (int)dx, (int)dy);
			viewport_ipc_notify_shell_command(server, command);
		}
		return;
	}

	/* A button pressed on shell chrome holds the pointer until release.
	 *
	 * Without this, dragging the divider between two windows breaks the
	 * instant the cursor crosses onto a window: hit-testing would start
	 * routing motion to that client and the shell would never see the rest of
	 * the drag. Wayland gives clients an implicit grab for exactly this
	 * reason; the shell needs the same. */
	if (server->pointer_grab_web) {
		if (server->web != NULL) {
			viewport_web_pointer_motion(server->web, time_msec,
				server->cursor->x, server->cursor->y);
		}
		return;
	}

	double sx, sy;
	struct viewport_toplevel *toplevel = NULL;
	struct wlr_surface *surface = viewport_surface_at(server,
		server->cursor->x, server->cursor->y, &sx, &sy, &toplevel);

	if (surface == NULL) {
		/* Over the shell. Drop the client's pointer focus so it stops
		 * receiving motion, and hand the event to WebKit in layout space. */
		if (!server->pointer_on_web) {
			wlr_seat_pointer_notify_clear_focus(server->seat);
			viewport_pointer_check_constraint(server, NULL);
			server->pointer_on_web = true;
		}
		/* The shell draws its own cursor via CSS, but a client may have left
		 * a themed cursor behind, so restore the default. */
		wlr_cursor_set_xcursor(server->cursor, server->xcursor_mgr, "default");

		if (server->web != NULL) {
			viewport_web_pointer_motion(server->web, time_msec,
				server->cursor->x, server->cursor->y);
		}
		return;
	}

	if (server->pointer_on_web) {
		if (server->web != NULL) {
			/* WebKit needs an explicit leave or :hover states stick. */
			viewport_web_pointer_motion(server->web, time_msec, -1, -1);
		}
		server->pointer_on_web = false;
	}

	wlr_seat_pointer_notify_enter(server->seat, surface, sx, sy);
	wlr_seat_pointer_notify_motion(server->seat, time_msec, sx, sy);

	/* A constraint belongs to a surface, so it takes effect when that surface
	 * has the pointer and lapses when it does not. */
	viewport_pointer_check_constraint(server, surface);
}

/* Exposed so the tablet can move the cursor and have everything downstream —
 * focus, enter and leave, the shell's idea of where the pointer is — brought up
 * to date exactly as a mouse movement would. */
void viewport_cursor_refresh(struct viewport_server *server, uint32_t time_msec)
{
	process_cursor_motion(server, time_msec);
}

/* Re-run hit-testing because the windows moved, not the pointer.
 *
 * A client only acts on a button if it believes the pointer is inside it, and
 * it believes that because of wl_pointer.enter — which is only ever sent from
 * the motion path. So a surface that appears underneath a cursor that is not
 * moving never receives one, and every click on it is delivered and ignored.
 *
 * That is what an X11 menu is: it opens under the pointer, at the pointer,
 * without the pointer having moved. The click reached the menu surface and
 * Steam discarded it, which from the outside is indistinguishable from the
 * click never arriving.
 *
 * Called when something maps or unmaps, which is when what is under the cursor
 * can change without the cursor doing anything. */
void viewport_cursor_rebase(struct viewport_server *server)
{
	struct timespec now;
	clock_gettime(CLOCK_MONOTONIC, &now);
	uint32_t time_msec = (uint32_t)(now.tv_sec * 1000 + now.tv_nsec / 1000000);
	process_cursor_motion(server, time_msec);
}

/* Likewise for a button. The stylus falls back to a left click where nothing
 * speaks tablet-v2, and this is the same delivery path a mouse click takes. */
void viewport_pointer_button(struct viewport_server *server, uint32_t time_msec,
	uint32_t button, bool pressed)
{
	double sx, sy;
	struct viewport_toplevel *toplevel = NULL;
	struct wlr_surface *surface = viewport_surface_at(server, server->cursor->x,
		server->cursor->y, &sx, &sy, &toplevel);

	if (surface == NULL) {
		if (server->web != NULL) {
			viewport_web_pointer_button(server->web, time_msec,
				server->cursor->x, server->cursor->y, button, pressed);
		}
		return;
	}

	if (pressed && toplevel != NULL && server->focused != toplevel) {
		viewport_toplevel_focus(toplevel);
	}
	wlr_seat_pointer_notify_button(server->seat, time_msec, button,
		pressed ? WL_POINTER_BUTTON_STATE_PRESSED
			: WL_POINTER_BUTTON_STATE_RELEASED);
}

static void handle_cursor_motion(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, cursor_motion);
	struct wlr_pointer_motion_event *event = data;
	viewport_idle_activity(server);

	/* Relative motion first, and unconditionally: a game reads deltas whether
	 * or not the cursor is constrained, and mouselook is driven by how far the
	 * mouse moved rather than where it ended up. Unaccelerated values are
	 * passed through separately so a game can apply its own sensitivity. */
	viewport_pointer_send_relative(server, event->time_msec, event->delta_x,
		event->delta_y, event->unaccel_dx, event->unaccel_dy);

	/* A locked pointer does not move. Moving the cursor anyway would let it
	 * drift onto the other monitor during a fight, and would feed the client
	 * absolute motion it is no longer expecting. */
	if (viewport_pointer_is_locked(server)) {
		return;
	}

	/* A confined pointer still moves, but only inside the region the client
	 * named, so the move is resolved to a target position and clamped rather
	 * than applied as a raw delta. */
	double lx = server->cursor->x + event->delta_x;
	double ly = server->cursor->y + event->delta_y;
	if (viewport_pointer_confine(server, &lx, &ly)) {
		wlr_cursor_warp_closest(server->cursor, &event->pointer->base, lx, ly);
	} else {
		wlr_cursor_move(server->cursor, &event->pointer->base, event->delta_x,
			event->delta_y);
	}
	process_cursor_motion(server, event->time_msec);
}

static void handle_cursor_motion_absolute(struct wl_listener *listener,
	void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, cursor_motion_absolute);
	struct wlr_pointer_motion_absolute_event *event = data;
	viewport_idle_activity(server);

	/* Absolute devices — tablets, and the nested backends — still have to
	 * produce relative deltas for a constrained client, so derive them from
	 * the cursor's current position before it moves. */
	double lx, ly;
	wlr_cursor_absolute_to_layout_coords(server->cursor,
		&event->pointer->base, event->x, event->y, &lx, &ly);
	double dx = lx - server->cursor->x;
	double dy = ly - server->cursor->y;
	viewport_pointer_send_relative(server, event->time_msec, dx, dy, dx, dy);

	if (viewport_pointer_is_locked(server)) {
		return;
	}

	if (viewport_pointer_confine(server, &lx, &ly)) {
		wlr_cursor_warp_closest(server->cursor, &event->pointer->base, lx, ly);
	} else {
		wlr_cursor_warp_absolute(server->cursor, &event->pointer->base, event->x,
			event->y);
	}
	process_cursor_motion(server, event->time_msec);
}

/* Modifier state, for gestures like Mod4 + drag. */
static uint32_t seat_modifiers(struct viewport_server *server)
{
	struct wlr_keyboard *keyboard = wlr_seat_get_keyboard(server->seat);
	return keyboard != NULL ? wlr_keyboard_get_modifiers(keyboard) : 0;
}

static void handle_cursor_button(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, cursor_button);
	struct wlr_pointer_button_event *event = data;
	bool pressed = event->state == WL_POINTER_BUTTON_STATE_PRESSED;

	/* Releasing anything ends an interactive resize or a shell drag.
	 *
	 * All of it, not the first one that matches. These used to return early in
	 * turn, so a release that ended a move while the shell also held the
	 * pointer cleared the move and left pointer_grab_web set — and that grab
	 * has no other way to end, so every click for the rest of the session went
	 * to the shell instead of to whatever was under the cursor. */
	if (!pressed) {
		bool consumed = false;
		if (server->resizing != NULL) {
			server->resizing = NULL;
			consumed = true;
		}
		if (server->moving != NULL) {
			server->moving = NULL;
			consumed = true;
		}
		if (server->pointer_grab_web) {
			server->pointer_grab_web = false;
			if (server->web != NULL) {
				viewport_web_pointer_button(server->web, event->time_msec,
					server->cursor->x, server->cursor->y, event->button, false);
			}
			consumed = true;
		}
		if (consumed) {
			return;
		}
	}

	double sx, sy;
	struct viewport_toplevel *toplevel = NULL;
	struct wlr_surface *surface = viewport_surface_at(server,
		server->cursor->x, server->cursor->y, &sx, &sy, &toplevel);

	/* Mod4 + right drag resizes, as in sway. The modifier is what makes this
	 * a compositor gesture rather than a click the client should see, so the
	 * event is consumed and never forwarded.
	 *
	 * Dragging a window *edge* is deliberately not handled here: the gap
	 * between windows is drawn by the shell, so those pixels belong to the web
	 * layer and the shell implements that itself. */
	/* Mod4 + left drag moves a window, as in sway. Only floating windows have a
	 * position of their own to move; on a tiled one the shell ignores it. */
	if (pressed && toplevel != NULL && event->button == BTN_LEFT &&
			(seat_modifiers(server) & WLR_MODIFIER_LOGO)) {
		server->moving = toplevel;
		server->resize_start_x = server->cursor->x;
		server->resize_start_y = server->cursor->y;
		viewport_toplevel_focus(toplevel);
		return;
	}

	if (pressed && toplevel != NULL && event->button == BTN_RIGHT &&
			(seat_modifiers(server) & WLR_MODIFIER_LOGO)) {
		server->resizing = toplevel;
		server->resize_start_x = server->cursor->x;
		server->resize_start_y = server->cursor->y;
		viewport_toplevel_focus(toplevel);
		return;
	}

	if (surface == NULL) {
		/* Clicking the shell — a titlebar, the dock, the desktop. Focus goes
		 * to the web view so keyboard input follows the click. */
		if (server->config.debug && pressed) {
			wlr_log(WLR_DEBUG, "click at %.0f,%.0f -> the shell",
				server->cursor->x, server->cursor->y);
		}
		if (pressed && server->focused != NULL) {
			viewport_focus_web(server);
		}
		if (pressed) {
			server->pointer_grab_web = true;
		}
		if (server->web != NULL) {
			viewport_web_pointer_button(server->web, event->time_msec,
				server->cursor->x, server->cursor->y, event->button, pressed);
		}
		return;
	}

	if (pressed && toplevel != NULL) {
		viewport_toplevel_focus(toplevel);
	}

	/* One line per click, saying where it went. An X11 menu that ignores clicks
	 * looks identical from the outside whether the click never arrived or
	 * arrived and was discarded, and those are different bugs.
	 *
	 * Under --debug rather than --trace: it is one line per press, not per
	 * frame, and it is the first thing wanted when something will not take a
	 * click. Behind --trace it was effectively unavailable, because nobody runs
	 * a session that logs sixty lines a second on the chance of needing it. */
	if (server->config.debug && pressed) {
		wlr_log(WLR_DEBUG, "click at %.0f,%.0f -> surface %p (%s)%s",
			server->cursor->x, server->cursor->y, (void *)surface,
			toplevel != NULL ? "a window" : "the surface alone",
			server->pointer_grab_web ? " [shell holds the pointer]" : "");
	}

	wlr_seat_pointer_notify_button(server->seat, event->time_msec,
		event->button, event->state);
}

static void handle_cursor_axis(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, cursor_axis);
	struct wlr_pointer_axis_event *event = data;

	if (server->pointer_on_web) {
		double dx = 0, dy = 0;
		if (event->orientation == WL_POINTER_AXIS_HORIZONTAL_SCROLL) {
			dx = event->delta;
		} else {
			dy = event->delta;
		}
		if (server->web != NULL) {
			viewport_web_pointer_axis(server->web, event->time_msec,
				server->cursor->x, server->cursor->y, dx, dy,
				event->source == WL_POINTER_AXIS_SOURCE_FINGER);
		}
		return;
	}

	wlr_seat_pointer_notify_axis(server->seat, event->time_msec,
		event->orientation, event->delta, event->delta_discrete, event->source,
		event->relative_direction);
}

static void handle_cursor_frame(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, cursor_frame);
	if (!server->pointer_on_web) {
		wlr_seat_pointer_notify_frame(server->seat);
	}
}

static void handle_request_cursor(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, request_cursor);
	struct wlr_seat_pointer_request_set_cursor_event *event = data;

	/* Honour the request only from the client that currently holds pointer
	 * focus, or any client could hijack the cursor. */
	if (server->seat->pointer_state.focused_client == event->seat_client) {
		wlr_cursor_set_surface(server->cursor, event->surface,
			event->hotspot_x, event->hotspot_y);
	}
}

/* A client may ask for a named cursor shape rather than supplying a surface —
 * the modern way to request a resize arrow or an I-beam. Without this such
 * clients get no cursor change at all. */
static void handle_request_set_shape(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, request_set_shape);
	struct wlr_cursor_shape_manager_v1_request_set_shape_event *event = data;

	if (server->seat->pointer_state.focused_client != event->seat_client) {
		return;
	}
	wlr_cursor_set_xcursor(server->cursor, server->xcursor_mgr,
		wlr_cursor_shape_v1_name(event->shape));
}

/* ------------------------------------------------------------------------
 * Touch
 *
 * A touchscreen is not a mouse: several fingers are down at once, each with its
 * own id, and the client tracks them itself. Routing touch through the cursor
 * would collapse that to one point and lose multi-touch entirely, so the events
 * go to the seat's touch interface directly.
 *
 * The cursor is still moved to the first contact. Nothing else knows where a
 * finger is, so without it a tap would not raise the window it landed on and
 * the shell would keep highlighting whatever the mouse last touched.
 * --------------------------------------------------------------------- */

static struct wlr_surface *touch_surface_at(struct viewport_server *server,
	struct wlr_touch *touch, double x, double y, double *sx, double *sy,
	struct viewport_toplevel **toplevel_out)
{
	double lx, ly;
	wlr_cursor_absolute_to_layout_coords(server->cursor, &touch->base, x, y,
		&lx, &ly);
	return viewport_surface_at(server, lx, ly, sx, sy, toplevel_out);
}

static void handle_touch_down(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, touch_down);
	struct wlr_touch_down_event *event = data;
	viewport_idle_activity(server);

	double sx, sy;
	struct viewport_toplevel *toplevel = NULL;
	struct wlr_surface *surface = touch_surface_at(server, event->touch,
		event->x, event->y, &sx, &sy, &toplevel);

	if (surface == NULL) {
		return;
	}

	/* A tap focuses what it lands on, the same as a click does. */
	if (toplevel != NULL && server->focused != toplevel) {
		viewport_toplevel_focus(toplevel);
	}

	wlr_seat_touch_notify_down(server->seat, surface, event->time_msec,
		event->touch_id, sx, sy);
}

static void handle_touch_up(struct wl_listener *listener, void *data)
{
	struct viewport_server *server = wl_container_of(listener, server, touch_up);
	struct wlr_touch_up_event *event = data;

	wlr_seat_touch_notify_up(server->seat, event->time_msec, event->touch_id);
}

static void handle_touch_motion(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, touch_motion);
	struct wlr_touch_motion_event *event = data;

	double sx, sy;
	struct wlr_surface *surface = touch_surface_at(server, event->touch,
		event->x, event->y, &sx, &sy, NULL);

	/* A finger that has slid off the surface it started on still belongs to
	 * that surface: the client owns the gesture until the finger lifts. wlroots
	 * discards motion for a point it never saw go down, so passing the surface
	 * under the finger is safe either way. */
	if (surface == NULL) {
		return;
	}

	wlr_seat_touch_notify_motion(server->seat, event->time_msec,
		event->touch_id, sx, sy);
}

static void handle_touch_frame(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, touch_frame);
	wlr_seat_touch_notify_frame(server->seat);
}

static void handle_touch_cancel(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, touch_cancel);
	struct wlr_touch_cancel_event *event = data;

	/* Cancel is addressed to the client owning the point, not the surface: the
	 * whole gesture is being taken away from it, across every finger it is
	 * tracking. */
	struct wlr_touch_point *point =
		wlr_seat_touch_get_point(server->seat, event->touch_id);
	if (point != NULL && point->client != NULL) {
		wlr_seat_touch_notify_cancel(server->seat, point->client);
	}
}

/* Keyboard shortcut inhibiting.
 *
 * A virtual machine, a nested compositor or a remote desktop client needs the
 * chords this compositor would otherwise swallow — Mod4+Return has to reach the
 * guest, not open a terminal here. The protocol is how a client asks for that,
 * and it only takes effect while that client is focused, so the bindings come
 * back the moment focus moves away.
 *
 * The exit binding is deliberately not inhibited. A client that has taken the
 * keyboard and then stops responding would otherwise leave no way out of the
 * session short of a TTY. */
static void handle_inhibitor_destroy(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, inhibitor_destroy);

	wl_list_remove(&server->inhibitor_destroy.link);
	server->active_inhibitor = NULL;
}

static void handle_new_shortcuts_inhibitor(struct wl_listener *listener,
	void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, new_shortcuts_inhibitor);
	struct wlr_keyboard_shortcuts_inhibitor_v1 *inhibitor = data;

	/* Only the focused client may take the keyboard, and only one at a time. */
	if (server->seat->keyboard_state.focused_surface != inhibitor->surface) {
		return;
	}
	if (server->active_inhibitor != NULL) {
		return;
	}

	server->active_inhibitor = inhibitor;
	server->inhibitor_destroy.notify = handle_inhibitor_destroy;
	wl_signal_add(&inhibitor->events.destroy, &server->inhibitor_destroy);

	wlr_keyboard_shortcuts_inhibitor_v1_activate(inhibitor);
	wlr_log(WLR_INFO, "client took the keyboard shortcuts");
}

/* True while a focused client has asked for the keyboard. */
static bool shortcuts_inhibited(struct viewport_server *server)
{
	return server->active_inhibitor != NULL &&
		server->active_inhibitor->surface ==
			server->seat->keyboard_state.focused_surface;
}

/* Drag and drop between applications.
 *
 * Without these the compositor silently drops every drag: dragging a file out
 * of a file manager, or a tab out of a browser, starts and then nothing
 * happens, because the seat is never told to begin the drag and the icon that
 * follows the cursor is never put in the scene.
 */
static void handle_request_start_drag(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, request_start_drag);
	struct wlr_seat_request_start_drag_event *event = data;

	/* Only honour a drag from the client that actually holds the implicit
	 * grab, or any client could start one at any time. */
	if (wlr_seat_validate_pointer_grab_serial(server->seat, event->origin,
			event->serial)) {
		wlr_seat_start_pointer_drag(server->seat, event->drag, event->serial);
		return;
	}

	wlr_data_source_destroy(event->drag->source);
}

static void handle_start_drag(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, start_drag);
	struct wlr_drag *drag = data;

	if (drag->icon == NULL) {
		return;
	}

	/* The icon rides above everything, including a fullscreen window, and is
	 * positioned by the scene as the cursor moves. */
	struct wlr_scene_tree *tree =
		wlr_scene_drag_icon_create(server->layer_overlay, drag->icon);
	if (tree != NULL) {
		wlr_scene_node_set_position(&tree->node, server->cursor->x,
			server->cursor->y);
	}
}

static void handle_request_set_primary_selection(struct wl_listener *listener,
	void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, request_set_primary_selection);
	struct wlr_seat_request_set_primary_selection_event *event = data;
	wlr_seat_set_primary_selection(server->seat, event->source, event->serial);
}

static void handle_request_set_selection(struct wl_listener *listener,
	void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, request_set_selection);
	struct wlr_seat_request_set_selection_event *event = data;
	wlr_seat_set_selection(server->seat, event->source, event->serial);
}

void viewport_cursor_init(struct viewport_server *server)
{
	server->cursor = wlr_cursor_create();
	wlr_cursor_attach_output_layout(server->cursor, server->output_layout);

	/* Theme and size come from the config so a HiDPI setup is not stuck with a
	 * 24px cursor, and the same values are reported to clients through the
	 * settings portal. */
	int cursor_size = server->config.cursor_size > 0
		? server->config.cursor_size : 24;
	server->xcursor_mgr = wlr_xcursor_manager_create(
		server->config.cursor_theme, cursor_size);
	server->pointer_on_web = true;

	server->cursor_motion.notify = handle_cursor_motion;
	wl_signal_add(&server->cursor->events.motion, &server->cursor_motion);
	server->cursor_motion_absolute.notify = handle_cursor_motion_absolute;
	wl_signal_add(&server->cursor->events.motion_absolute,
		&server->cursor_motion_absolute);
	server->cursor_button.notify = handle_cursor_button;
	wl_signal_add(&server->cursor->events.button, &server->cursor_button);
	server->cursor_axis.notify = handle_cursor_axis;
	wl_signal_add(&server->cursor->events.axis, &server->cursor_axis);
	server->cursor_frame.notify = handle_cursor_frame;
	wl_signal_add(&server->cursor->events.frame, &server->cursor_frame);

	server->request_cursor.notify = handle_request_cursor;
	wl_signal_add(&server->seat->events.request_set_cursor,
		&server->request_cursor);
	server->request_set_selection.notify = handle_request_set_selection;
	wl_signal_add(&server->seat->events.request_set_selection,
		&server->request_set_selection);
	server->request_set_primary_selection.notify =
		handle_request_set_primary_selection;
	wl_signal_add(&server->seat->events.request_set_primary_selection,
		&server->request_set_primary_selection);
	server->cursor_shape = wlr_cursor_shape_manager_v1_create(
		server->wl_display, 1);
	if (server->cursor_shape != NULL) {
		server->request_set_shape.notify = handle_request_set_shape;
		wl_signal_add(&server->cursor_shape->events.request_set_shape,
			&server->request_set_shape);
	}

	server->shortcuts_inhibit =
		wlr_keyboard_shortcuts_inhibit_v1_create(server->wl_display);
	if (server->shortcuts_inhibit != NULL) {
		server->new_shortcuts_inhibitor.notify = handle_new_shortcuts_inhibitor;
		wl_signal_add(&server->shortcuts_inhibit->events.new_inhibitor,
			&server->new_shortcuts_inhibitor);
	}

	server->touch_down.notify = handle_touch_down;
	wl_signal_add(&server->cursor->events.touch_down, &server->touch_down);
	server->touch_up.notify = handle_touch_up;
	wl_signal_add(&server->cursor->events.touch_up, &server->touch_up);
	server->touch_motion.notify = handle_touch_motion;
	wl_signal_add(&server->cursor->events.touch_motion, &server->touch_motion);
	server->touch_frame.notify = handle_touch_frame;
	wl_signal_add(&server->cursor->events.touch_frame, &server->touch_frame);
	server->touch_cancel.notify = handle_touch_cancel;
	wl_signal_add(&server->cursor->events.touch_cancel, &server->touch_cancel);

	server->request_start_drag.notify = handle_request_start_drag;
	wl_signal_add(&server->seat->events.request_start_drag,
		&server->request_start_drag);
	server->start_drag.notify = handle_start_drag;
	wl_signal_add(&server->seat->events.start_drag, &server->start_drag);
}

/* ------------------------------------------------------------------------
 * Keyboard
 * --------------------------------------------------------------------- */

static uint32_t wpe_modifiers_from_keyboard(struct wlr_keyboard *keyboard)
{
	uint32_t mods = wlr_keyboard_get_modifiers(keyboard);
	uint32_t out = 0;

	if (mods & WLR_MODIFIER_CTRL) {
		out |= WPE_MODIFIER_KEYBOARD_CONTROL;
	}
	if (mods & WLR_MODIFIER_SHIFT) {
		out |= WPE_MODIFIER_KEYBOARD_SHIFT;
	}
	if (mods & WLR_MODIFIER_ALT) {
		out |= WPE_MODIFIER_KEYBOARD_ALT;
	}
	if (mods & WLR_MODIFIER_LOGO) {
		out |= WPE_MODIFIER_KEYBOARD_META;
	}
	if (mods & WLR_MODIFIER_CAPS) {
		out |= WPE_MODIFIER_KEYBOARD_CAPS_LOCK;
	}
	return out;
}

static void handle_keyboard_modifiers(struct wl_listener *listener, void *data)
{
	struct viewport_keyboard *keyboard =
		wl_container_of(listener, keyboard, modifiers);
	struct viewport_server *server = keyboard->server;

	wlr_seat_set_keyboard(server->seat, keyboard->wlr_keyboard);

	/* A bar that appears while Mod4 is held needs to know when that starts and
	 * stops, and this is the only place that sees it — a binding fires on a
	 * chord, not on the modifier alone.
	 *
	 * On the transition only, and only when something is listening: this
	 * handler runs for every Shift and Ctrl of ordinary typing, and a message
	 * per keystroke to say Mod4 is still not held would be pure noise. Sent
	 * before the IME check, because whether an input method holds the grab has
	 * no bearing on whether the key is physically down. */
	if (server->config.bar != NULL && strcmp(server->config.bar, "auto") == 0) {
		bool logo = (wlr_keyboard_get_modifiers(keyboard->wlr_keyboard) &
			WLR_MODIFIER_LOGO) != 0;
		if (logo != server->logo_held) {
			server->logo_held = logo;
			viewport_ipc_notify_modifiers(server, logo);
		}
	}

	/* An input method holding the keyboard grab gets the modifiers instead of
	 * the client, matching where the keys themselves are going. */
	if (viewport_ime_handle_modifiers(server->ime, keyboard->wlr_keyboard)) {
		return;
	}

	if (server->focused != NULL) {
		wlr_seat_keyboard_notify_modifiers(server->seat,
			&keyboard->wlr_keyboard->modifiers);
	}
}

static void handle_keyboard_key(struct wl_listener *listener, void *data)
{
	struct viewport_keyboard *keyboard = wl_container_of(listener, keyboard, key);
	struct viewport_server *server = keyboard->server;
	struct wlr_keyboard_key_event *event = data;
	bool pressed = event->state == WL_KEYBOARD_KEY_STATE_PRESSED;

	/* Presses only. A release is the tail of something already counted, and
	 * counting it undoes a binding that turned the screens off: the chord fires
	 * on press, and letting go of it then says someone is there. Holding the
	 * keys longer would outlast any grace period, so the release must not count
	 * at all rather than merely count late. */
	if (pressed) {
		viewport_idle_activity(server);
	}

	/* WPE wants an evdev keycode (libinput's, offset by 8 as X11 numbers
	 * them) plus the resolved keysym. Resolve both up front: compositor
	 * bindings are checked against the same keysyms. */
	uint32_t keycode = event->keycode + 8;
	const xkb_keysym_t *syms;
	int nsyms = xkb_state_key_get_syms(keyboard->wlr_keyboard->xkb_state,
		keycode, &syms);
	uint32_t modifiers = wlr_keyboard_get_modifiers(keyboard->wlr_keyboard);

	/* Bindings must also be matched against the *untranslated* keysyms.
	 *
	 * Holding Shift makes xkb translate `e` into `E`, so a binding written
	 * "Mod4+Shift+e" — which parses to the keysym `e` — never matches the `E`
	 * that actually arrives. That silently kills every shifted bind while
	 * unshifted ones like Mod4+d keep working. Level 0 of the current layout
	 * gives the unshifted symbol, which is what the config text means. */
	const xkb_keysym_t *raw_syms = NULL;
	int n_raw = 0;
	struct xkb_keymap *keymap =
		xkb_state_get_keymap(keyboard->wlr_keyboard->xkb_state);
	if (keymap != NULL) {
		xkb_layout_index_t layout = xkb_state_key_get_layout(
			keyboard->wlr_keyboard->xkb_state, keycode);
		if (layout != XKB_LAYOUT_INVALID) {
			n_raw = xkb_keymap_key_get_syms_by_level(keymap, keycode, layout,
				0, &raw_syms);
		}
	}

	/* VT switching, checked before anything else and never configurable.
	 *
	 * This is the escape hatch. Running on a TTY, if the shell never paints or
	 * the compositor wedges, Ctrl+Alt+F2 is the only way back to a console
	 * short of a hard reset — so it must not depend on the config file being
	 * valid, on a binding having been registered, or on the shell being alive.
	 * The keysym already encodes the target VT, so no modifier check is
	 * needed. Absent under the Wayland and headless backends, where there is
	 * no session to switch. */
	if (pressed) {
		for (int i = 0; i < nsyms; i++) {
			if (syms[i] < XKB_KEY_XF86Switch_VT_1 ||
					syms[i] > XKB_KEY_XF86Switch_VT_12) {
				continue;
			}
			unsigned vt = syms[i] - XKB_KEY_XF86Switch_VT_1 + 1;
			if (server->session == NULL) {
				wlr_log(WLR_ERROR,
					"VT switch to %u ignored: no session (nested backend?)", vt);
				return;
			}
			wlr_log(WLR_INFO, "switching to VT %u", vt);
			if (!wlr_session_change_vt(server->session, vt)) {
				wlr_log(WLR_ERROR, "wlr_session_change_vt(%u) failed", vt);
			}
			return;
		}

		/* One line per press when debugging, so a binding that "does nothing"
		 * can be told apart from one whose chord never reached us. */
		if (server->config.debug && nsyms > 0) {
			char name[64], raw_name[64] = "-";
			xkb_keysym_get_name(syms[0], name, sizeof(name));
			if (n_raw > 0) {
				xkb_keysym_get_name(raw_syms[0], raw_name, sizeof(raw_name));
			}
			wlr_log(WLR_DEBUG, "key press: %s (raw %s, mods 0x%x)", name,
				raw_name, modifiers);
		}
	}

	/* Compositor bindings outrank both the focused client and the shell, and
	 * are checked on press only — forwarding the release of a consumed chord
	 * would leave the client with an unmatched key-up.
	 *
	 * Unless the focused client has asked for the keyboard, in which case they
	 * are its chords for as long as it holds focus. */
	if (pressed && !server->locked && !shortcuts_inhibited(server) &&
			(viewport_bindings_handle(server, modifiers, raw_syms, n_raw) ||
			 viewport_bindings_handle(server, modifiers, syms, nsyms))) {
		return;
	}

	/* Route by who actually holds seat keyboard focus, not by whether a
	 * toplevel is focused. A layer surface — a launcher, a lock screen — takes
	 * focus without ever becoming a toplevel, and checking server->focused
	 * alone would silently deliver its keystrokes to the web shell instead,
	 * leaving the launcher unable to type. */
	/* An input method composing a character takes the keyboard: the keys are
	 * how it is being driven, and the application must not also see them or
	 * everything gets typed twice. Checked after bindings, so Mod4 chords still
	 * work while composing. */
	if (viewport_ime_handle_key(server->ime, keyboard->wlr_keyboard,
			event->time_msec, event->keycode, event->state)) {
		return;
	}

	if (server->seat->keyboard_state.focused_surface != NULL) {
		wlr_seat_set_keyboard(server->seat, keyboard->wlr_keyboard);
		wlr_seat_keyboard_notify_key(server->seat, event->time_msec,
			event->keycode, event->state);
		return;
	}

	if (server->web != NULL) {
		uint32_t keysym = nsyms > 0 ? syms[0] : XKB_KEY_NoSymbol;
		viewport_web_keyboard_key(server->web, event->time_msec, keycode,
			keysym, pressed, wpe_modifiers_from_keyboard(keyboard->wlr_keyboard));
	}
}

static void handle_keyboard_destroy(struct wl_listener *listener, void *data)
{
	struct viewport_keyboard *keyboard =
		wl_container_of(listener, keyboard, destroy);

	wl_list_remove(&keyboard->modifiers.link);
	wl_list_remove(&keyboard->key.link);
	wl_list_remove(&keyboard->destroy.link);
	wl_list_remove(&keyboard->link);
	free(keyboard);
}

/* Apply the configured layout and repeat rate to one keyboard.
 *
 * Separate from creation so a config reload can re-apply it to keyboards that
 * already exist — otherwise changing the layout would only affect devices
 * plugged in afterwards. A virtual keyboard is skipped: it supplies its own
 * keymap, and overwriting that would break whatever is driving it. */
static void configure_keyboard(struct viewport_server *server,
	struct wlr_keyboard *wlr_keyboard)
{
	/* A hardcoded layout is wrong for anyone not on a US keyboard, and there
	 * is no way to discover the right one — so it comes from the config, with
	 * NULL fields falling back to libxkbcommon's defaults. */
	struct xkb_context *context = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
	struct xkb_rule_names rules = {
		.layout = server->config.xkb_layout,
		.variant = server->config.xkb_variant,
		.options = server->config.xkb_options,
	};
	struct xkb_keymap *keymap = xkb_keymap_new_from_names(context, &rules,
		XKB_KEYMAP_COMPILE_NO_FLAGS);
	if (keymap == NULL) {
		wlr_log(WLR_ERROR, "invalid keymap (layout '%s'), using the default",
			server->config.xkb_layout ? server->config.xkb_layout : "");
		keymap = xkb_keymap_new_from_names(context, NULL,
			XKB_KEYMAP_COMPILE_NO_FLAGS);
	}
	if (keymap != NULL) {
		wlr_keyboard_set_keymap(wlr_keyboard, keymap);
		xkb_keymap_unref(keymap);
	}
	xkb_context_unref(context);

	wlr_keyboard_set_repeat_info(wlr_keyboard,
		server->config.repeat_rate > 0 ? server->config.repeat_rate : 25,
		server->config.repeat_delay > 0 ? server->config.repeat_delay : 600);
}

void viewport_keyboards_reconfigure(struct viewport_server *server)
{
	struct viewport_keyboard *keyboard;
	wl_list_for_each(keyboard, &server->keyboards, link) {
		if (keyboard->virtual_keyboard) {
			continue;
		}
		configure_keyboard(server, keyboard->wlr_keyboard);
	}
}

static void new_keyboard(struct viewport_server *server,
	struct wlr_input_device *device)
{
	struct wlr_keyboard *wlr_keyboard = wlr_keyboard_from_input_device(device);

	struct viewport_keyboard *keyboard = calloc(1, sizeof(*keyboard));
	if (keyboard == NULL) {
		return;
	}
	keyboard->server = server;
	keyboard->wlr_keyboard = wlr_keyboard;

	configure_keyboard(server, wlr_keyboard);

	keyboard->modifiers.notify = handle_keyboard_modifiers;
	wl_signal_add(&wlr_keyboard->events.modifiers, &keyboard->modifiers);
	keyboard->key.notify = handle_keyboard_key;
	wl_signal_add(&wlr_keyboard->events.key, &keyboard->key);
	keyboard->destroy.notify = handle_keyboard_destroy;
	wl_signal_add(&device->events.destroy, &keyboard->destroy);

	wlr_seat_set_keyboard(server->seat, wlr_keyboard);
	wl_list_insert(&server->keyboards, &keyboard->link);
}

void viewport_handle_new_virtual_keyboard(struct wl_listener *listener,
	void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, new_virtual_keyboard);
	struct wlr_virtual_keyboard_v1 *virtual_keyboard = data;

	/* Treated exactly like a physical keyboard: same bindings, same routing.
	 * The client supplies its own keymap, so new_keyboard()'s default is not
	 * imposed on it. */
	new_keyboard(server, &virtual_keyboard->keyboard.base);

	/* Flagged so a config reload does not overwrite that keymap. */
	struct viewport_keyboard *keyboard;
	wl_list_for_each(keyboard, &server->keyboards, link) {
		if (keyboard->wlr_keyboard == &virtual_keyboard->keyboard) {
			keyboard->virtual_keyboard = true;
			break;
		}
	}
}


void viewport_handle_new_input(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, new_input);
	struct wlr_input_device *device = data;

	switch (device->type) {
	case WLR_INPUT_DEVICE_KEYBOARD:
		new_keyboard(server, device);
		break;
	case WLR_INPUT_DEVICE_POINTER:
		wlr_cursor_attach_input_device(server->cursor, device);
		break;
	case WLR_INPUT_DEVICE_TABLET:
	case WLR_INPUT_DEVICE_TABLET_PAD:
		viewport_tablet_add(server, device);
		break;
	case WLR_INPUT_DEVICE_TOUCH:
		/* Attached to the cursor only for its output mapping — the events
		 * themselves are taken from the cursor's touch signals, not turned into
		 * pointer motion. */
		wlr_cursor_attach_input_device(server->cursor, device);
		server->has_touch = true;
		break;
	default:
		break;
	}

	uint32_t caps = WL_SEAT_CAPABILITY_POINTER;
	if (!wl_list_empty(&server->keyboards)) {
		caps |= WL_SEAT_CAPABILITY_KEYBOARD;
	}
	if (server->has_touch) {
		/* Clients bind wl_touch off this capability; without it a touchscreen
		 * produces events no one is listening for. */
		caps |= WL_SEAT_CAPABILITY_TOUCH;
	}
	wlr_seat_set_capabilities(server->seat, caps);
}
