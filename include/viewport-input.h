/* SPDX-License-Identifier: MIT
 *
 * Input.
 *
 * Pointer, keyboard, touch, tablet and gestures, plus the two things that decide
 where an event goes: the keybinding table, which outranks every client, and
 the input-method relay, which outranks the client but not the bindings.
 *
 * Split out of viewport.h, which had grown to declare every function in the
 * project: a comment edited in it recompiled all twenty-nine translation
 * units, and nothing recorded which of them any given declaration was for.
 * The types stay there — struct viewport_server embeds them by value, so
 * everything needs it — and only the interfaces moved.
 */
#ifndef VIEWPORT_INPUT_H
#define VIEWPORT_INPUT_H

#include "viewport.h"

/* Graphics tablets. */
void viewport_tablet_init(struct viewport_server *server);
void viewport_tablet_add(struct viewport_server *server,
	struct wlr_input_device *device);
/* Re-runs the pointer's idea of what it is over, after it has been moved by
 * something other than the pointer itself. */
void viewport_cursor_refresh(struct viewport_server *server, uint32_t time_msec);
/* Re-runs it because the scene changed rather than the pointer, which is what
 * gives a menu opening under a still cursor its wl_pointer.enter. */
void viewport_cursor_rebase(struct viewport_server *server);
/* Delivers a pointer button to whatever is under the cursor. */
void viewport_pointer_button(struct viewport_server *server, uint32_t time_msec,
	uint32_t button, bool pressed);

/* Touchpad gestures: three fingers for the compositor, the rest forwarded. */
void viewport_gestures_init(struct viewport_server *server);

/* Input methods: the relay between text-input-v3 and input-method-v2. */
struct viewport_ime *viewport_ime_create(struct viewport_server *server);
void viewport_ime_destroy(struct viewport_ime *ime);
bool viewport_ime_handle_key(struct viewport_ime *ime,
	struct wlr_keyboard *keyboard, uint32_t time_msec, uint32_t keycode,
	uint32_t state);
bool viewport_ime_handle_modifiers(struct viewport_ime *ime,
	struct wlr_keyboard *keyboard);

/* -------------------------------------------------------------------------
 * input.c
 * ---------------------------------------------------------------------- */

void viewport_handle_new_input(struct wl_listener *listener, void *data);
void viewport_handle_new_virtual_keyboard(struct wl_listener *listener,
	void *data);
void viewport_cursor_init(struct viewport_server *server);

/* Topmost surface under a layout-space point, or NULL if the point lands on
 * the web shell. `sx`/`sy` receive surface-local coordinates. */
struct wlr_surface *viewport_surface_at(struct viewport_server *server,
	double lx, double ly, double *sx, double *sy,
	struct viewport_toplevel **toplevel_out);

/* Moves keyboard focus to the shell (no client toplevel focused). */
void viewport_focus_web(struct viewport_server *server);
/* Hands the keyboard back after a launcher or menu closes, to the previously
 * focused window if it is still on screen and to the shell if it is not. */
void viewport_focus_restore(struct viewport_server *server);

/* -------------------------------------------------------------------------
 * binding.c
 * ---------------------------------------------------------------------- */

enum viewport_action {
	/* Bound to nothing on purpose.
	 *
	 * Not the same as leaving a chord out of the config: the built-in defaults
	 * fill in every chord nobody claimed, so the only way to say "Mod4+d must
	 * reach the application" is to claim it and do nothing with it. A match on
	 * one of these stops the search and does not consume the key. */
	VIEWPORT_ACTION_NONE,
	VIEWPORT_ACTION_EXEC,   /* run a shell command */
	VIEWPORT_ACTION_CLOSE,  /* close the focused window */
	VIEWPORT_ACTION_EXIT,   /* quit the compositor */
	VIEWPORT_ACTION_RELOAD, /* reload the web shell */
	VIEWPORT_ACTION_FOCUS,  /* move focus: next|prev|left|right|up|down */
	VIEWPORT_ACTION_SHELL,  /* forward the rest of the line to the shell */
	VIEWPORT_ACTION_MODE,   /* switch binding mode, e.g. sway's resize mode */
	VIEWPORT_ACTION_APPEARANCE, /* toggle the dark/light preference */
	VIEWPORT_ACTION_LOCK,   /* run the configured locker now */
	VIEWPORT_ACTION_BLANK,  /* turn the outputs off until the next input */
};

struct viewport_binding {
	struct wl_list link;
	/* Mode this binding belongs to; "default" unless qualified. Written in
	 * the config as `resize/h=...`, mirroring sway's `mode "resize"` blocks. */
	char *mode;
	uint32_t modifiers;   /* mask of WLR_MODIFIER_* */
	xkb_keysym_t keysym;
	enum viewport_action action;
	char *argument;       /* command line, for VIEWPORT_ACTION_EXEC */
};

/* Parses "Mod4+Shift+q=close" or "Mod4+Return=exec ghostty". Returns false
 * with a logged reason on malformed input. */
bool viewport_binding_add(struct viewport_server *server, const char *spec);

/* Installs the sway-compatible defaults. `terminal` and `menu` may be NULL to
 * skip those two binds. */
void viewport_bindings_add_defaults(struct viewport_server *server,
	const char *terminal, const char *menu);

/* Runs the binding matching this chord, if any. Returns true when the key was
 * consumed and must not reach the focused client or the shell. */
bool viewport_bindings_handle(struct viewport_server *server,
	uint32_t modifiers, const xkb_keysym_t *keysyms, int nsyms);

void viewport_bindings_finish(struct viewport_server *server);

/* -------------------------------------------------------------------------
 * pointer.c
 *
 * Pointer capture: relative motion and lock/confine constraints, which is what
 * lets a first-person game read mouselook. Covers X11 games too — Xwayland
 * implements XGrabPointer using these same protocols.
 * ---------------------------------------------------------------------- */

void viewport_pointer_init(struct viewport_server *server);
void viewport_handle_new_constraint(struct wl_listener *listener, void *data);
void viewport_pointer_apply_constraint(struct viewport_server *server,
	struct wlr_pointer_constraint_v1 *constraint);
void viewport_pointer_deactivate_constraint(struct viewport_server *server);
void viewport_pointer_check_constraint(struct viewport_server *server,
	struct wlr_surface *surface);
bool viewport_pointer_is_locked(struct viewport_server *server);
bool viewport_pointer_confine(struct viewport_server *server, double *lx,
	double *ly);
void viewport_pointer_send_relative(struct viewport_server *server,
	uint32_t time_msec, double dx, double dy, double dx_unaccel,
	double dy_unaccel);

#endif /* VIEWPORT_INPUT_H */
