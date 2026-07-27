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
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>

#include <glib.h>
#include <sys/wait.h>
#include <unistd.h>

#include <wlr/util/log.h>

#include "viewport-view.h"
#include "viewport-input.h"
#include "viewport-shell.h"

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
	/* "none" claims a chord without doing anything with it, which is how a
	 * built-in default is removed rather than replaced: the defaults skip any
	 * chord that is already spoken for, and a match here stops the search
	 * without consuming the key, so it reaches the focused application. sway
	 * spells this by simply not writing the bindsym, which works there because
	 * sway has no built-in keymap to push back against. */
	if (strcmp(text, "none") == 0 || strcmp(text, "unbind") == 0) {
		*action = VIEWPORT_ACTION_NONE;
	} else if (strcmp(text, "close") == 0 || strcmp(text, "kill") == 0) {
		*action = VIEWPORT_ACTION_CLOSE;
	} else if (strcmp(text, "exit") == 0) {
		*action = VIEWPORT_ACTION_EXIT;
	} else if (strcmp(text, "reload") == 0) {
		*action = VIEWPORT_ACTION_RELOAD;
	} else if (strcmp(text, "lock") == 0) {
		*action = VIEWPORT_ACTION_LOCK;
	} else if (strcmp(text, "blank") == 0) {
		*action = VIEWPORT_ACTION_BLANK;
	} else {
		return false;
	}
	return true;
}

/* Is this chord already spoken for?
 *
 * Asked by the defaults, and only by them. Two user bindings for one chord are
 * a user overriding themselves and the later one should win; a user binding and
 * a built-in are a user overriding us, and there the user wins regardless of
 * which was installed first. Those want opposite answers, so the question is
 * asked at the point where the difference is known rather than resolved by
 * insertion order — which is what used to be attempted, and got it backwards. */
static bool binding_exists(struct viewport_server *server, const char *mode,
	uint32_t modifiers, xkb_keysym_t keysym)
{
	struct viewport_binding *binding;
	wl_list_for_each(binding, &server->bindings, link) {
		if (binding->modifiers == modifiers && binding->keysym == keysym &&
				strcmp(binding->mode, mode) == 0) {
			return true;
		}
	}
	return false;
}

static bool binding_add(struct viewport_server *server, const char *spec,
	bool yield_to_existing)
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
		if (mode == NULL) {
			free(chord);
			return false;
		}
		memmove(chord, slash + 1, strlen(slash + 1) + 1);
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
		free(mode);
		return false;
	}

	enum viewport_action action;
	const char *argument;
	if (!action_from_string(equals + 1, &action, &argument)) {
		wlr_log(WLR_ERROR, "binding '%s': unknown action '%s'", spec,
			equals + 1);
		free(mode);
		return false;
	}

	/* A default stepping aside for something the user already asked for. This
	 * is what makes overriding one chord cost only that chord: everything the
	 * config did not mention still gets its built-in. */
	if (yield_to_existing && binding_exists(server,
			mode != NULL ? mode : "default", modifiers, keysym)) {
		free(mode);
		return true;
	}

	struct viewport_binding *binding = calloc(1, sizeof(*binding));
	if (binding == NULL) {
		free(mode);
		return false;
	}
	binding->mode = mode != NULL ? mode : strdup("default");
	if (binding->mode == NULL) {
		free(binding);
		return false;
	}
	binding->modifiers = modifiers;
	binding->keysym = keysym;
	binding->action = action;
	if (argument != NULL) {
		binding->argument = strdup(argument);
		if (binding->argument == NULL) {
			free(binding->mode);
			free(binding);
			return false;
		}
	}

	/* Front, so that of two bindings for one chord the later definition is the
	 * one the matcher reaches. Between a user's two bindings that is what is
	 * wanted; against a built-in it is not, and it used to be relied on for
	 * both — the defaults are installed last, so this put every one of them in
	 * front of the config that was supposed to override it. Now the defaults
	 * yield above instead, and this ordering only decides between equals. */
	wl_list_insert(&server->bindings, &binding->link);
	return true;
}

bool viewport_binding_add(struct viewport_server *server, const char *spec)
{
	return binding_add(server, spec, false);
}

/* Something to open a terminal and a launcher with, when nothing said which.
 *
 * A fresh install has no config file, and without these two bindings the
 * desktop comes up with no way to start anything at all — the only working key
 * is the one that quits. Guessing is better than that, and the guess is
 * checked: the first of these that is actually installed wins, so the binding
 * either works or is not made.
 *
 * Ordered by how likely someone on this compositor is to have them rather than
 * by popularity in general. */
