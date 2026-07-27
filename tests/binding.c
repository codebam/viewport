/* SPDX-License-Identifier: MIT
 *
 * Which binding wins.
 *
 * binding.c is the tier that has to keep working when everything above it is
 * broken — the shell is a web page, and if it never loads, these are the only
 * keys that still do anything. What it does with a chord that is defined twice
 * is therefore not a detail: it decides whether the config file a user just
 * edited has any effect at all, and it is invisible from the outside. A binding
 * that silently loses to a built-in default looks exactly like a binding that
 * failed to parse, and neither logs anything.
 *
 * Nothing here runs an action. run_action() spawns processes and terminates
 * compositors; what is under test is which entry the matcher would reach, and
 * struct viewport_binding is public, so the list can simply be read. The nine
 * stubs at the bottom exist because binding.c calls into the rest of the
 * compositor from run_action() alone, which these tests never enter.
 */
#define _POSIX_C_SOURCE 200809L

#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <glib.h>
#include <glib/gstdio.h>

#include <xkbcommon/xkbcommon.h>

#include "viewport-view.h"
#include "viewport-input.h"
#include "viewport-shell.h"

static int failures;

static void check(const char *what, bool ok)
{
	printf("%s %s\n", ok ? "ok   " : "not ok", what);
	if (!ok) {
		failures++;
	}
}

/* The binding the matcher would reach first for this chord, or NULL.
 *
 * Deliberately a copy of viewport_bindings_handle()'s search rather than a call
 * to it: that function runs what it finds, and the whole point here is to look
 * without touching. Both walk the list front to back and stop at the first
 * match, so a change that breaks one breaks this too. */
static struct viewport_binding *first_match(struct viewport_server *server,
	uint32_t modifiers, xkb_keysym_t keysym, const char *mode)
{
	struct viewport_binding *binding;
	wl_list_for_each(binding, &server->bindings, link) {
		if (strcmp(binding->mode, mode != NULL ? mode : "default") == 0 &&
				binding->modifiers == modifiers &&
				binding->keysym == keysym) {
			return binding;
		}
	}
	return NULL;
}

static int count_matches(struct viewport_server *server, uint32_t modifiers,
	xkb_keysym_t keysym)
{
	int found = 0;
	struct viewport_binding *binding;
	wl_list_for_each(binding, &server->bindings, link) {
		if (binding->modifiers == modifiers && binding->keysym == keysym) {
			found++;
		}
	}
	return found;
}

static void server_init(struct viewport_server *server)
{
	memset(server, 0, sizeof(*server));
	wl_list_init(&server->bindings);
}

static void server_finish(struct viewport_server *server)
{
	viewport_bindings_finish(server);
}

/* A chord the defaults definitely bind, so "does a user bind beat a default"
 * is being asked about a real collision rather than a hypothetical one. */
#define DEFAULT_CHORD_MODS  WLR_MODIFIER_LOGO
#define DEFAULT_CHORD_SYM   XKB_KEY_Return

static void test_user_bind_beats_default(void)
{
	struct viewport_server server;
	server_init(&server);

	/* The order main.c uses: whatever the user asked for, then the built-ins
	 * filling in what they did not mention. */
	check("a user bind parses",
		viewport_binding_add(&server, "Mod4+Return=exec user-terminal"));
	viewport_bindings_add_defaults(&server, "default-terminal", NULL);

	struct viewport_binding *winner = first_match(&server, DEFAULT_CHORD_MODS,
		DEFAULT_CHORD_SYM, "default");

	check("Mod4+Return is bound at all", winner != NULL);
	check("the user's bind wins over the built-in default",
		winner != NULL && winner->argument != NULL &&
		strcmp(winner->argument, "user-terminal") == 0);
	check("and the default is not left behind as a dead entry",
		count_matches(&server, DEFAULT_CHORD_MODS, DEFAULT_CHORD_SYM) == 1);

	server_finish(&server);
}

static void test_defaults_still_land(void)
{
	struct viewport_server server;
	server_init(&server);

	viewport_binding_add(&server, "Mod4+Return=exec user-terminal");
	viewport_bindings_add_defaults(&server, "default-terminal", NULL);

	/* Overriding one chord must not cost the other hundred. */
	check("an unrelated default is still bound",
		first_match(&server, WLR_MODIFIER_LOGO | WLR_MODIFIER_SHIFT,
			XKB_KEY_e, "default") != NULL);
	check("and so is a mode-qualified one",
		first_match(&server, 0, XKB_KEY_h, "resize") != NULL);

	server_finish(&server);
}

