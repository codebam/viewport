/* SPDX-License-Identifier: MIT */
#define _POSIX_C_SOURCE 200809L

#include <math.h>
#include <stdlib.h>
#include <time.h>

#include <wlr/types/wlr_compositor.h>
#include <wlr/types/wlr_fractional_scale_v1.h>
#include <wlr/types/wlr_output.h>
#include <wlr/util/log.h>

#include "viewport.h"

static void handle_output_frame(struct wl_listener *listener, void *data)
{
	struct viewport_output *output = wl_container_of(listener, output, frame);
	struct viewport_server *server = output->server;

	/* Re-apply the overview's scaling before compositing.
	 *
	 * wlr_scene_surface recomputes a buffer's destination size from its surface
	 * whenever that surface commits, which undoes any scale set from outside.
	 * Doing it per commit is not enough either: a client painting through
	 * subsurfaces — Firefox does — commits on surfaces the toplevel has no
	 * listener for, so its content stayed stubbornly full size while simpler
	 * windows shrank. Once a frame catches every case, costs nothing while no
	 * window is scaled, and the iterator only writes when the value differs so
	 * this does not damage the scene on its own. */
	if (server->overview) {
		struct viewport_toplevel *toplevel;
		wl_list_for_each(toplevel, &server->toplevels, link) {
			viewport_toplevel_apply_crop(toplevel);
		}
	}

	/* One call composites everything: the shell's dma-buf underneath, each
	 * client's dma-buf in the rect the shell asked for. The scene picks damage
	 * regions, decides whether a surface can be scanned out directly, and
	 * threads explicit-sync timeline points through. No pixel is read back. */
	bool committed = wlr_scene_output_commit(output->scene_output, NULL);

	static int logged;
	if (server->config.trace && logged < 40) {
		logged++;
		wlr_log(WLR_DEBUG, "output %s frame: committed=%d",
			output->wlr_output->name, committed);
	}


	if (committed) {
		struct timespec now;
		clock_gettime(CLOCK_MONOTONIC, &now);
		wlr_scene_output_send_frame_done(output->scene_output, &now);
	}

	/* Acknowledge WebKit's frame even when the scene had nothing to repaint.
	 *
	 * wlr_scene_output_commit() returns false when there is no damage, and
	 * returning early there deadlocks the shell: WebKit will not paint frame
	 * N+1 until N is acknowledged, and nothing will ever damage the scene
	 * because the only thing that would have is the shell painting. The
	 * symptom is a shell frozen on its first frame until some unrelated input
	 * — moving the mouse — damages the scene and breaks the cycle.
	 *
	 * The frame is on screen either way, so acknowledging is correct. */
	if (server->web != NULL) {
		viewport_web_notify_presented(server->web);
	}
}

static void handle_output_request_state(struct wl_listener *listener,
	void *data)
{
	struct viewport_output *output =
		wl_container_of(listener, output, request_state);
	const struct wlr_output_event_request_state *event = data;

	if (wlr_output_commit_state(output->wlr_output, event->state)) {
		viewport_layers_arrange(output);
		int width, height;
		viewport_layout_size(output->server, &width, &height);
		if (output->server->web != NULL) {
			viewport_web_resize(output->server->web, width, height);
		}
		viewport_ipc_notify_output_layout(output->server);
	}
}

