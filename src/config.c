/* SPDX-License-Identifier: MIT
 *
 * Bootstrap configuration.
 *
 * This is the tier that cannot live in the web shell. The shell is fetched
 * over the network; if it 404s, throws, or hangs, anything it owned dies with
 * it. Keybindings defined here still work in that state, which is the
 * difference between "the desktop UI is broken" and "the machine is bricked
 * until you switch to a TTY".
 *
 * The shell may still add bindings at runtime over IPC (bind.add / bind.clear)
 * — that layer is additive and expendable. This one is the floor.
 *
 * Precedence: command-line flags > config file > built-in defaults.
 *
 *   ~/.config/viewport/config.json
 *   {
 *     "url": "http://localhost:3000",
 *     "timeout_ms": 5000,
 *     "terminal": "ghostty",
 *     "menu": "wmenu-run -f 'Fira Code NerdFont 11' -i",
 *     "binds": {
 *       "Mod4+Return":   "exec ghostty",
 *       "Mod4+d":        "exec wmenu-run",
 *       "Mod4+Shift+q":  "close",
 *       "Mod4+Shift+e":  "exit",
 *       "Mod4+Shift+c":  "reload"
 *     }
 *   }
 */
#define _POSIX_C_SOURCE 200809L

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <glib.h>
#include <json-glib/json-glib.h>

#include <wlr/util/log.h>

#include "viewport.h"

/* Strings handed to viewport_config are owned here and freed by
 * viewport_config_finish(). */
static char *config_strings[8];
static size_t config_string_count;

static const char *keep(char *owned)
{
	if (owned == NULL) {
		return NULL;
	}
	if (config_string_count < sizeof(config_strings) / sizeof(config_strings[0])) {
		config_strings[config_string_count++] = owned;
		return owned;
	}
	/* Should never happen: the table is sized for every field below. */
	return owned;
}

char *viewport_config_default_path(void)
{
	const char *explicit_dir = getenv("XDG_CONFIG_HOME");
	if (explicit_dir != NULL && explicit_dir[0] != '\0') {
		return g_strdup_printf("%s/viewport/config.json", explicit_dir);
	}

	const char *home = getenv("HOME");
	if (home == NULL) {
		return NULL;
	}
	return g_strdup_printf("%s/.config/viewport/config.json", home);
}

bool viewport_config_load(struct viewport_server *server,
	struct viewport_config *config, const char *path, bool required)
{
	JsonParser *parser = json_parser_new();
	GError *error = NULL;

	if (!json_parser_load_from_file(parser, path, &error)) {
		/* A missing default config is normal; a missing --config is not. */
		if (required) {
			wlr_log(WLR_ERROR, "config %s: %s", path, error->message);
		} else {
			wlr_log(WLR_DEBUG, "no config at %s, using defaults", path);
		}
		g_error_free(error);
		g_object_unref(parser);
		return false;
	}

	JsonNode *root = json_parser_get_root(parser);
	if (root == NULL || json_node_get_node_type(root) != JSON_NODE_OBJECT) {
		wlr_log(WLR_ERROR, "config %s: top level must be an object", path);
		g_object_unref(parser);
		return false;
	}

	JsonObject *object = json_node_get_object(root);

	if (json_object_has_member(object, "url")) {
		config->url = keep(g_strdup(
			json_object_get_string_member(object, "url")));
	}
	if (json_object_has_member(object, "fallback")) {
		config->fallback_url = keep(g_strdup(
			json_object_get_string_member(object, "fallback")));
	}
	if (json_object_has_member(object, "timeout_ms")) {
		config->load_timeout_ms =
			(unsigned)json_object_get_int_member(object, "timeout_ms");
	}
	if (json_object_has_member(object, "terminal")) {
		config->terminal = keep(g_strdup(
			json_object_get_string_member(object, "terminal")));
	}
	if (json_object_has_member(object, "dark_mode")) {
		config->dark_mode = json_object_get_boolean_member(object, "dark_mode");
	}
	if (json_object_has_member(object, "decorations")) {
		const char *mode = json_object_get_string_member(object, "decorations");
		config->server_decorations = (mode == NULL || strcmp(mode, "client") != 0);
	}
	if (json_object_has_member(object, "menu")) {
		config->menu = keep(g_strdup(
			json_object_get_string_member(object, "menu")));
	}

	/* Binds are applied against the live server, so this runs after
	 * viewport_server_init(). An empty "binds": {} is meaningful — it means
	 * "no defaults", which is why presence is what suppresses them. */
	if (json_object_has_member(object, "binds")) {
		JsonObject *binds = json_object_get_object_member(object, "binds");
		GList *chords = json_object_get_members(binds);

		for (GList *item = chords; item != NULL; item = item->next) {
			const char *chord = item->data;
			const char *action = json_object_get_string_member(binds, chord);
			if (action == NULL) {
				continue;
			}
			char *spec = g_strdup_printf("%s=%s", chord, action);
			viewport_binding_add(server, spec);
			g_free(spec);
		}

		g_list_free(chords);
		config->binds_from_config = true;
	}

	wlr_log(WLR_INFO, "loaded config from %s", path);
	g_object_unref(parser);
	return true;
}

void viewport_config_finish(void)
{
	for (size_t i = 0; i < config_string_count; i++) {
		g_free(config_strings[i]);
	}
	config_string_count = 0;
}