static const char *const terminal_candidates[] = {
	"rio", "foot", "ghostty", "alacritty", "kitty", "wezterm", "xterm", NULL,
};

static const char *const menu_candidates[] = {
	"wmenu-run", "fuzzel", "wofi", "rofi", "bemenu-run", "dmenu-wl_run", NULL,
};

static const char *first_installed(const char *const *candidates)
{
	for (size_t i = 0; candidates[i] != NULL; i++) {
		char *path = g_find_program_in_path(candidates[i]);
		if (path != NULL) {
			g_free(path);
			return candidates[i];
		}
	}
	return NULL;
}

/* A built-in, installed only where the user has not already spoken.
 *
 * Every default goes through this rather than viewport_binding_add() so the
 * rule lives in one place: the config file and --bind are the authority, and a
 * built-in is what fills the silence. */
static void add_default(struct viewport_server *server, const char *spec)
{
	binding_add(server, spec, true);
}

void viewport_bindings_add_defaults(struct viewport_server *server,
	const char *terminal, const char *menu)
{
	char spec[512];

	if (terminal == NULL) {
		terminal = first_installed(terminal_candidates);
		if (terminal != NULL) {
			wlr_log(WLR_INFO, "no terminal configured; using %s", terminal);
		} else {
			wlr_log(WLR_ERROR,
				"no terminal configured and none of the usual ones are "
				"installed: Mod4+Return will do nothing");
		}
	}
	if (menu == NULL) {
		menu = first_installed(menu_candidates);
		if (menu != NULL) {
			wlr_log(WLR_INFO, "no launcher configured; using %s", menu);
		}
	}

	/* The scrolling layout needs different movement keys, not just a different
	 * renderer. Directional focus in the compositor works from where windows
	 * are on screen, and in an infinite strip the column you want to reach is
	 * usually scrolled off it — so focus and movement go to the shell, which is
	 * the only thing that knows the strip exists. */
	const bool scrolling = server->config.layout != NULL &&
		strcmp(server->config.layout, "scrolling") == 0;

	if (terminal != NULL) {
		snprintf(spec, sizeof(spec), "Mod4+Return=exec %s", terminal);
		add_default(server, spec);
	}
	if (menu != NULL) {
		snprintf(spec, sizeof(spec), "Mod4+d=exec %s", menu);
		add_default(server, spec);
	}

	add_default(server, "Mod4+Shift+q=close");
	add_default(server, "Mod4+Shift+e=exit");
	add_default(server, "Mod4+Shift+c=reload");

	/* sway's movement keys, plus the arrows. */
	static const char *const directions[] = { "left", "down", "up", "right" };
	static const char *const letters[] = { "h", "j", "k", "l" };
	static const char *const arrows[] = { "Left", "Down", "Up", "Right" };

	for (int i = 0; i < 4; i++) {
		const char *action = scrolling ? "shell layout.focus" : "focus";
		snprintf(spec, sizeof(spec), "Mod4+%s=%s %s", letters[i], action,
			directions[i]);
		add_default(server, spec);
		snprintf(spec, sizeof(spec), "Mod4+%s=%s %s", arrows[i], action,
			directions[i]);
		add_default(server, spec);
	}
	add_default(server, "Mod4+Tab=focus next");
	add_default(server, "Mod4+Shift+Tab=focus prev");

	/* Layout is shell policy, so all of these are passthroughs: the shell
	 * decides what tiling, fullscreen and moving mean. */
	add_default(server, "Mod4+f=shell window.fullscreen");
	add_default(server, "Mod4+a=shell window.focus_parent");
	/* sway's floating toggle. Dialogs float on their own; this is for the rest,
	 * and for dropping one back into the tiling layout. */
	add_default(server, "Mod4+Shift+space=shell layout.float.toggle");
	add_default(server, "Mod4+Shift+h=shell window.move left");
	add_default(server, "Mod4+Shift+j=shell window.move down");
	add_default(server, "Mod4+Shift+k=shell window.move up");
	add_default(server, "Mod4+Shift+l=shell window.move right");
	add_default(server, "Mod4+Shift+Left=shell window.move left");
	add_default(server, "Mod4+Shift+Down=shell window.move down");
	add_default(server, "Mod4+Shift+Up=shell window.move up");
	add_default(server, "Mod4+Shift+Right=shell window.move right");
	add_default(server, "Mod4+b=shell layout.split horizontal");
	add_default(server, "Mod4+v=shell layout.split vertical");
	/* sway's `layout toggle split`: flip the container the focused window is
	 * in between side-by-side and stacked. */
	add_default(server, "Mod4+e=shell layout.toggle");
	/* sway's tabbed and stacked containers. Both show one window at a time out
	 * of a container, with a strip of titles to pick from — the one place this
	 * shell draws titles, because without them a tab cannot be identified. */
	add_default(server, "Mod4+w=shell layout.tabbed");
	add_default(server, "Mod4+s=shell layout.stacked");
	add_default(server, "Mod4+n=shell bar.toggle");
	/* Every workspace at once, scaled down. The compositor shrinks the windows
	 * themselves — no client is asked to resize. */
	add_default(server, "Mod4+o=shell layout.overview");
	add_default(server, "Mod4+Shift+d=appearance toggle");
	/* Lock now, and turn the screens off now — the same two things the idle
	 * timer does, for when you are leaving rather than waiting to be noticed
	 * leaving. The screens come back on the next keypress. */
	add_default(server, "Mod4+Shift+x=lock");
	add_default(server, "Mod4+Shift+b=blank");
	/* HDR on the monitor you are looking at. Per output rather than global,
	 * because a display that can do it usually sits next to one that cannot. */
	add_default(server, "Mod4+Shift+p=shell output.hdr");

	if (scrolling) {
		/* niri's column keys. A column is the unit here: windows stack inside
		 * one, and the strip scrolls between them.
		 *
		 * Consume and expel are what make the model work — pulling the window
		 * beside you into your column, or pushing one back out into its own —
		 * and there is no equivalent in a tiling tree. */
		add_default(server, "Mod4+comma=shell layout.consume");
		add_default(server, "Mod4+period=shell layout.expel");
		/* Cycle the focused column through a few widths, as Mod+R does in
		 * niri. Nothing else resizes: columns do not share space, so widening
		 * one just pushes the rest along the strip. */
		add_default(server, "Mod4+r=shell layout.column.width");
		add_default(server,
			"Mod4+Shift+r=shell layout.column.height");
		/* Jump to the ends of the strip. */
		add_default(server, "Mod4+Home=shell layout.focus first");
		add_default(server, "Mod4+End=shell layout.focus last");
	}

	/* Resize mode, as in sway: Mod4+r enters it, hjkl and the arrows resize a
	 * step at a time, Escape or Return leaves. Bindings are scoped to the mode
	 * so h/j/k/l keep meaning "move focus" everywhere else. */
	if (!scrolling) {
		add_default(server, "Mod4+r=mode resize");
	}
	add_default(server, "resize/h=shell layout.resize left");
	add_default(server, "resize/j=shell layout.resize down");
	add_default(server, "resize/k=shell layout.resize up");
	add_default(server, "resize/l=shell layout.resize right");
	add_default(server, "resize/Left=shell layout.resize left");
	add_default(server, "resize/Down=shell layout.resize down");
	add_default(server, "resize/Up=shell layout.resize up");
	add_default(server, "resize/Right=shell layout.resize right");
	add_default(server, "resize/Escape=mode default");
	add_default(server, "resize/Return=mode default");
	add_default(server, "resize/Mod4+r=mode default");

	/* Media and hardware keys.
	 *
	 * These are not compositor concepts at all — they are keys a keyboard
	 * happens to have, and every one of them is somebody else's job. They are
	 * bound here anyway because a desktop where the play key does nothing is
	 * broken in a way no application can fix from its own side: the key never
	 * reaches it. XF86AudioPlay does not go to "the focused window", it goes to
	 * whichever player is playing, which is exactly what playerctl resolves
	 * over MPRIS.
	 *
	 * Missing tools fail quietly per keypress rather than at startup, which is
	 * the right trade for a binding nobody may ever press. Anything defined in
	 * the config file replaces the lot, so a different mixer is a matter of
	 * saying so there. */
	add_default(server,
		"XF86AudioPlay=exec playerctl play-pause");
	add_default(server, "XF86AudioPause=exec playerctl pause");
	add_default(server, "XF86AudioNext=exec playerctl next");
	add_default(server, "XF86AudioPrev=exec playerctl previous");
	add_default(server, "XF86AudioStop=exec playerctl stop");

	/* Volume and mute go to the sink rather than to a player: turning the
	 * volume down means the machine, not whatever happens to be playing. */
	add_default(server,
		"XF86AudioRaiseVolume=exec wpctl set-volume -l 1.5 "
		"@DEFAULT_AUDIO_SINK@ 5%+");
	add_default(server,
		"XF86AudioLowerVolume=exec wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%-");
	add_default(server,
		"XF86AudioMute=exec wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle");
	add_default(server,
		"XF86AudioMicMute=exec wpctl set-mute @DEFAULT_AUDIO_SOURCE@ toggle");

	add_default(server,
		"XF86MonBrightnessUp=exec brightnessctl set 5%+");
	add_default(server,
		"XF86MonBrightnessDown=exec brightnessctl set 5%-");

	/* Workspaces are shell policy, so these are passthroughs. The shell
	 * decides what "workspace 3" means; C only delivers the keystroke. */
	/* sway's `workspace back_and_forth`. Repeating the switch for the workspace
	 * you are on does the same thing; this is for reaching it directly. */
	add_default(server, "Mod4+grave=shell workspace.back");

	for (int i = 1; i <= 9; i++) {
		snprintf(spec, sizeof(spec), "Mod4+%d=shell workspace.switch %d", i, i);
		add_default(server, spec);
		snprintf(spec, sizeof(spec), "Mod4+Shift+%d=shell workspace.move %d",
			i, i);
		add_default(server, spec);
	}
}

