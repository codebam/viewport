/* SPDX-License-Identifier: MIT
 *
 * Input methods.
 *
 * Typing Chinese, Japanese or Korean — or emoji, or any compose sequence —
 * needs a program between the keyboard and the application: the user presses
 * several keys, sees candidates, and picks one. That program is fcitx5 or ibus,
 * and it cannot talk to the application directly. Both ends speak only to the
 * compositor, over two protocols that mirror each other:
 *
 *   text-input-v3     the application: "I have a text field here, at this
 *                     rectangle, containing this text around the cursor"
 *   input-method-v2   the input method: "show this as preedit, commit this
 *                     string, delete these characters"
 *
 * Nothing joins them up on its own. This file is that relay, and without it an
 * input method connects, sees no text fields ever, and does nothing — which is
 * also why foot warns about a missing text-input manager at startup.
 *
 * Three things beyond the relay are needed for it to actually be usable:
 *
 *   focus         a text input is only live while its own client holds keyboard
 *                 focus, so enter/leave follow the seat rather than the client's
 *                 own idea of being enabled.
 *   keyboard grab the input method takes the keyboard while composing —
 *                 otherwise the raw keys reach the application as well as the
 *                 candidate window, and every keystroke is typed twice.
 *   popups        the candidate list is a surface the compositor must place,
 *                 next to the text cursor rather than at the origin.
 */
#define _POSIX_C_SOURCE 200809L

#include <stdlib.h>

#include <wlr/types/wlr_compositor.h>
#include <wlr/types/wlr_input_method_v2.h>
#include <wlr/types/wlr_scene.h>
#include <wlr/types/wlr_seat.h>
#include <wlr/types/wlr_text_input_v3.h>
#include <wlr/util/log.h>

#include "viewport.h"

struct viewport_ime {
	struct viewport_server *server;

	struct wlr_text_input_manager_v3 *text_input_manager;
	struct wlr_input_method_manager_v2 *input_method_manager;

	/* At most one input method may be bound to a seat at a time; a second one
	 * is told it is unavailable rather than silently ignored. */
	struct wlr_input_method_v2 *input_method;
	struct wl_list text_inputs;
	/* The text input currently receiving the input method's output. */
	struct viewport_text_input *active;

	struct wlr_input_method_keyboard_grab_v2 *keyboard_grab;

	struct wl_listener new_text_input;
	struct wl_listener new_input_method;
	struct wl_listener input_method_commit;
	struct wl_listener input_method_new_popup;
	struct wl_listener input_method_grab_keyboard;
	struct wl_listener input_method_destroy;
	struct wl_listener keyboard_grab_destroy;
	struct wl_listener focus_change;
};

struct viewport_text_input {
	struct viewport_ime *ime;
	struct wlr_text_input_v3 *input;
	struct wl_list link;

	struct wl_listener enable;
	struct wl_listener commit;
	struct wl_listener disable;
	struct wl_listener destroy;
};

struct viewport_ime_popup {
	struct viewport_ime *ime;
	struct wlr_input_popup_surface_v2 *popup;
	struct wlr_scene_tree *tree;

	struct wl_listener commit;
	struct wl_listener destroy;
};

/* ------------------------------------------------------------------------
 * Popups
 * --------------------------------------------------------------------- */

/* Place the candidate window just below the text cursor.
 *
 * The rectangle the application gives is relative to its own surface, so it has
 * to be shifted by where the compositor put that window. Nudged back on screen
 * if it would run off the bottom or right, since a candidate list hanging past
 * the edge of the monitor cannot be read. */
