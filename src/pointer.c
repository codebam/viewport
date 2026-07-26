/* SPDX-License-Identifier: MIT
 *
 * Pointer capture, for games.
 *
 * A first-person game needs two things a desktop pointer does not provide:
 *
 *   relative motion  Mouselook is driven by how far the mouse moved, not where
 *                    the cursor ended up. An absolute position saturates the
 *                    moment the cursor reaches a screen edge, which is why a
 *                    game without this can only turn so far before stopping.
 *
 *   a constraint     The cursor must stop moving — locked in place, or confined
 *                    to a region — so it neither escapes onto the other monitor
 *                    mid-fight nor generates absolute motion the game would
 *                    misread.
 *
 * Both are separate Wayland protocols, and the two work together: while a lock
 * is active the compositor stops moving its cursor entirely and the client is
 * driven purely by relative deltas.
 *
 * This also covers X11 games under Xwayland. XGrabPointer has no direct
 * equivalent here; Xwayland implements it by taking out exactly these two
 * protocols on the client's behalf, so CS2 through Proton and a native Wayland
 * game arrive at the same code.
 */
#define _POSIX_C_SOURCE 200809L

#include <stdlib.h>

#include <wlr/types/wlr_pointer_constraints_v1.h>
#include <wlr/types/wlr_relative_pointer_v1.h>
#include <wlr/types/wlr_seat.h>
#include <wlr/util/log.h>

#include "viewport.h"

static void constraint_destroy(struct wl_listener *listener, void *data)
{
	struct viewport_constraint *constraint =
		wl_container_of(listener, constraint, destroy);
	struct viewport_server *server = constraint->server;

	if (server->active_constraint == constraint->constraint) {
		viewport_pointer_deactivate_constraint(server);
	}

	wl_list_remove(&constraint->destroy.link);
	free(constraint);
}

void viewport_handle_new_constraint(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, new_constraint);
	struct wlr_pointer_constraint_v1 *wlr_constraint = data;

	struct viewport_constraint *constraint = calloc(1, sizeof(*constraint));
	if (constraint == NULL) {
		return;
	}
	constraint->server = server;
	constraint->constraint = wlr_constraint;

	constraint->destroy.notify = constraint_destroy;
	wl_signal_add(&wlr_constraint->events.destroy, &constraint->destroy);

	/* A client may request a constraint long before it has pointer focus —
	 * at startup, or while another window is focused. It only takes effect
	 * once the pointer is actually over it. */
	if (server->seat->pointer_state.focused_surface == wlr_constraint->surface) {
		viewport_pointer_apply_constraint(server, wlr_constraint);
	}
}

void viewport_pointer_apply_constraint(struct viewport_server *server,
	struct wlr_pointer_constraint_v1 *constraint)
{
	if (server->active_constraint == constraint) {
		return;
	}

	if (server->active_constraint != NULL) {
		viewport_pointer_deactivate_constraint(server);
	}

	server->active_constraint = constraint;
	wlr_pointer_constraint_v1_send_activated(constraint);

	wlr_log(WLR_DEBUG, "pointer %s",
		constraint->type == WLR_POINTER_CONSTRAINT_V1_LOCKED
			? "locked" : "confined");
}

void viewport_pointer_deactivate_constraint(struct viewport_server *server)
{
	struct wlr_pointer_constraint_v1 *constraint = server->active_constraint;
	if (constraint == NULL) {
		return;
	}
	server->active_constraint = NULL;

	/* A client may name where the cursor should reappear when the grab ends —
	 * a game usually wants it back under the crosshair rather than wherever it
	 * happened to be when the lock started. */
	if (constraint->current.committed &
			WLR_POINTER_CONSTRAINT_V1_STATE_CURSOR_HINT) {
		struct wlr_surface *surface = constraint->surface;
		struct viewport_toplevel *toplevel;
		wl_list_for_each(toplevel, &server->toplevels, link) {
			if (viewport_view_surface(toplevel) != surface) {
				continue;
			}
			wlr_cursor_warp(server->cursor, NULL,
				toplevel->box.x + constraint->current.cursor_hint.x,
				toplevel->box.y + constraint->current.cursor_hint.y);
			break;
		}
	}

	wlr_pointer_constraint_v1_send_deactivated(constraint);
	wlr_log(WLR_DEBUG, "pointer released");
}

/* Called whenever pointer focus changes: a constraint belongs to a surface, so
 * it activates when that surface gains the pointer and drops when it loses it.
 * Without this a game would keep the mouse captured after the user focused
 * something else. */
