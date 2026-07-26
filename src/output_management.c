/* SPDX-License-Identifier: MIT
 *
 * Letting clients configure the displays.
 *
 * Outputs are auto-arranged left to right in connection order, which is a
 * guess. wlr-output-management-v1 is how that guess gets corrected: it is the
 * protocol behind `wlr-randr`, `kanshi` and every graphical display settings
 * panel. Without it there is no way at all to change a resolution, refresh
 * rate, scale or monitor position from inside the session — the arrangement is
 * whatever the compositor decided at startup.
 *
 * The protocol has a strict shape. A client is handed the current
 * configuration with a serial, builds a new one from it, and asks to either
 * *test* it or *apply* it. Exactly one of send_succeeded or send_failed must
 * come back, and any change to the real outputs invalidates outstanding
 * serials — so the current state is republished after every successful apply
 * and whenever an output appears or disappears.
 */
#define _POSIX_C_SOURCE 200809L

#include <stdlib.h>

#include <wlr/types/wlr_output.h>
#include <wlr/types/wlr_output_layout.h>
#include <wlr/types/wlr_output_management_v1.h>
#include <wlr/types/wlr_scene.h>
#include <wlr/util/log.h>

#include "viewport.h"

/* Translate one head's requested state into an output state. The position is
 * not part of it: that lives in the output layout, and is applied separately
 * once the mode is known to work. */
static void build_output_state(struct wlr_output_configuration_head_v1 *head,
	struct wlr_output_state *state)
{
	wlr_output_state_init(state);
	wlr_output_state_set_enabled(state, head->state.enabled);

	if (!head->state.enabled) {
		return;
	}

	if (head->state.mode != NULL) {
		wlr_output_state_set_mode(state, head->state.mode);
	} else if (head->state.custom_mode.width > 0 &&
			head->state.custom_mode.height > 0) {
		wlr_output_state_set_custom_mode(state, head->state.custom_mode.width,
			head->state.custom_mode.height, head->state.custom_mode.refresh);
	}

	wlr_output_state_set_transform(state, head->state.transform);
	wlr_output_state_set_scale(state, head->state.scale);
	wlr_output_state_set_adaptive_sync_enabled(state,
		head->state.adaptive_sync_enabled);
}

/* Test alone cannot tell a client the whole truth — a configuration may be
 * rejected only once every head is committed together — but it catches the
 * common case of an unsupported mode or scale before anything on screen
 * changes. */
static bool test_configuration(struct wlr_output_configuration_v1 *config)
{
	struct wlr_output_configuration_head_v1 *head;
	wl_list_for_each(head, &config->heads, link) {
		struct wlr_output_state state;
		build_output_state(head, &state);
		bool ok = wlr_output_test_state(head->state.output, &state);
		wlr_output_state_finish(&state);
		if (!ok) {
			return false;
		}
	}
	return true;
}

static bool apply_configuration(struct viewport_server *server,
	struct wlr_output_configuration_v1 *config)
{
	/* Test everything first. A half-applied configuration — one monitor at a
	 * new resolution, the next refusing — is worse than a rejected one, and the
	 * client has no way to undo it. */
	if (!test_configuration(config)) {
		return false;
	}

	struct wlr_output_configuration_head_v1 *head;
	wl_list_for_each(head, &config->heads, link) {
		struct wlr_output *wlr_output = head->state.output;

		struct wlr_output_state state;
		build_output_state(head, &state);
		bool ok = wlr_output_commit_state(wlr_output, &state);
		wlr_output_state_finish(&state);
		if (!ok) {
			wlr_log(WLR_ERROR, "output %s rejected its configuration",
				wlr_output->name);
			return false;
		}

		/* A disabled output leaves the layout entirely, or it would keep
		 * occupying space that windows could be placed into. */
		if (!head->state.enabled) {
			wlr_output_layout_remove(server->output_layout, wlr_output);
			continue;
		}

		struct wlr_output_layout_output *layout_output =
			wlr_output_layout_add(server->output_layout, wlr_output,
				head->state.x, head->state.y);
		if (layout_output == NULL) {
			return false;
		}

		struct viewport_output *output = wlr_output->data;
		if (output != NULL && output->scene_output != NULL) {
			wlr_scene_output_layout_add_output(server->scene_layout,
				layout_output, output->scene_output);
		}
	}

	/* The shell lays out against the whole layout, so a changed arrangement
	 * means a new web view size and a fresh output list. */
	int width, height;
	viewport_layout_size(server, &width, &height);
	if (server->web != NULL) {
		viewport_web_resize(server->web, width, height);
	}
	viewport_ipc_notify_output_layout(server);

	struct viewport_output *output;
	wl_list_for_each(output, &server->outputs, link) {
		viewport_layers_arrange(output);
	}

	return true;
}

static void handle_apply(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, output_manager_apply);
	struct wlr_output_configuration_v1 *config = data;

	if (apply_configuration(server, config)) {
		wlr_output_configuration_v1_send_succeeded(config);
	} else {
		wlr_output_configuration_v1_send_failed(config);
	}
	wlr_output_configuration_v1_destroy(config);

	/* Republish regardless of the outcome: on success the client's serial is
	 * stale, and on failure it may not match what the outputs actually kept. */
	viewport_output_manager_update(server);
}

static void handle_test(struct wl_listener *listener, void *data)
{
	struct wlr_output_configuration_v1 *config = data;

	if (test_configuration(config)) {
		wlr_output_configuration_v1_send_succeeded(config);
	} else {
		wlr_output_configuration_v1_send_failed(config);
	}
	wlr_output_configuration_v1_destroy(config);
}

/* Publish what the outputs are actually doing right now. Called after every
 * change, because a client's configuration serial is only valid against the
 * state it was handed. */
void viewport_output_manager_update(struct viewport_server *server)
{
	if (server->output_manager == NULL) {
		return;
	}

	struct wlr_output_configuration_v1 *config =
		wlr_output_configuration_v1_create();
	if (config == NULL) {
		return;
	}

	struct viewport_output *output;
	wl_list_for_each(output, &server->outputs, link) {
		struct wlr_output_configuration_head_v1 *head =
			wlr_output_configuration_head_v1_create(config, output->wlr_output);
		if (head == NULL) {
			continue;
		}

		/* head_v1_create copies the output's own state, which does not include
		 * where it sits in the layout. */
		struct wlr_box box;
		wlr_output_layout_get_box(server->output_layout, output->wlr_output,
			&box);
		head->state.x = box.x;
		head->state.y = box.y;
	}

	wlr_output_manager_v1_set_configuration(server->output_manager, config);
}

void viewport_output_manager_init(struct viewport_server *server)
{
	server->output_manager = wlr_output_manager_v1_create(server->wl_display);
	if (server->output_manager == NULL) {
		wlr_log(WLR_ERROR, "output management unavailable; displays are fixed");
		return;
	}

	server->output_manager_apply.notify = handle_apply;
	wl_signal_add(&server->output_manager->events.apply,
		&server->output_manager_apply);
	server->output_manager_test.notify = handle_test;
	wl_signal_add(&server->output_manager->events.test,
		&server->output_manager_test);
}