static void position_popup(struct viewport_ime_popup *popup)
{
	struct viewport_ime *ime = popup->ime;
	if (popup->tree == NULL || ime->active == NULL) {
		return;
	}

	struct wlr_text_input_v3 *input = ime->active->input;
	struct wlr_surface *focused = input->focused_surface;
	if (focused == NULL) {
		return;
	}

	/* Anchor to whichever window owns the text field. */
	int base_x = 0, base_y = 0;
	struct viewport_toplevel *toplevel;
	wl_list_for_each(toplevel, &ime->server->toplevels, link) {
		if (viewport_view_surface(toplevel) == focused) {
			base_x = toplevel->box.x;
			base_y = toplevel->box.y;
			break;
		}
	}

	struct wlr_box cursor = input->current.cursor_rectangle;
	int x = base_x + cursor.x;
	int y = base_y + cursor.y + cursor.height;

	int width = popup->popup->surface->current.width;
	int height = popup->popup->surface->current.height;

	struct wlr_box output_box;
	wlr_output_layout_get_box(ime->server->output_layout, NULL, &output_box);
	if (x + width > output_box.x + output_box.width) {
		x = output_box.x + output_box.width - width;
	}
	if (y + height > output_box.y + output_box.height) {
		/* No room below: flip above the cursor rather than off the screen. */
		y = base_y + cursor.y - height;
	}
	if (x < output_box.x) {
		x = output_box.x;
	}
	if (y < output_box.y) {
		y = output_box.y;
	}

	wlr_scene_node_set_position(&popup->tree->node, x, y);

	/* The input method needs the rectangle in its own terms to lay the
	 * candidate list out sensibly. */
	struct wlr_box relative = {
		.x = cursor.x,
		.y = cursor.y,
		.width = cursor.width,
		.height = cursor.height,
	};
	wlr_input_popup_surface_v2_send_text_input_rectangle(popup->popup,
		&relative);
}

static void handle_popup_commit(struct wl_listener *listener, void *data)
{
	struct viewport_ime_popup *popup =
		wl_container_of(listener, popup, commit);
	position_popup(popup);
}

static void handle_popup_destroy(struct wl_listener *listener, void *data)
{
	struct viewport_ime_popup *popup =
		wl_container_of(listener, popup, destroy);

	wl_list_remove(&popup->commit.link);
	wl_list_remove(&popup->destroy.link);
	if (popup->tree != NULL) {
		wlr_scene_node_destroy(&popup->tree->node);
	}
	free(popup);
}

static void handle_new_popup(struct wl_listener *listener, void *data)
{
	struct viewport_ime *ime =
		wl_container_of(listener, ime, input_method_new_popup);
	struct wlr_input_popup_surface_v2 *popup_surface = data;

	struct viewport_ime_popup *popup = calloc(1, sizeof(*popup));
	if (popup == NULL) {
		return;
	}
	popup->ime = ime;
	popup->popup = popup_surface;

	/* Overlay, not the app layer: the candidate list belongs above whatever it
	 * is being typed into, including a fullscreen window. */
	popup->tree = wlr_scene_subsurface_tree_create(ime->server->layer_overlay,
		popup_surface->surface);

	popup->commit.notify = handle_popup_commit;
	wl_signal_add(&popup_surface->surface->events.commit, &popup->commit);
	popup->destroy.notify = handle_popup_destroy;
	wl_signal_add(&popup_surface->events.destroy, &popup->destroy);

	position_popup(popup);
}

/* ------------------------------------------------------------------------
 * The relay
 * --------------------------------------------------------------------- */

static void send_state_to_input_method(struct viewport_ime *ime,
	struct viewport_text_input *text_input)
{
	struct wlr_input_method_v2 *method = ime->input_method;
	if (method == NULL) {
		return;
	}

	struct wlr_text_input_v3 *input = text_input->input;

	if (input->active_features & WLR_TEXT_INPUT_V3_FEATURE_SURROUNDING_TEXT) {
		wlr_input_method_v2_send_surrounding_text(method,
			input->current.surrounding.text,
			input->current.surrounding.cursor,
			input->current.surrounding.anchor);
	}
	wlr_input_method_v2_send_text_change_cause(method,
		input->current.text_change_cause);
	if (input->active_features & WLR_TEXT_INPUT_V3_FEATURE_CONTENT_TYPE) {
		wlr_input_method_v2_send_content_type(method,
			input->current.content_type.hint,
			input->current.content_type.purpose);
	}
	wlr_input_method_v2_send_done(method);
}

/* Which text input, if any, should be live.
 *
 * Two conditions, and both matter: the client must hold keyboard focus, and it
 * must have enabled the input. A text input belonging to an unfocused window
 * would otherwise keep receiving what is typed into another one. */
static struct viewport_text_input *find_active(struct viewport_ime *ime)
{
	struct wlr_surface *focused =
		ime->server->seat->keyboard_state.focused_surface;
	if (focused == NULL) {
		return NULL;
	}

	struct viewport_text_input *text_input;
	wl_list_for_each(text_input, &ime->text_inputs, link) {
		if (text_input->input->focused_surface == focused &&
				text_input->input->current_enabled) {
			return text_input;
		}
	}
	return NULL;
}