void viewport_pointer_check_constraint(struct viewport_server *server,
	struct wlr_surface *surface)
{
	if (server->pointer_constraints == NULL) {
		return;
	}

	if (server->active_constraint != NULL &&
			server->active_constraint->surface == surface) {
		return;
	}

	if (server->active_constraint != NULL) {
		viewport_pointer_deactivate_constraint(server);
	}

	if (surface == NULL) {
		return;
	}

	struct wlr_pointer_constraint_v1 *constraint =
		wlr_pointer_constraints_v1_constraint_for_surface(
			server->pointer_constraints, surface, server->seat);
	if (constraint != NULL) {
		viewport_pointer_apply_constraint(server, constraint);
	}
}

/* Confinement, as opposed to locking.
 *
 * A locked pointer stops dead; a confined one still moves, but may not leave a
 * region the client nominated. That is what a windowed game or a map widget
 * asks for. The region is surface-local, so it has to be shifted into layout
 * coordinates before the cursor position can be tested against it.
 *
 * Returns true when the position was outside the region and has been pulled
 * back to its edge. */
bool viewport_pointer_confine(struct viewport_server *server, double *lx,
	double *ly)
{
	struct wlr_pointer_constraint_v1 *constraint = server->active_constraint;
	if (constraint == NULL ||
			constraint->type != WLR_POINTER_CONSTRAINT_V1_CONFINED) {
		return false;
	}

	struct viewport_toplevel *toplevel = NULL, *candidate;
	wl_list_for_each(candidate, &server->toplevels, link) {
		if (viewport_view_surface(candidate) == constraint->surface) {
			toplevel = candidate;
			break;
		}
	}
	if (toplevel == NULL) {
		return false;
	}

	double sx = *lx - toplevel->box.x;
	double sy = *ly - toplevel->box.y;

	/* An empty region means the whole surface, per the protocol. */
	const pixman_region32_t *region = &constraint->region;
	if (!pixman_region32_not_empty(region)) {
		return false;
	}
	if (pixman_region32_contains_point(region, (int)sx, (int)sy, NULL)) {
		return false;
	}

	/* Outside: snap to the nearest point of the nearest rectangle. Boxes are
	 * half-open, so the far edges sit one pixel short of the bound. */
	int count = 0;
	const pixman_box32_t *boxes = pixman_region32_rectangles(
		(pixman_region32_t *)region, &count);

	double best_x = sx, best_y = sy, best_distance = -1.0;
	for (int i = 0; i < count; i++) {
		double cx = sx, cy = sy;
		if (cx < boxes[i].x1) {
			cx = boxes[i].x1;
		} else if (cx > boxes[i].x2 - 1) {
			cx = boxes[i].x2 - 1;
		}
		if (cy < boxes[i].y1) {
			cy = boxes[i].y1;
		} else if (cy > boxes[i].y2 - 1) {
			cy = boxes[i].y2 - 1;
		}

		double dx = cx - sx, dy = cy - sy;
		double distance = dx * dx + dy * dy;
		if (best_distance < 0.0 || distance < best_distance) {
			best_distance = distance;
			best_x = cx;
			best_y = cy;
		}
	}

	*lx = toplevel->box.x + best_x;
	*ly = toplevel->box.y + best_y;
	return true;
}

bool viewport_pointer_is_locked(struct viewport_server *server)
{
	return server->active_constraint != NULL &&
		server->active_constraint->type == WLR_POINTER_CONSTRAINT_V1_LOCKED;
}

/* Relative motion goes to the focused client on every movement, constrained or
 * not: a game reads deltas even while the cursor is free, and sending them
 * unconditionally is what the protocol expects. */
void viewport_pointer_send_relative(struct viewport_server *server,
	uint32_t time_msec, double dx, double dy, double dx_unaccel,
	double dy_unaccel)
{
	if (server->relative_pointer == NULL) {
		return;
	}
	wlr_relative_pointer_manager_v1_send_relative_motion(
		server->relative_pointer, server->seat, (uint64_t)time_msec * 1000,
		dx, dy, dx_unaccel, dy_unaccel);
}

void viewport_pointer_init(struct viewport_server *server)
{
	server->relative_pointer =
		wlr_relative_pointer_manager_v1_create(server->wl_display);
	server->pointer_constraints =
		wlr_pointer_constraints_v1_create(server->wl_display);

	if (server->pointer_constraints != NULL) {
		server->new_constraint.notify = viewport_handle_new_constraint;
		wl_signal_add(&server->pointer_constraints->events.new_constraint,
			&server->new_constraint);
	}
}
