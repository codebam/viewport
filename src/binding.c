/* SPDX-License-Identifier: MIT
 *
 * Keybindings.
 *
 * Deliberately compositor-side rather than shell-side. The shell is a web page
 * loaded over the network: if it fails to load, throws, or hangs, keyboard
 * shortcuts driven by its JavaScript would go with it — including the one that
 * quits the compositor. Binding "open a terminal" and "exit" in C means the
 * machine stays usable when the UI does not.
 *
 * Syntax mirrors sway closely enough to port a config by hand:
 *
 *   Mod4+Return=exec ghostty
 *   Mod4+d=exec wmenu-run
 *   Mod4+Shift+q=close
 *   Mod4+Shift+e=exit
 *   Mod4+Shift+c=reload
 */
#define _POSIX_C_SOURCE 200809L

#include <ctype.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <sys/wait.h>
#include <unistd.h>

#include <wlr/util/log.h>

#include "viewport.h"

/* ------------------------------------------------------------------------
 * Parsing
 * --------------------------------------------------------------------- */

static bool modifier_from_name(const char *name, uint32_t *out)
{
	static const struct {
		const char *name;
		uint32_t value;
	} table[] = {
		{ "shift", WLR_MODIFIER_SHIFT },
		{ "caps", WLR_MODIFIER_CAPS },
		{ "ctrl", WLR_MODIFIER_CTRL },
		{ "control", WLR_MODIFIER_CTRL },
		{ "alt", WLR_MODIFIER_ALT },
		{ "mod1", WLR_MODIFIER_ALT },
		{ "mod2", WLR_MODIFIER_MOD2 },
		{ "mod3", WLR_MODIFIER_MOD3 },
		/* sway spells the super key Mod4; accept the common aliases too. */
		{ "mod4", WLR_MODIFIER_LOGO },
		{ "super", WLR_MODIFIER_LOGO },
		{ "logo", WLR_MODIFIER_LOGO },
		{ "mod5", WLR_MODIFIER_MOD5 },
	};

	for (size_t i = 0; i < sizeof(table) / sizeof(table[0]); i++) {
		if (strcasecmp(name, table[i].name) == 0) {
			*out = table[i].value;
			return true;
		}
	}
	return false;
}

static bool action_from_string(const char *text, enum viewport_action *action,
	const char **argument)
{
	if (strncmp(text, "exec", 4) == 0 && (text[4] == ' ' || text[4] == '\t')) {
		const char *command = text + 4;
		while (*command == ' ' || *command == '\t') {
			command++;
		}
		if (*command == '\0') {
			return false;
		}
		*action = VIEWPORT_ACTION_EXEC;
		*argument = command;
		return true;
	}

	if (strncmp(text, "focus", 5) == 0 && (text[5] == ' ' || text[5] == '\t')) {
		const char *direction = text + 5;
		while (*direction == ' ' || *direction == '\t') {
			direction++;
		}
		if (*direction == '\0') {
			return false;
		}
		*action = VIEWPORT_ACTION_FOCUS;
		*argument = direction;
		return true;
	}

	if (strcmp(text, "appearance toggle") == 0) {
		*action = VIEWPORT_ACTION_APPEARANCE;
		*argument = NULL;
		return true;
	}

	if (strncmp(text, "mode", 4) == 0 && (text[4] == ' ' || text[4] == '\t')) {
		const char *name = text + 4;
		while (*name == ' ' || *name == '\t') {
			name++;
		}
		if (*name == '\0') {
			return false;
		}
		*action = VIEWPORT_ACTION_MODE;
		*argument = name;
		return true;
	}

	if (strncmp(text, "shell", 5) == 0 && (text[5] == ' ' || text[5] == '\t')) {
		const char *command = text + 5;
		while (*command == ' ' || *command == '\t') {
			command++;
		}
		if (*command == '\0') {
			return false;
		}
		*action = VIEWPORT_ACTION_SHELL;
		*argument = command;
		return true;
	}

	*argument = NULL;
	if (strcmp(text, "close") == 0 || strcmp(text, "kill") == 0) {
		*action = VIEWPORT_ACTION_CLOSE;
	} else if (strcmp(text, "exit") == 0) {
		*action = VIEWPORT_ACTION_EXIT;
	} else if (strcmp(text, "reload") == 0) {
		*action = VIEWPORT_ACTION_RELOAD;
	} else {
		return false;
	}
	return true;
}