/* Bring the input method's activation in line with which text input is live.
 * Activate and deactivate are edges, so this only acts on a change. */
static void update_active(struct viewport_ime *ime)
{
	struct viewport_text_input *active = find_active(ime);
	if (active == ime->active) {
		return;
	}

	if (ime->active != NULL && ime->input_method != NULL) {
		wlr_input_method_v2_send_deactivate(ime->input_method);
		wlr_input_method_v2_send_done(ime->input_method);
	}

	ime->active = active;

	if (active != NULL && ime->input_method != NULL) {
		wlr_input_method_v2_send_activate(ime->input_method);
		send_state_to_input_method(ime, active);
	}
}

static void handle_text_input_enable(struct wl_listener *listener, void *data)
{
	struct viewport_text_input *text_input =
		wl_container_of(listener, text_input, enable);
	update_active(text_input->ime);
}

static void handle_text_input_commit(struct wl_listener *listener, void *data)
{
	struct viewport_text_input *text_input =
		wl_container_of(listener, text_input, commit);
	struct viewport_ime *ime = text_input->ime;

	update_active(ime);
	if (ime->active == text_input) {
		send_state_to_input_method(ime, text_input);
	}
}

static void handle_text_input_disable(struct wl_listener *listener, void *data)
{
	struct viewport_text_input *text_input =
		wl_container_of(listener, text_input, disable);
	update_active(text_input->ime);
}

static void handle_text_input_destroy(struct wl_listener *listener, void *data)
{
	struct viewport_text_input *text_input =
		wl_container_of(listener, text_input, destroy);
	struct viewport_ime *ime = text_input->ime;

	wl_list_remove(&text_input->enable.link);
	wl_list_remove(&text_input->commit.link);
	wl_list_remove(&text_input->disable.link);
	wl_list_remove(&text_input->destroy.link);
	wl_list_remove(&text_input->link);

	if (ime->active == text_input) {
		/* Cleared first: update_active() must not read the entry being freed. */
		ime->active = NULL;
		if (ime->input_method != NULL) {
			wlr_input_method_v2_send_deactivate(ime->input_method);
			wlr_input_method_v2_send_done(ime->input_method);
		}
	}

	free(text_input);
	update_active(ime);
}

static void handle_new_text_input(struct wl_listener *listener, void *data)
{
	struct viewport_ime *ime =
		wl_container_of(listener, ime, new_text_input);
	struct wlr_text_input_v3 *input = data;

	if (input->seat != ime->server->seat) {
		return;
	}

	struct viewport_text_input *text_input = calloc(1, sizeof(*text_input));
	if (text_input == NULL) {
		return;
	}
	text_input->ime = ime;
	text_input->input = input;
	wl_list_insert(&ime->text_inputs, &text_input->link);

	text_input->enable.notify = handle_text_input_enable;
	wl_signal_add(&input->events.enable, &text_input->enable);
	text_input->commit.notify = handle_text_input_commit;
	wl_signal_add(&input->events.commit, &text_input->commit);
	text_input->disable.notify = handle_text_input_disable;
	wl_signal_add(&input->events.disable, &text_input->disable);
	text_input->destroy.notify = handle_text_input_destroy;
	wl_signal_add(&input->events.destroy, &text_input->destroy);

	/* A text input created while its surface already has focus never sees an
	 * enter otherwise — it would sit dead until the user clicked away and
	 * back. */
	struct wlr_surface *focused =
		ime->server->seat->keyboard_state.focused_surface;
	if (focused != NULL &&
			wl_resource_get_client(input->resource) ==
				wl_resource_get_client(focused->resource)) {
		wlr_text_input_v3_send_enter(input, focused);
	}
}

/* The input method has produced something: preedit to display, text to insert,
 * characters to delete. It goes to the one text input that is live. */
static void handle_input_method_commit(struct wl_listener *listener, void *data)
{
	struct viewport_ime *ime =
		wl_container_of(listener, ime, input_method_commit);
	struct wlr_input_method_v2 *method = ime->input_method;

	if (ime->active == NULL) {
		return;
	}
	struct wlr_text_input_v3 *input = ime->active->input;

	if (method->current.preedit.text != NULL) {
		wlr_text_input_v3_send_preedit_string(input,
			method->current.preedit.text,
			method->current.preedit.cursor_begin,
			method->current.preedit.cursor_end);
	}
	if (method->current.commit_text != NULL) {
		wlr_text_input_v3_send_commit_string(input, method->current.commit_text);
	}
	if (method->current.delete.before_length != 0 ||
			method->current.delete.after_length != 0) {
		wlr_text_input_v3_send_delete_surrounding_text(input,
			method->current.delete.before_length,
			method->current.delete.after_length);
	}
	wlr_text_input_v3_send_done(input);
}

