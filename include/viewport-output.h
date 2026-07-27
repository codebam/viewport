/* SPDX-License-Identifier: MIT
 *
 * Outputs.
 *
 * Monitors and what is configurable about them — modes, layout, HDR, the
 revert timer that protects a user from a mode their screen cannot show — and
 session locking, which is per output because a lock that misses one leaves
 the desktop visible on it.
 *
 * Split out of viewport.h, which had grown to declare every function in the
 * project: a comment edited in it recompiled all twenty-nine translation
 * units, and nothing recorded which of them any given declaration was for.
 * The types stay there — struct viewport_server embeds them by value, so
 * everything needs it — and only the interfaces moved.
 */
#ifndef VIEWPORT_OUTPUT_H
#define VIEWPORT_OUTPUT_H

#include "viewport.h"

/* -------------------------------------------------------------------------
 * output.c
 * ---------------------------------------------------------------------- */

void viewport_handle_new_output(struct wl_listener *listener, void *data);

/* Total layout extent, used to size the shell's viewport. */
/* Tells a surface which scale to render at, over both scale protocols. */
void viewport_surface_update_scale(struct viewport_server *server,
	struct wlr_surface *surface);

void viewport_layout_size(struct viewport_server *server,
	int *width, int *height);

/* Display configuration, for wlr-randr and kanshi. */
void viewport_output_manager_init(struct viewport_server *server);
void viewport_output_manager_update(struct viewport_server *server);
void viewport_output_revert_cancel(struct viewport_server *server);

/* HDR, per output. */
void viewport_hdr_init(struct viewport_server *server);
bool viewport_output_hdr_capable(struct viewport_output *output);
bool viewport_output_set_hdr(struct viewport_output *output, bool enabled);

/* Drive the hotplug path from a test. Both refuse unless --headless. */
bool viewport_output_test_add(struct viewport_server *server);
bool viewport_output_test_remove(struct viewport_server *server,
	const char *name);

/* -------------------------------------------------------------------------
 * session_lock.c
 *
 * ext-session-lock-v1. Without it nothing can lock the screen: swaylock and
 * every other locker exits immediately when the protocol is absent.
 * ---------------------------------------------------------------------- */

void viewport_session_lock_init(struct viewport_server *server);
/* Keeps the lock backdrop covering every output when the layout changes. */
void viewport_session_lock_outputs_changed(struct viewport_server *server);

#endif /* VIEWPORT_OUTPUT_H */