bool viewport_binding_add(struct viewport_server *server, const char *spec)
{
	const char *equals = strchr(spec, '=');
	if (equals == NULL || equals == spec) {
		wlr_log(WLR_ERROR, "binding '%s': expected CHORD=ACTION", spec);
		return false;
	}

	size_t chord_len = (size_t)(equals - spec);
	char *chord = strndup(spec, chord_len);
	if (chord == NULL) {
		return false;
	}

	/* An optional `mode/` prefix scopes the binding, so `resize/h` only fires
	 * while resize mode is active. Unqualified bindings are "default". */
	char *mode = NULL;
	char *slash = strchr(chord, '/');
	if (slash != NULL) {
		*slash = '\0';
		mode = strdup(chord);
		memmove(chord, slash + 1, strlen(slash + 1) + 1);
		if (mode == NULL) {
			free(chord);
			return false;
		}
	}

	uint32_t modifiers = 0;
	xkb_keysym_t keysym = XKB_KEY_NoSymbol;
	bool ok = true;

	/* Everything before the final '+' is a modifier; the remainder is the
	 * key. Walking left to right keeps "Mod4+Shift+plus" unambiguous. */
	char *cursor = chord;
	char *plus;
	while ((plus = strchr(cursor, '+')) != NULL) {
		*plus = '\0';
		uint32_t modifier;
		if (!modifier_from_name(cursor, &modifier)) {
			wlr_log(WLR_ERROR, "binding '%s': unknown modifier '%s'", spec,
				cursor);
			ok = false;
			break;
		}
		modifiers |= modifier;
		cursor = plus + 1;
	}

	if (ok) {
		keysym = xkb_keysym_from_name(cursor, XKB_KEYSYM_NO_FLAGS);
		if (keysym == XKB_KEY_NoSymbol) {
			/* Accept lowercase spellings such as "return" or "q". */
			keysym = xkb_keysym_from_name(cursor, XKB_KEYSYM_CASE_INSENSITIVE);
		}
		if (keysym == XKB_KEY_NoSymbol) {
			wlr_log(WLR_ERROR, "binding '%s': unknown key '%s'", spec, cursor);
			ok = false;
		}
	}

	free(chord);
	if (!ok) {
		return false;
	}

	enum viewport_action action;
	const char *argument;
	if (!action_from_string(equals + 1, &action, &argument)) {
		wlr_log(WLR_ERROR, "binding '%s': unknown action '%s'", spec,
			equals + 1);
		return false;
	}

	struct viewport_binding *binding = calloc(1, sizeof(*binding));
	if (binding == NULL) {
		free(mode);
		return false;
	}
	binding->mode = mode != NULL ? mode : strdup("default");
	binding->modifiers = modifiers;
	binding->keysym = keysym;
	binding->action = action;
	if (argument != NULL) {
		binding->argument = strdup(argument);
		if (binding->argument == NULL) {
			free(binding);
			return false;
		}
	}

	/* Later definitions win: pushing to the front means a user-supplied bind
	 * shadows the built-in default for the same chord. */
	wl_list_insert(&server->bindings, &binding->link);
	return true;
}