static void handle_keyboard_grab_destroy(struct wl_listener *listener,
	void *data)
{
	struct viewport_ime *ime =
		wl_container_of(listener, ime, keyboard_grab_destroy);

	wl_list_remove(&ime->keyboard_grab_destroy.link);
	ime->keyboard_grab = NULL;
}

static void handle_grab_keyboard(struct wl_listener *listener, void *data)
{
	struct viewport_ime *ime =
		wl_container_of(listener, ime, input_method_grab_keyboard);
	struct wlr_input_method_keyboard_grab_v2 *grab = data;

	/* The grab needs a keyboard to take its keymap and repeat rate from, or the
	 * input method has no idea what the keys mean. */
	struct wlr_keyboard *keyboard = wlr_seat_get_keyboard(ime->server->seat);
	if (keyboard != NULL) {
		wlr_input_method_keyboard_grab_v2_set_keyboard(grab, keyboard);
	}

	ime->keyboard_grab = grab;
	ime->keyboard_grab_destroy.notify = handle_keyboard_grab_destroy;
	wl_signal_add(&grab->events.destroy, &ime->keyboard_grab_destroy);
}

static void handle_input_method_destroy(struct wl_listener *listener,
	void *data)
{
	struct viewport_ime *ime =
		wl_container_of(listener, ime, input_method_destroy);

	wl_list_remove(&ime->input_method_commit.link);
	wl_list_remove(&ime->input_method_new_popup.link);
	wl_list_remove(&ime->input_method_grab_keyboard.link);
	wl_list_remove(&ime->input_method_destroy.link);

	ime->input_method = NULL;
	ime->active = NULL;

	wlr_log(WLR_INFO, "input method disconnected");
}

static void handle_new_input_method(struct wl_listener *listener, void *data)
{
	struct viewport_ime *ime =
		wl_container_of(listener, ime, new_input_method);
	struct wlr_input_method_v2 *method = data;

	if (method->seat != ime->server->seat) {
		return;
	}
	if (ime->input_method != NULL) {
		/* Two input methods on one seat is not a state the protocol can
		 * express, so the newcomer is told plainly rather than left waiting for
		 * events that will never arrive. */
		wlr_input_method_v2_send_unavailable(method);
		return;
	}

	ime->input_method = method;

	ime->input_method_commit.notify = handle_input_method_commit;
	wl_signal_add(&method->events.commit, &ime->input_method_commit);
	ime->input_method_new_popup.notify = handle_new_popup;
	wl_signal_add(&method->events.new_popup_surface,
		&ime->input_method_new_popup);
	ime->input_method_grab_keyboard.notify = handle_grab_keyboard;
	wl_signal_add(&method->events.grab_keyboard,
		&ime->input_method_grab_keyboard);
	ime->input_method_destroy.notify = handle_input_method_destroy;
	wl_signal_add(&method->events.destroy, &ime->input_method_destroy);

	/* A text field may already be focused and enabled when the input method
	 * starts — fcitx5 launched after the terminal, which is the usual order. */
	ime->active = NULL;
	update_active(ime);

	wlr_log(WLR_INFO, "input method connected");
}

/* Keyboard focus moved. Text inputs are per surface, so every one belonging to
 * the client losing focus gets a leave and every one belonging to the client
 * gaining it gets an enter — the protocol requires both, and an input method
 * that never sees an enter simply does nothing. */
static void handle_focus_change(struct wl_listener *listener, void *data)
{
	struct viewport_ime *ime = wl_container_of(listener, ime, focus_change);
	struct wlr_seat_keyboard_focus_change_event *event = data;

	struct viewport_text_input *text_input;
	wl_list_for_each(text_input, &ime->text_inputs, link) {
		struct wlr_text_input_v3 *input = text_input->input;
		struct wl_client *client = wl_resource_get_client(input->resource);

		if (input->focused_surface != NULL &&
				input->focused_surface != event->new_surface) {
			wlr_text_input_v3_send_leave(input);
		}
		if (event->new_surface != NULL &&
				input->focused_surface != event->new_surface &&
				wl_resource_get_client(event->new_surface->resource) == client) {
			wlr_text_input_v3_send_enter(input, event->new_surface);
		}
	}

	update_active(ime);
}