/* ------------------------------------------------------------------------
 * Execution
 * --------------------------------------------------------------------- */

void viewport_spawn(const char *command)
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
			/* execve() resets installed handlers but carries SIG_IGN
			 * through untouched, so main.c's signal(SIGCHLD, SIG_IGN)
			 * would otherwise become the launched program's problem —
			 * and its children's, and theirs. A process that ignores
			 * SIGCHLD has its children reaped out from under it, so
			 * wait() and waitpid() fail with ECHILD and system()
			 * returns -1: terminals lose the exit status of anything
			 * they run, and anything that shells out misreads it as
			 * failure. Reset it here, in the branch that actually
			 * execs, because this is the only disposition that
			 * survives the call.
			 *
			 * The process signal mask needs no such repair: GLib's
			 * g_unix_signal_add() installs an ordinary sigaction
			 * handler and blocks nothing, so main.c's SIGTERM and
			 * SIGINT sources leave the mask empty for exec to
			 * inherit. */
			signal(SIGCHLD, SIG_DFL);
			execl("/bin/sh", "/bin/sh", "-c", command, (void *)NULL);
			_exit(127);
		}
		_exit(0);
	}

	/* Under SIG_IGN this returns -1/ECHILD rather than the pid, because the
	 * kernel has already reaped the child. That is fine: the wait is only
	 * here to keep the intermediate child from lingering, and it _exit(0)s
	 * the moment the second fork returns, so either outcome gets us back
	 * into the event loop without blocking on the launched program. */
	waitpid(pid, NULL, 0);
}

