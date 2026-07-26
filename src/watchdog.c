/* SPDX-License-Identifier: MIT
 *
 * What happens when the shell stops answering.
 *
 * The entire layout lives in a web page. That is the point of this compositor,
 * and it is also its one structural risk: a JavaScript error, a page that fails
 * to load, a shell served from a machine that has gone away — any of them and
 * no window is ever placed. Windows stay hidden, nothing recovers, and the
 * session looks like a black screen with a working keyboard.
 *
 * So placement is watched. A window that maps and is not given a rect within a
 * couple of seconds is laid out here instead, by a deliberately stupid tiler:
 * every visible window gets an equal column of the output. It is not a fallback
 * layout anyone would want to use, and it is not meant to be — it exists so
 * that a broken shell leaves a usable desktop rather than an unusable one,
 * long enough to open a terminal and fix it.
 *
 * The moment the shell does answer, the watchdog is disarmed and never fires
 * again for that window. A shell that is merely slow to start therefore costs
 * nothing.
 */
#define _POSIX_C_SOURCE 200809L

#include <stdlib.h>

#include <glib.h>

#include <wlr/types/wlr_output_layout.h>
#include <wlr/util/log.h>

#include "viewport.h"

/* Long enough that a shell fetching over the network is not cut off, short
 * enough that a broken one is not left on screen doing nothing. */
#define WATCHDOG_MS 2500

/* Lay every placed-nowhere window out in equal columns on the first output.
 *
 * Deliberately ignores workspaces, the tiling tree and everything else the
 * shell owns: none of that state is trustworthy at this point, since the thing
 * that maintains it is what stopped responding. */
static void place_everything(struct viewport_server *server)
{
	struct wlr_box area;
	wlr_output_layout_get_box(server->output_layout, NULL, &area);
	if (area.width <= 0 || area.height <= 0) {
		return;
	}

	int count = 0;
	struct viewport_toplevel *toplevel;
	wl_list_for_each(toplevel, &server->toplevels, link) {
		if (toplevel->mapped) {
			count++;
		}
	}
	if (count == 0) {
		return;
	}

	int width = area.width / count;
	int index = 0;
	wl_list_for_each(toplevel, &server->toplevels, link) {
		if (!toplevel->mapped) {
			continue;
		}
		struct wlr_box box = {
			.x = area.x + index * width,
			.y = area.y,
			.width = width,
			.height = area.height,
		};
		toplevel->has_clip = false;
		viewport_toplevel_set_box(toplevel, &box);
		index++;
	}
}

static gboolean watchdog_fire(gpointer data)
{
	struct viewport_toplevel *toplevel = data;
	struct viewport_server *server = toplevel->server;

	toplevel->watchdog = 0;

	if (toplevel->has_box) {
		return G_SOURCE_REMOVE;
	}

	wlr_log(WLR_ERROR,
		"shell did not place view %u within %dms; falling back to a built-in "
		"layout. The shell is broken or unreachable.",
		toplevel->id, WATCHDOG_MS);

	place_everything(server);
	return G_SOURCE_REMOVE;
}

void viewport_watchdog_arm(struct viewport_toplevel *toplevel)
{
	if (toplevel->watchdog != 0 || toplevel->has_box) {
		return;
	}
	toplevel->watchdog = g_timeout_add(WATCHDOG_MS, watchdog_fire, toplevel);
}

void viewport_watchdog_disarm(struct viewport_toplevel *toplevel)
{
	if (toplevel->watchdog != 0) {
		g_source_remove(toplevel->watchdog);
		toplevel->watchdog = 0;
	}
}