/* ------------------------------------------------------------------------
 * Keyboard routing
 * --------------------------------------------------------------------- */

/* True when the input method has taken the keyboard and the key has been handed
 * to it instead of the application.
 *
 * Both must not receive it: an input method composing a character needs the raw
 * keys, and the application must not also see them or every keystroke is
 * duplicated alongside the composed text. */
bool viewport_ime_handle_key(struct viewport_ime *ime,
	struct wlr_keyboard *keyboard, uint32_t time_msec, uint32_t keycode,
	uint32_t state)
{
	if (ime == NULL || ime->keyboard_grab == NULL || ime->active == NULL) {
		return false;
	}

	wlr_input_method_keyboard_grab_v2_set_keyboard(ime->keyboard_grab, keyboard);
	wlr_input_method_keyboard_grab_v2_send_key(ime->keyboard_grab, time_msec,
		keycode, state);
	return true;
}

bool viewport_ime_handle_modifiers(struct viewport_ime *ime,
	struct wlr_keyboard *keyboard)
{
	if (ime == NULL || ime->keyboard_grab == NULL || ime->active == NULL) {
		return false;
	}

	wlr_input_method_keyboard_grab_v2_set_keyboard(ime->keyboard_grab, keyboard);
	wlr_input_method_keyboard_grab_v2_send_modifiers(ime->keyboard_grab,
		&keyboard->modifiers);
	return true;
}

/* ------------------------------------------------------------------------
 * Lifecycle
 * --------------------------------------------------------------------- */

struct viewport_ime *viewport_ime_create(struct viewport_server *server)
{
	struct viewport_ime *ime = calloc(1, sizeof(*ime));
	if (ime == NULL) {
		return NULL;
	}
	ime->server = server;
	wl_list_init(&ime->text_inputs);

	ime->text_input_manager = wlr_text_input_manager_v3_create(
		server->wl_display);
	ime->input_method_manager = wlr_input_method_manager_v2_create(
		server->wl_display);
	if (ime->text_input_manager == NULL || ime->input_method_manager == NULL) {
		free(ime);
		return NULL;
	}

	ime->new_text_input.notify = handle_new_text_input;
	wl_signal_add(&ime->text_input_manager->events.new_text_input,
		&ime->new_text_input);
	ime->new_input_method.notify = handle_new_input_method;
	wl_signal_add(&ime->input_method_manager->events.new_input_method,
		&ime->new_input_method);

	/* One hook covers every way focus can move — toplevels, layer surfaces, the
	 * lock screen — rather than a call at each site that changes it. */
	ime->focus_change.notify = handle_focus_change;
	wl_signal_add(&server->seat->keyboard_state.events.focus_change,
		&ime->focus_change);

	return ime;
}

void viewport_ime_destroy(struct viewport_ime *ime)
{
	if (ime == NULL) {
		return;
	}

	/* Every text input still registered holds listeners that point back here.
	 * Freeing the relay without unhooking them left those handlers running
	 * against freed memory the moment the clients were destroyed — which is
	 * immediately afterwards, in the same teardown. A terminal with a text
	 * input was enough to crash every shutdown. */
	struct viewport_text_input *text_input, *tmp;
	wl_list_for_each_safe(text_input, tmp, &ime->text_inputs, link) {
		wl_list_remove(&text_input->enable.link);
		wl_list_remove(&text_input->commit.link);
		wl_list_remove(&text_input->disable.link);
		wl_list_remove(&text_input->destroy.link);
		wl_list_remove(&text_input->link);
		free(text_input);
	}
	ime->active = NULL;

	wl_list_remove(&ime->new_text_input.link);
	wl_list_remove(&ime->new_input_method.link);
	wl_list_remove(&ime->focus_change.link);

	if (ime->input_method != NULL) {
		wl_list_remove(&ime->input_method_commit.link);
		wl_list_remove(&ime->input_method_new_popup.link);
		wl_list_remove(&ime->input_method_grab_keyboard.link);
		wl_list_remove(&ime->input_method_destroy.link);
	}
	if (ime->keyboard_grab != NULL) {
		wl_list_remove(&ime->keyboard_grab_destroy.link);
	}

	free(ime);
}