static void run_action(struct viewport_server *server,
	struct viewport_binding *binding)
{
	switch (binding->action) {
	/* Never reached: viewport_bindings_handle() answers an unbound chord and
	 * returns before it gets here. Named anyway, because -Wswitch is what will
	 * point at this function the next time an action is added. */
	case VIEWPORT_ACTION_NONE:
		break;
	case VIEWPORT_ACTION_EXEC:
		wlr_log(WLR_DEBUG, "exec: %s", binding->argument);
		viewport_spawn(binding->argument);
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
		/* Both halves: the config file is where bindings live, and reloading
		 * only the shell meant a changed keybinding needed a full restart. */
		viewport_config_reload(server);
		if (server->web != NULL) {
			wlr_log(WLR_INFO, "reloading shell");
			viewport_web_reload(server->web);
		}
		break;
	case VIEWPORT_ACTION_LOCK:
		/* The same locker the idle timer would run, so there is one place to
		 * configure it and no second answer to what locking means here. */
		if (server->config.idle_lock_command != NULL) {
			viewport_spawn(server->config.idle_lock_command);
		} else {
			wlr_log(WLR_ERROR,
				"lock: no idle.lock_command in the config; nothing to run");
		}
		break;
	case VIEWPORT_ACTION_BLANK:
		viewport_idle_blank(server);
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
			/* Deliberately bound to nothing. Reporting the chord as unhandled
			 * is the whole point — it is how the key gets past the compositor
			 * and reaches the application — but the search still stops here,
			 * because this entry is the answer for this chord. */
			if (binding->action == VIEWPORT_ACTION_NONE) {
				wlr_log(WLR_DEBUG, "chord is unbound (mods 0x%x)", modifiers);
				return false;
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
