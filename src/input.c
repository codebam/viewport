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

#include <wlr/types/wlr_data_device.h>
#include <wlr/types/wlr_pointer.h>
#include <wlr/types/wlr_scene.h>
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

	struct wlr_scene_node *node =
		wlr_scene_node_at(&server->scene->tree.node, lx, ly, sx, sy);
	if (node == NULL || node->type != WLR_SCENE_NODE_BUFFER) {
		return NULL;
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
				*toplevel_out = (struct viewport_toplevel *)tagged;
			}
			/* A layer surface is not focusable as a window: leave the output
			 * NULL so callers route the event to the surface directly. */
			break;
		}
	}

	return scene_surface->surface;
}

void viewport_focus_web(struct viewport_server *server)
{
	if (server->focused != NULL) {
		wlr_xdg_toplevel_set_activated(server->focused->xdg_toplevel, false);
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
}

static void handle_cursor_motion(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, cursor_motion);
	struct wlr_pointer_motion_event *event = data;

	wlr_cursor_move(server->cursor, &event->pointer->base, event->delta_x,
		event->delta_y);
	process_cursor_motion(server, event->time_msec);
}

static void handle_cursor_motion_absolute(struct wl_listener *listener,
	void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, cursor_motion_absolute);
	struct wlr_pointer_motion_absolute_event *event = data;

	wlr_cursor_warp_absolute(server->cursor, &event->pointer->base, event->x,
		event->y);
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

	/* Releasing anything ends an interactive resize or a shell drag. */
	if (!pressed && server->resizing != NULL) {
		server->resizing = NULL;
		return;
	}
	if (!pressed && server->pointer_grab_web) {
		server->pointer_grab_web = false;
		if (server->web != NULL) {
			viewport_web_pointer_button(server->web, event->time_msec,
				server->cursor->x, server->cursor->y, event->button, false);
		}
		return;
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

	server->xcursor_mgr = wlr_xcursor_manager_create(NULL, 24);
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
	 * would leave the client with an unmatched key-up. */
	if (pressed &&
			(viewport_bindings_handle(server, modifiers, raw_syms, n_raw) ||
			 viewport_bindings_handle(server, modifiers, syms, nsyms))) {
		return;
	}

	/* Route by who actually holds seat keyboard focus, not by whether a
	 * toplevel is focused. A layer surface — a launcher, a lock screen — takes
	 * focus without ever becoming a toplevel, and checking server->focused
	 * alone would silently deliver its keystrokes to the web shell instead,
	 * leaving the launcher unable to type. */
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

	struct xkb_context *context = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
	struct xkb_keymap *keymap = xkb_keymap_new_from_names(context, NULL,
		XKB_KEYMAP_COMPILE_NO_FLAGS);
	if (keymap != NULL) {
		wlr_keyboard_set_keymap(wlr_keyboard, keymap);
		xkb_keymap_unref(keymap);
	}
	xkb_context_unref(context);
	wlr_keyboard_set_repeat_info(wlr_keyboard, 25, 600);

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
	default:
		break;
	}

	uint32_t caps = WL_SEAT_CAPABILITY_POINTER;
	if (!wl_list_empty(&server->keyboards)) {
		caps |= WL_SEAT_CAPABILITY_KEYBOARD;
	}
	wlr_seat_set_capabilities(server->seat, caps);
}