static void handle_output_destroy(struct wl_listener *listener, void *data)
{
	struct viewport_output *output = wl_container_of(listener, output, destroy);

	struct viewport_server *server = output->server;

	wl_list_remove(&output->frame.link);
	wl_list_remove(&output->request_state.link);
	wl_list_remove(&output->destroy.link);
	wl_list_remove(&output->link);

	/* Not while shutting down.
	 *
	 * Destroying the backend destroys every output, which lands here, which
	 * republishes the output list, which asks each output whether it can do
	 * HDR — and that reads the renderer, destroyed moments earlier in the same
	 * function. A use-after-free on every clean exit, corrupting the heap on
	 * the way out.
	 *
	 * There is nothing to publish anyway: the shell is already gone and a
	 * monitor vanishing because the compositor is exiting is not news. */
	if (!server->shutting_down) {
		/* The layout is smaller than it was, and the shell's buffer is still
		 * the old size — every other path that changes the layout resizes it,
		 * and this one did not, so unplugging a monitor left the shell drawing
		 * into a viewport larger than the screens that remained. */
		int width, height;
		viewport_layout_size(server, &width, &height);
		if (server->web != NULL) {
			viewport_web_resize(server->web, width, height);
		}
		viewport_ipc_notify_output_layout(server);
		/* And the lock backdrop has to shrink with it, for the same reason it
		 * has to grow when a monitor appears. */
		viewport_session_lock_outputs_changed(server);
	}
	free(output);
}

void viewport_handle_new_output(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, new_output);
	struct wlr_output *wlr_output = data;

	if (!wlr_output_init_render(wlr_output, server->allocator,
			server->renderer)) {
		wlr_log(WLR_ERROR, "wlr_output_init_render failed for %s",
			wlr_output->name);
		return;
	}

	struct wlr_output_state state;
	wlr_output_state_init(&state);
	wlr_output_state_set_enabled(&state, true);

	/* What the display says it prefers, unless the config says otherwise.
	 *
	 * Preferred is the right default: it is the timing the manufacturer chose,
	 * and second-guessing it for every display to suit one of them is how a
	 * compositor ends up with a mode nobody asked for on hardware nobody
	 * tested. But it is not always the fastest — plenty of high-refresh
	 * monitors nominate a 60Hz timing — so the config can ask for the fastest
	 * instead, or name a mode outright. */
	struct wlr_output_mode *mode = wlr_output_preferred_mode(wlr_output);
	const struct viewport_output_config *rule =
		viewport_output_config_for(server, wlr_output->name);

	if (rule != NULL && rule->width > 0 && rule->height > 0) {
		/* A named mode. Matched on refresh too when one was given, and on
		 * resolution alone when it was not. */
		struct wlr_output_mode *candidate;
		wl_list_for_each(candidate, &wlr_output->modes, link) {
			if (candidate->width != rule->width ||
					candidate->height != rule->height) {
				continue;
			}
			if (rule->refresh > 0 && candidate->refresh != rule->refresh) {
				continue;
			}
			mode = candidate;
			break;
		}
	} else if (rule != NULL && rule->max_refresh && mode != NULL) {
		/* Fastest at the preferred resolution. Only the refresh rate is
		 * maximised: the highest refresh overall may belong to a lower
		 * resolution, and a sharper picture is worth more than a faster one. */
		struct wlr_output_mode *candidate;
		wl_list_for_each(candidate, &wlr_output->modes, link) {
			if (candidate->width == mode->width &&
					candidate->height == mode->height &&
					candidate->refresh > mode->refresh) {
				mode = candidate;
			}
		}
	}

	if (mode != NULL) {
		wlr_output_state_set_mode(&state, mode);
	}

	/* Variable refresh rate, when asked for and when the hardware will take it.
	 * Tested separately rather than folded into the commit below: a monitor or
	 * driver that cannot do it would otherwise take the mode down with it, and
	 * a black screen is a poor trade for a smoother one. */
	if (server->config.adaptive_sync) {
		wlr_output_state_set_adaptive_sync_enabled(&state, true);
		if (!wlr_output_test_state(wlr_output, &state)) {
			wlr_log(WLR_INFO, "%s will not do adaptive sync; leaving it off",
				wlr_output->name);
			wlr_output_state_set_adaptive_sync_enabled(&state, false);
		}
	}

	bool ok = wlr_output_commit_state(wlr_output, &state);
	wlr_output_state_finish(&state);
	if (!ok) {
		wlr_log(WLR_ERROR, "initial commit failed for %s", wlr_output->name);
		return;
	}

	if (server->config.adaptive_sync) {
		wlr_log(WLR_INFO, "%s adaptive sync %s", wlr_output->name,
			wlr_output->adaptive_sync_status == WLR_OUTPUT_ADAPTIVE_SYNC_ENABLED
				? "on" : "off");
	}

	struct viewport_output *output = calloc(1, sizeof(*output));
	if (output == NULL) {
		return;
	}
	output->server = server;
	output->wlr_output = wlr_output;
	/* layer_shell.c resolves a wlr_output back to ours through this. */
	wlr_output->data = output;

	output->frame.notify = handle_output_frame;
	wl_signal_add(&wlr_output->events.frame, &output->frame);
	output->request_state.notify = handle_output_request_state;
	wl_signal_add(&wlr_output->events.request_state, &output->request_state);
	output->destroy.notify = handle_output_destroy;
	wl_signal_add(&wlr_output->events.destroy, &output->destroy);

	wl_list_insert(&server->outputs, &output->link);

	/* Auto-arrange left to right in connection order. A real deployment would
	 * take this from the shell over IPC; the shell already learns the result
	 * via the output.layout event below. */
	struct wlr_output_layout_output *layout_output =
		wlr_output_layout_add_auto(server->output_layout, wlr_output);
	output->scene_output = wlr_scene_output_create(server->scene, wlr_output);
	wlr_scene_output_layout_add_output(server->scene_layout, layout_output,
		output->scene_output);

	/* Until a panel claims anything the whole output is usable. Set before the
	 * first IPC notify so the shell never sees a zero-sized usable area. */
	wlr_output_layout_get_box(server->output_layout, wlr_output,
		&output->usable_area);

	int width, height;
	viewport_layout_size(server, &width, &height);
	if (server->web != NULL) {
		viewport_web_resize(server->web, width, height);
	}
	viewport_ipc_notify_output_layout(server);
	viewport_output_manager_update(server);
	/* A screen appearing while the session is locked must not show what is
	 * behind the lock while the locker works out that it exists. */
	viewport_session_lock_outputs_changed(server);

	/* A frame may already be pending from before this output existed; without
	 * a scheduled frame nothing would ever acknowledge it. */
	wlr_output_schedule_frame(wlr_output);

	/* The refresh rate is part of what mode was chosen, and leaving it out of
	 * the line is how a monitor can sit at 60Hz unnoticed. */
	wlr_log(WLR_INFO, "output %s online at %dx%d@%.3fHz", wlr_output->name,
		wlr_output->width, wlr_output->height,
		wlr_output->refresh / 1000.0);
}