static void test_unbind_removes_a_default(void)
{
	struct viewport_server server;
	server_init(&server);

	check("an unbind parses",
		viewport_binding_add(&server, "Mod4+Return=none"));
	viewport_bindings_add_defaults(&server, "default-terminal", NULL);

	struct viewport_binding *winner = first_match(&server, DEFAULT_CHORD_MODS,
		DEFAULT_CHORD_SYM, "default");

	check("an unbound chord keeps the marker rather than the default",
		winner != NULL && winner->action == VIEWPORT_ACTION_NONE);
	check("and the default did not sneak in behind it",
		count_matches(&server, DEFAULT_CHORD_MODS, DEFAULT_CHORD_SYM) == 1);

	/* The point of unbinding is that the key reaches the application. A
	 * consumed chord would be worse than the default it replaced. */
	const xkb_keysym_t syms[] = { DEFAULT_CHORD_SYM };
	check("and the key is not consumed, so the client sees it",
		!viewport_bindings_handle(&server, DEFAULT_CHORD_MODS, syms, 1));

	server_finish(&server);
}

static void test_later_user_bind_wins(void)
{
	struct viewport_server server;
	server_init(&server);

	viewport_binding_add(&server, "Mod4+g=exec first");
	viewport_binding_add(&server, "Mod4+g=exec second");

	struct viewport_binding *winner =
		first_match(&server, WLR_MODIFIER_LOGO, XKB_KEY_g, "default");
	check("the last of two user binds for one chord wins",
		winner != NULL && winner->argument != NULL &&
		strcmp(winner->argument, "second") == 0);

	server_finish(&server);
}

static void test_modes_are_separate_namespaces(void)
{
	struct viewport_server server;
	server_init(&server);

	viewport_binding_add(&server, "resize/Return=exec in-resize-mode");
	viewport_bindings_add_defaults(&server, "default-terminal", NULL);

	/* A chord bound in one mode must not shadow the same chord in another:
	 * `resize/Return` and `Mod4+Return` are unrelated bindings. */
	struct viewport_binding *def = first_match(&server, DEFAULT_CHORD_MODS,
		DEFAULT_CHORD_SYM, "default");
	check("a mode-qualified bind does not shadow the default mode",
		def != NULL && def->argument != NULL &&
		strcmp(def->argument, "default-terminal") == 0);

	server_finish(&server);
}

static void test_malformed_binds_are_refused(void)
{
	struct viewport_server server;
	server_init(&server);

	check("a bind with no '=' is refused",
		!viewport_binding_add(&server, "Mod4+Return"));
	check("a bind with an empty chord is refused",
		!viewport_binding_add(&server, "=close"));
	check("an unknown modifier is refused",
		!viewport_binding_add(&server, "Nope+Return=close"));
	check("an unknown key is refused",
		!viewport_binding_add(&server, "Mod4+NotAKey=close"));
	check("an unknown action is refused",
		!viewport_binding_add(&server, "Mod4+Return=frobnicate"));
	check("exec with no command is refused",
		!viewport_binding_add(&server, "Mod4+Return=exec "));
	check("nothing was added by any of those",
		wl_list_empty(&server.bindings));

	server_finish(&server);
}

/* ------------------------------------------------------------------------
 * Through the config file
 *
 * The tests above drive viewport_binding_add() directly. These go through the
 * parser, because "binds_override" is a config-file concept: what it has to get
 * right is which of the three keys — binds, binds_override, neither — leaves
 * the built-in keymap standing.
 * --------------------------------------------------------------------- */

static void load_config(struct viewport_server *server, const char *json)
{
	char *path = g_strdup_printf("%s/viewport-test-config.json",
		g_get_tmp_dir());
	GError *error = NULL;
	if (!g_file_set_contents(path, json, -1, &error)) {
		fprintf(stderr, "not ok  could not write %s: %s\n", path,
			error != NULL ? error->message : "failed");
		exit(1);
	}

	viewport_config_load(server, &server->config, path, true);

	/* What main.c does next, and the half that matters here. */
	if (!server->config.binds_from_config) {
		viewport_bindings_add_defaults(server, "default-terminal", NULL);
	}

	g_unlink(path);
	g_free(path);
}

static void test_override_keeps_the_rest_of_the_defaults(void)
{
	struct viewport_server server;
	server_init(&server);

	load_config(&server,
		"{\"binds_override\":{\"Mod4+Return\":\"exec my-terminal\"}}");

	struct viewport_binding *winner = first_match(&server, DEFAULT_CHORD_MODS,
		DEFAULT_CHORD_SYM, "default");
	check("binds_override replaces the chord it names",
		winner != NULL && winner->argument != NULL &&
		strcmp(winner->argument, "my-terminal") == 0);
	check("and only that chord: an unrelated default survives",
		first_match(&server, WLR_MODIFIER_LOGO | WLR_MODIFIER_SHIFT,
			XKB_KEY_e, "default") != NULL);
	check("and the built-in it replaced is gone rather than shadowed",
		count_matches(&server, DEFAULT_CHORD_MODS, DEFAULT_CHORD_SYM) == 1);

	server_finish(&server);
}