void viewport_bindings_add_defaults(struct viewport_server *server,
	const char *terminal, const char *menu)
{
	char spec[512];

	/* The scrolling layout needs different movement keys, not just a different
	 * renderer. Directional focus in the compositor works from where windows
	 * are on screen, and in an infinite strip the column you want to reach is
	 * usually scrolled off it — so focus and movement go to the shell, which is
	 * the only thing that knows the strip exists. */
	const bool scrolling = server->config.layout != NULL &&
		strcmp(server->config.layout, "scrolling") == 0;

	if (terminal != NULL) {
		snprintf(spec, sizeof(spec), "Mod4+Return=exec %s", terminal);
		viewport_binding_add(server, spec);
	}
	if (menu != NULL) {
		snprintf(spec, sizeof(spec), "Mod4+d=exec %s", menu);
		viewport_binding_add(server, spec);
	}

	viewport_binding_add(server, "Mod4+Shift+q=close");
	viewport_binding_add(server, "Mod4+Shift+e=exit");
	viewport_binding_add(server, "Mod4+Shift+c=reload");

	/* sway's movement keys, plus the arrows. */
	static const char *const directions[] = { "left", "down", "up", "right" };
	static const char *const letters[] = { "h", "j", "k", "l" };
	static const char *const arrows[] = { "Left", "Down", "Up", "Right" };

	for (int i = 0; i < 4; i++) {
		const char *action = scrolling ? "shell layout.focus" : "focus";
		snprintf(spec, sizeof(spec), "Mod4+%s=%s %s", letters[i], action,
			directions[i]);
		viewport_binding_add(server, spec);
		snprintf(spec, sizeof(spec), "Mod4+%s=%s %s", arrows[i], action,
			directions[i]);
		viewport_binding_add(server, spec);
	}
	viewport_binding_add(server, "Mod4+Tab=focus next");
	viewport_binding_add(server, "Mod4+Shift+Tab=focus prev");

	/* Layout is shell policy, so all of these are passthroughs: the shell
	 * decides what tiling, fullscreen and moving mean. */
	viewport_binding_add(server, "Mod4+f=shell window.fullscreen");
	/* sway's floating toggle. Dialogs float on their own; this is for the rest,
	 * and for dropping one back into the tiling layout. */
	viewport_binding_add(server, "Mod4+Shift+space=shell layout.float.toggle");
	viewport_binding_add(server, "Mod4+Shift+h=shell window.move left");
	viewport_binding_add(server, "Mod4+Shift+j=shell window.move down");
	viewport_binding_add(server, "Mod4+Shift+k=shell window.move up");
	viewport_binding_add(server, "Mod4+Shift+l=shell window.move right");
	viewport_binding_add(server, "Mod4+Shift+Left=shell window.move left");
	viewport_binding_add(server, "Mod4+Shift+Down=shell window.move down");
	viewport_binding_add(server, "Mod4+Shift+Up=shell window.move up");
	viewport_binding_add(server, "Mod4+Shift+Right=shell window.move right");
	viewport_binding_add(server, "Mod4+b=shell layout.split horizontal");
	viewport_binding_add(server, "Mod4+v=shell layout.split vertical");
	/* sway's `layout toggle split`: flip the container the focused window is
	 * in between side-by-side and stacked. */
	viewport_binding_add(server, "Mod4+e=shell layout.toggle");
	/* sway's tabbed and stacked containers. Both show one window at a time out
	 * of a container, with a strip of titles to pick from — the one place this
	 * shell draws titles, because without them a tab cannot be identified. */
	viewport_binding_add(server, "Mod4+w=shell layout.tabbed");
	viewport_binding_add(server, "Mod4+s=shell layout.stacked");
	viewport_binding_add(server, "Mod4+n=shell bar.toggle");
	viewport_binding_add(server, "Mod4+Shift+d=appearance toggle");

	if (scrolling) {
		/* niri's column keys. A column is the unit here: windows stack inside
		 * one, and the strip scrolls between them.
		 *
		 * Consume and expel are what make the model work — pulling the window
		 * beside you into your column, or pushing one back out into its own —
		 * and there is no equivalent in a tiling tree. */
		viewport_binding_add(server, "Mod4+comma=shell layout.consume");
		viewport_binding_add(server, "Mod4+period=shell layout.expel");
		/* Cycle the focused column through a few widths, as Mod+R does in
		 * niri. Nothing else resizes: columns do not share space, so widening
		 * one just pushes the rest along the strip. */
		viewport_binding_add(server, "Mod4+r=shell layout.column.width");
		viewport_binding_add(server,
			"Mod4+Shift+r=shell layout.column.height");
		/* Jump to the ends of the strip. */
		viewport_binding_add(server, "Mod4+Home=shell layout.focus first");
		viewport_binding_add(server, "Mod4+End=shell layout.focus last");
	}

	/* Resize mode, as in sway: Mod4+r enters it, hjkl and the arrows resize a
	 * step at a time, Escape or Return leaves. Bindings are scoped to the mode
	 * so h/j/k/l keep meaning "move focus" everywhere else. */
	if (!scrolling) {
		viewport_binding_add(server, "Mod4+r=mode resize");
	}
	viewport_binding_add(server, "resize/h=shell layout.resize left");
	viewport_binding_add(server, "resize/j=shell layout.resize down");
	viewport_binding_add(server, "resize/k=shell layout.resize up");
	viewport_binding_add(server, "resize/l=shell layout.resize right");
	viewport_binding_add(server, "resize/Left=shell layout.resize left");
	viewport_binding_add(server, "resize/Down=shell layout.resize down");
	viewport_binding_add(server, "resize/Up=shell layout.resize up");
	viewport_binding_add(server, "resize/Right=shell layout.resize right");
	viewport_binding_add(server, "resize/Escape=mode default");
	viewport_binding_add(server, "resize/Return=mode default");
	viewport_binding_add(server, "resize/Mod4+r=mode default");

	/* Workspaces are shell policy, so these are passthroughs. The shell
	 * decides what "workspace 3" means; C only delivers the keystroke. */
	for (int i = 1; i <= 9; i++) {
		snprintf(spec, sizeof(spec), "Mod4+%d=shell workspace.switch %d", i, i);
		viewport_binding_add(server, spec);
		snprintf(spec, sizeof(spec), "Mod4+Shift+%d=shell workspace.move %d",
			i, i);
		viewport_binding_add(server, spec);
	}
}