/* Tell a surface what scale to paint at.
 *
 * A client renders at whatever scale it is told and the compositor stretches
 * whatever it gets, so saying nothing means every client paints at 1x and is
 * scaled up on a HiDPI screen — sharp text becomes soft text. Two protocols
 * carry it: fractional-scale-v1 for the exact value, and the wl_surface
 * preferred buffer scale for clients that only understand whole numbers, which
 * has to be rounded up so they overshoot rather than blur.
 *
 * The scale is the largest of the outputs the surface is actually on, so a
 * window straddling a 1x and a 2x monitor is sharp on the better one rather
 * than soft on it. */
void viewport_surface_update_scale(struct viewport_server *server,
	struct wlr_surface *surface)
{
	if (surface == NULL) {
		return;
	}

	double scale = 0.0;
	struct viewport_output *output;
	wl_list_for_each(output, &server->outputs, link) {
		if (output->wlr_output->scale > scale) {
			scale = output->wlr_output->scale;
		}
	}
	if (scale <= 0.0) {
		return;
	}

	wlr_fractional_scale_v1_notify_scale(surface, scale);
	wlr_surface_set_preferred_buffer_scale(surface, (int32_t)ceil(scale));
}

void viewport_layout_size(struct viewport_server *server, int *width,
	int *height)
{
	struct wlr_box box;
	wlr_output_layout_get_box(server->output_layout, NULL, &box);

	/* Before any output is attached the layout is empty; hand back a sane
	 * size so WebKit has something to lay out against. */
	*width = box.width > 0 ? box.width : 1920;
	*height = box.height > 0 ? box.height : 1080;
}