static void test_override_null_unbinds(void)
{
	struct viewport_server server;
	server_init(&server);

	load_config(&server, "{\"binds_override\":{\"Mod4+Return\":null}}");

	struct viewport_binding *winner = first_match(&server, DEFAULT_CHORD_MODS,
		DEFAULT_CHORD_SYM, "default");
	check("a null in binds_override unbinds the default",
		winner != NULL && winner->action == VIEWPORT_ACTION_NONE);

	const xkb_keysym_t syms[] = { DEFAULT_CHORD_SYM };
	check("so the chord reaches the application",
		!viewport_bindings_handle(&server, DEFAULT_CHORD_MODS, syms, 1));
	check("and the rest of the keymap is untouched",
		first_match(&server, WLR_MODIFIER_LOGO | WLR_MODIFIER_SHIFT,
			XKB_KEY_e, "default") != NULL);

	server_finish(&server);
}

static void test_binds_still_replaces_wholesale(void)
{
	struct viewport_server server;
	server_init(&server);

	/* The existing contract, which binds_override must not have changed: a
	 * "binds" object is the whole keymap. */
	load_config(&server, "{\"binds\":{\"Mod4+Return\":\"exec only-this\"}}");

	check("binds suppresses the built-ins", server.config.binds_from_config);
	check("so an unrelated default is not bound",
		first_match(&server, WLR_MODIFIER_LOGO | WLR_MODIFIER_SHIFT,
			XKB_KEY_e, "default") == NULL);
	check("and only what was asked for is",
		first_match(&server, DEFAULT_CHORD_MODS, DEFAULT_CHORD_SYM,
			"default") != NULL);

	server_finish(&server);
}

static void test_empty_binds_means_no_keymap(void)
{
	struct viewport_server server;
	server_init(&server);

	load_config(&server, "{\"binds\":{}}");

	check("an empty binds object still means no keymap at all",
		wl_list_empty(&server.bindings));

	server_finish(&server);
}

static void test_override_rejects_a_non_string_action(void)
{
	struct viewport_server server;
	server_init(&server);

	/* A number is a mistake, not an unbind. Treating it as one would leave the
	 * user with a chord that silently does nothing. */
	load_config(&server, "{\"binds_override\":{\"Mod4+Return\":5}}");

	struct viewport_binding *winner = first_match(&server, DEFAULT_CHORD_MODS,
		DEFAULT_CHORD_SYM, "default");
	check("a non-string action is refused, leaving the default in place",
		winner != NULL && winner->action == VIEWPORT_ACTION_EXEC &&
		winner->argument != NULL &&
		strcmp(winner->argument, "default-terminal") == 0);

	server_finish(&server);
}

int main(void)
{
	test_user_bind_beats_default();
	test_defaults_still_land();
	test_unbind_removes_a_default();
	test_later_user_bind_wins();
	test_modes_are_separate_namespaces();
	test_malformed_binds_are_refused();

	test_override_keeps_the_rest_of_the_defaults();
	test_override_null_unbinds();
	test_binds_still_replaces_wholesale();
	test_empty_binds_means_no_keymap();
	test_override_rejects_a_non_string_action();

	viewport_config_finish();

	printf("%s %d failure(s)\n", failures == 0 ? "ok   " : "not ok", failures);
	return failures == 0 ? 0 : 1;
}

/* ------------------------------------------------------------------------
 * Stubs
 *
 * Everything binding.c reaches outside itself, all of it from run_action(),
 * which no test above enters. If a test ever does, these are what it will hit,
 * so they are loud rather than silent.
 * --------------------------------------------------------------------- */

static void unreachable(const char *name)
{
	fprintf(stderr, "not ok  %s was called; no test should run an action\n",
		name);
	exit(1);
}

void viewport_appearance_set_dark(struct viewport_appearance *a, bool dark)
{
	(void)a; (void)dark; unreachable("viewport_appearance_set_dark");
}

bool viewport_appearance_is_dark(struct viewport_appearance *a)
{
	(void)a; unreachable("viewport_appearance_is_dark"); return false;
}

/* viewport_config_reload() is deliberately absent: config.c is linked in for
 * the parser, so the real one is already here. */

void viewport_focus_direction(struct viewport_server *s, const char *d)
{
	(void)s; (void)d; unreachable("viewport_focus_direction");
}

void viewport_idle_blank(struct viewport_server *s)
{
	(void)s; unreachable("viewport_idle_blank");
}

void viewport_ipc_notify_shell_command(struct viewport_server *s,
	const char *command)
{
	(void)s; (void)command; unreachable("viewport_ipc_notify_shell_command");
}

void viewport_server_terminate(struct viewport_server *s)
{
	(void)s; unreachable("viewport_server_terminate");
}

void viewport_toplevel_close(struct viewport_toplevel *t)
{
	(void)t; unreachable("viewport_toplevel_close");
}

void viewport_web_reload(struct viewport_web *w)
{
	(void)w; unreachable("viewport_web_reload");
}

/* Reached by viewport_config_load()'s callers rather than by run_action(), so
 * these are quiet no-ops: the config tests do load a file, and none of what
 * these would do is what those tests are asking about. */
void viewport_idle_init(struct viewport_server *s) { (void)s; }
void viewport_idle_finish(struct viewport_server *s) { (void)s; }
void viewport_keyboards_reconfigure(struct viewport_server *s) { (void)s; }
void viewport_ipc_notify_config(struct viewport_server *s) { (void)s; }