/* ------------------------------------------------------------------------
 * Execution
 * --------------------------------------------------------------------- */

static void spawn(const char *command)
{
	/* Double fork so the child is reparented to init. A single fork would
	 * leave a zombie for every launched program, because the compositor never
	 * reaps — and it would also make the terminal a child of the compositor,
	 * so quitting would take every window with it. */
	pid_t pid = fork();
	if (pid < 0) {
		wlr_log(WLR_ERROR, "fork failed for '%s'", command);
		return;
	}

	if (pid == 0) {
		setsid();
		if (fork() == 0) {
			execl("/bin/sh", "/bin/sh", "-c", command, (void *)NULL);
			_exit(127);
		}
		_exit(0);
	}

	waitpid(pid, NULL, 0);
}

static void run_action(struct viewport_server *server,
	struct viewport_binding *binding)
{
	switch (binding->action) {
	case VIEWPORT_ACTION_EXEC:
		wlr_log(WLR_DEBUG, "exec: %s", binding->argument);
		spawn(binding->argument);
		break;
	case VIEWPORT_ACTION_CLOSE:
		if (server->focused != NULL) {
			viewport_toplevel_close(server->focused);
		}
		break;
	case VIEWPORT_ACTION_EXIT:
		wlr_log(WLR_INFO, "exit requested via keybinding");
		viewport_server_terminate(server);
		break;
	case VIEWPORT_ACTION_RELOAD:
		if (server->web != NULL) {
			wlr_log(WLR_INFO, "reloading shell");
			viewport_web_reload(server->web);
		}
		break;
	case VIEWPORT_ACTION_FOCUS:
		viewport_focus_direction(server, binding->argument);
		break;
	case VIEWPORT_ACTION_SHELL:
		viewport_ipc_notify_shell_command(server, binding->argument);
		break;
	case VIEWPORT_ACTION_APPEARANCE:
		viewport_appearance_set_dark(server->appearance,
			!viewport_appearance_is_dark(server->appearance));
		break;
	case VIEWPORT_ACTION_MODE: {
		free(server->mode);
		server->mode = strdup(binding->argument);
		wlr_log(WLR_DEBUG, "binding mode: %s", server->mode);
		/* The shell shows which mode is active, as sway's bar does. */
		char command[96];
		snprintf(command, sizeof(command), "mode.changed %s", server->mode);
		viewport_ipc_notify_shell_command(server, command);
		break;
	}
	}
}

bool viewport_bindings_handle(struct viewport_server *server,
	uint32_t modifiers, const xkb_keysym_t *keysyms, int nsyms)
{
	/* Caps and num lock must not participate in matching, or a binding stops
	 * working the moment caps lock is on. */
	modifiers &= ~(uint32_t)(WLR_MODIFIER_CAPS | WLR_MODIFIER_MOD2);

	const char *mode = server->mode != NULL ? server->mode : "default";

	for (int i = 0; i < nsyms; i++) {
		struct viewport_binding *binding;
		wl_list_for_each(binding, &server->bindings, link) {
			if (strcmp(binding->mode, mode) != 0 ||
					binding->modifiers != modifiers ||
					binding->keysym != keysyms[i]) {
				continue;
			}
			wlr_log(WLR_DEBUG, "binding matched (mods 0x%x)", modifiers);
			run_action(server, binding);
			return true;
		}
	}

	return false;
}

void viewport_bindings_finish(struct viewport_server *server)
{
	struct viewport_binding *binding, *tmp;
	wl_list_for_each_safe(binding, tmp, &server->bindings, link) {
		wl_list_remove(&binding->link);
		free(binding->mode);
		free(binding->argument);
		free(binding);
	}
}
