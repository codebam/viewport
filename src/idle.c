/* SPDX-License-Identifier: MIT
 *
 * Doing something when nobody is there.
 *
 * The compositor already tells clients when the seat goes idle — that is
 * idle-notify, and it is what an external daemon like swayidle listens to. But
 * advertising it is not a policy: with nothing running, the screens stay lit
 * for ever and the session never locks. This is the policy, so that a machine
 * left alone behaves without a daemon needing to be installed and configured
 * alongside it.
 *
 * Two things happen, at two thresholds. The session locks, by running whatever
 * locker the config names — the compositor does not lock the screen itself,
 * because ext-session-lock exists precisely so that the thing drawing the lock
 * screen can be a separate program that can crash without unlocking anything.
 * Then the outputs are turned off, which is what actually saves the panel and
 * the power.
 *
 * Idle inhibitors are honoured. A video player holding one is saying the user
 * is present and simply not typing, which is exactly the case a timer cannot
 * see for itself.
 */
#define _POSIX_C_SOURCE 200809L

#include <stdlib.h>

#include <glib.h>

#include <wlr/types/wlr_idle_inhibit_v1.h>
#include <wlr/types/wlr_idle_notify_v1.h>
#include <wlr/types/wlr_output.h>
#include <wlr/types/wlr_output_power_management_v1.h>
#include <wlr/util/log.h>

#include "viewport.h"

/* How often the thresholds are checked. Idle timeouts are measured in minutes,
 * so a coarse tick costs nothing and keeps the compositor asleep between
 * checks rather than waking to compare timestamps. */
#define IDLE_TICK_SECONDS 5

static bool inhibited(struct viewport_server *server)
{
	if (server->idle_inhibit == NULL) {
		return false;
	}

	struct wlr_idle_inhibitor_v1 *inhibitor;
	wl_list_for_each(inhibitor, &server->idle_inhibit->inhibitors, link) {
		/* An inhibitor only counts while its surface is actually being shown;
		 * a paused video on a hidden workspace is not keeping anyone awake. */
		if (inhibitor->surface != NULL && inhibitor->surface->mapped) {
			return true;
		}
	}
	return false;
}

static void set_outputs_enabled(struct viewport_server *server, bool enabled)
{
	struct viewport_output *output;
	wl_list_for_each(output, &server->outputs, link) {
		if (output->wlr_output->enabled == enabled) {
			continue;
		}

		struct wlr_output_state state;
		wlr_output_state_init(&state);
		wlr_output_state_set_enabled(&state, enabled);
		wlr_output_commit_state(output->wlr_output, &state);
		wlr_output_state_finish(&state);
	}

	wlr_log(WLR_INFO, "outputs %s", enabled ? "on" : "off");
}

static gboolean idle_tick(gpointer data)
{
	struct viewport_server *server = data;
	const struct viewport_config *config = &server->config;

	if (inhibited(server)) {
		/* Treat an inhibitor as activity, so the countdown starts again from
		 * when it is released rather than firing the instant it is. */
		server->idle_since = g_get_monotonic_time();
		return G_SOURCE_CONTINUE;
	}

	gint64 idle_seconds =
		(g_get_monotonic_time() - server->idle_since) / G_USEC_PER_SEC;

	if (config->idle_lock_after > 0 && !server->idle_locked &&
			idle_seconds >= config->idle_lock_after) {
		server->idle_locked = true;
		if (config->idle_lock_command != NULL) {
			wlr_log(WLR_INFO, "idle for %lds; locking",
				(long)idle_seconds);
			viewport_spawn(config->idle_lock_command);
		}
	}

	if (config->idle_blank_after > 0 && !server->idle_blanked &&
			idle_seconds >= config->idle_blank_after) {
		server->idle_blanked = true;
		wlr_log(WLR_INFO, "idle for %lds; blanking", (long)idle_seconds);
		set_outputs_enabled(server, false);
	}

	return G_SOURCE_CONTINUE;
}

void viewport_idle_blank(struct viewport_server *server)
{
	/* Flagged as though the timer had done it, so the next keypress or mouse
	 * movement brings the screens back through the same path. Blanking without
	 * that flag would leave no way to undo it short of a timer that has already
	 * fired. */
	server->idle_blanked = true;
	set_outputs_enabled(server, false);
}

/* Any input at all. Called from the key, pointer and touch paths rather than
 * from a single place, because there is no single place — the seat sees these
 * separately and an idle timer that only noticed the keyboard would blank the
 * screen out from under someone using the mouse. */
void viewport_idle_activity(struct viewport_server *server)
{
	server->idle_since = g_get_monotonic_time();

	if (server->idle_blanked) {
		server->idle_blanked = false;
		set_outputs_enabled(server, true);
	}
	/* The lock is not undone here: dismissing it is the locker's business, and
	 * moving the mouse must not unlock anything. Re-arming it once the session
	 * is unlocked again is enough. */
	if (server->idle_locked && !server->locked) {
		server->idle_locked = false;
	}

	if (server->idle_notifier != NULL) {
		wlr_idle_notifier_v1_notify_activity(server->idle_notifier,
			server->seat);
	}
}

/* A client asking to turn a monitor off — wlopm, or a settings panel. Handled
 * so that the same thing the idle timer does is available on request. */
static void handle_output_power_mode(struct wl_listener *listener, void *data)
{
	struct wlr_output_power_v1_set_mode_event *event = data;

	struct wlr_output_state state;
	wlr_output_state_init(&state);
	wlr_output_state_set_enabled(&state,
		event->mode == ZWLR_OUTPUT_POWER_V1_MODE_ON);
	wlr_output_commit_state(event->output, &state);
	wlr_output_state_finish(&state);
}

void viewport_idle_init(struct viewport_server *server)
{
	server->output_power = wlr_output_power_manager_v1_create(
		server->wl_display);
	if (server->output_power != NULL) {
		server->output_power_mode.notify = handle_output_power_mode;
		wl_signal_add(&server->output_power->events.set_mode,
			&server->output_power_mode);
	}

	server->idle_since = g_get_monotonic_time();

	/* No thresholds configured means no policy — the protocol is still
	 * advertised, so an external daemon can own this instead. */
	if (server->config.idle_lock_after <= 0 &&
			server->config.idle_blank_after <= 0) {
		return;
	}

	server->idle_timer = g_timeout_add_seconds(IDLE_TICK_SECONDS, idle_tick,
		server);
	wlr_log(WLR_INFO, "idle: lock after %ds, blank after %ds",
		server->config.idle_lock_after, server->config.idle_blank_after);
}

void viewport_idle_finish(struct viewport_server *server)
{
	if (server->idle_timer != 0) {
		g_source_remove(server->idle_timer);
		server->idle_timer = 0;
	}
	if (server->output_power != NULL) {
		wl_list_remove(&server->output_power_mode.link);
	}
}
