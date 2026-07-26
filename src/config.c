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

#include <wlr/types/wlr_xcursor_manager.h>
#include <wlr/util/log.h>

#include "viewport.h"

/* Strings handed to viewport_config are owned here and freed by
 * viewport_config_finish().
 *
 * Growable rather than a fixed table: the config can be reloaded at runtime,
 * and each reload allocates a fresh set. A fixed array would silently start
 * leaking the moment it filled. Strings from previous loads are kept until
 * shutdown — they are a handful of bytes each, and freeing them at reload time
 * would dangle every config field that the new file did not mention. */
static GPtrArray *config_strings;

static const char *keep(char *owned)
{
	if (owned == NULL) {
		return NULL;
	}
	if (config_strings == NULL) {
		config_strings = g_ptr_array_new_with_free_func(g_free);
	}
	g_ptr_array_add(config_strings, owned);
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
	/* Per-output mode preferences.
	 *
	 *   "outputs": { "DP-1": { "max_refresh": true },
	 *                "DP-3": { "mode": "2560x1440@239.760" } }
	 *
	 * A "*" entry applies to any output the rest do not name. */
	if (json_object_has_member(object, "outputs")) {
		JsonObject *outputs = json_object_get_object_member(object, "outputs");
		GList *names = json_object_get_members(outputs);

		size_t count = g_list_length(names);
		struct viewport_output_config *parsed = count > 0
			? g_new0(struct viewport_output_config, count) : NULL;
		size_t at = 0;

		for (GList *item = names; item != NULL && parsed != NULL;
				item = item->next) {
			const char *name = item->data;
			JsonObject *entry = json_object_get_object_member(outputs, name);

			parsed[at].name = keep(g_strdup(name));
			if (json_object_has_member(entry, "max_refresh")) {
				parsed[at].max_refresh =
					json_object_get_boolean_member(entry, "max_refresh");
			}
			if (json_object_has_member(entry, "mode")) {
				/* "2560x1440@239.760", or "2560x1440" to match on resolution
				 * alone. Refresh is stored in mHz, which is what wlroots and
				 * DRM both use. */
				const char *text =
					json_object_get_string_member(entry, "mode");
				double refresh = 0.0;
				int width = 0, height = 0;
				int fields = sscanf(text, "%dx%d@%lf", &width, &height,
					&refresh);
				if (fields >= 2) {
					parsed[at].width = width;
					parsed[at].height = height;
					parsed[at].refresh = (int)(refresh * 1000.0 + 0.5);
				} else {
					wlr_log(WLR_ERROR,
						"output %s: cannot read mode '%s', expected "
						"WIDTHxHEIGHT[@REFRESH]", name, text);
				}
			}
			at++;
		}

		g_list_free(names);
		if (parsed != NULL) {
			/* A reload parses afresh, so the previous array goes now. The names
			 * inside it are owned by the string table and outlive it. */
			g_free(config->outputs);
			config->outputs = parsed;
			config->output_count = at;
		}
	}

	/* Window rules travel to the shell verbatim rather than being parsed here.
	 * Which workspace a window opens on, whether it floats and how wide its
	 * column is are all layout decisions, and the compositor has no opinion
	 * about any of them — it would only be reassembling the JSON it was handed
	 * in order to hand it on. */
	if (json_object_has_member(object, "rules")) {
		JsonNode *rules = json_object_get_member(object, "rules");
		JsonGenerator *generator = json_generator_new();
		json_generator_set_root(generator, rules);
		config->rules_json = keep(json_generator_to_data(generator, NULL));
		g_object_unref(generator);
	}
	/* Same reasoning as the rules: these become CSS custom properties in the
	 * shell, and nothing here would do anything with them but pass them on. */
	if (json_object_has_member(object, "theme")) {
		JsonNode *theme = json_object_get_member(object, "theme");
		JsonGenerator *generator = json_generator_new();
		json_generator_set_root(generator, theme);
		config->theme_json = keep(json_generator_to_data(generator, NULL));
		g_object_unref(generator);
	}
	if (json_object_has_member(object, "logo")) {
		config->logo = json_object_get_boolean_member(object, "logo");
	}
	if (json_object_has_member(object, "tutorial")) {
		config->tutorial = json_object_get_boolean_member(object, "tutorial");
	}
	if (json_object_has_member(object, "bar")) {
		config->bar = keep(g_strdup(
			json_object_get_string_member(object, "bar")));
	}
	if (json_object_has_member(object, "idle")) {
		JsonObject *idle = json_object_get_object_member(object, "idle");
		if (json_object_has_member(idle, "lock_after")) {
			config->idle_lock_after =
				(int)json_object_get_int_member(idle, "lock_after");
		}
		if (json_object_has_member(idle, "blank_after")) {
			config->idle_blank_after =
				(int)json_object_get_int_member(idle, "blank_after");
		}
		if (json_object_has_member(idle, "lock_command")) {
			config->idle_lock_command = keep(g_strdup(
				json_object_get_string_member(idle, "lock_command")));
		}
	}
	if (json_object_has_member(object, "adaptive_sync")) {
		config->adaptive_sync =
			json_object_get_boolean_member(object, "adaptive_sync");
	}
	if (json_object_has_member(object, "layout")) {
		config->layout = keep(g_strdup(
			json_object_get_string_member(object, "layout")));
	}
	if (json_object_has_member(object, "cursor")) {
		JsonObject *cursor = json_object_get_object_member(object, "cursor");
		if (json_object_has_member(cursor, "theme")) {
			config->cursor_theme = keep(g_strdup(
				json_object_get_string_member(cursor, "theme")));
		}
		if (json_object_has_member(cursor, "size")) {
			config->cursor_size =
				(int)json_object_get_int_member(cursor, "size");
		}
	}
	if (json_object_has_member(object, "keyboard")) {
		JsonObject *keyboard = json_object_get_object_member(object, "keyboard");
		if (json_object_has_member(keyboard, "layout")) {
			config->xkb_layout = keep(g_strdup(
				json_object_get_string_member(keyboard, "layout")));
		}
		if (json_object_has_member(keyboard, "variant")) {
			config->xkb_variant = keep(g_strdup(
				json_object_get_string_member(keyboard, "variant")));
		}
		if (json_object_has_member(keyboard, "options")) {
			config->xkb_options = keep(g_strdup(
				json_object_get_string_member(keyboard, "options")));
		}
		if (json_object_has_member(keyboard, "repeat_rate")) {
			config->repeat_rate =
				(int)json_object_get_int_member(keyboard, "repeat_rate");
		}
		if (json_object_has_member(keyboard, "repeat_delay")) {
			config->repeat_delay =
				(int)json_object_get_int_member(keyboard, "repeat_delay");
		}
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

/* Re-read the config file into a running compositor.
 *
 * Only the keys the file actually contains are applied, so a reload never
 * resets something back to a built-in default that a command-line flag had
 * set. That does mean a key present in the file wins over the flag on reload,
 * which is the behaviour worth having: the file is what the user just edited.
 *
 * Startup-only settings — the shell URL, the backend, the IPC socket path —
 * are re-read into the config struct but have no effect until restart. There
 * is nothing sensible to do about a changed socket path in a live session. */
void viewport_config_reload(struct viewport_server *server)
{
	char *path = server->config_path != NULL
		? g_strdup(server->config_path) : viewport_config_default_path();
	if (path == NULL) {
		return;
	}

	/* Bindings are rebuilt from scratch: a reload that only added would leave
	 * every binding the user just deleted still working. */
	viewport_bindings_finish(server);
	wl_list_init(&server->bindings);
	server->config.binds_from_config = false;

	bool loaded = viewport_config_load(server, &server->config, path,
		server->config_path != NULL);
	if (!loaded || !server->config.binds_from_config) {
		viewport_bindings_add_defaults(server, server->config.terminal,
			server->config.menu);
	}

	/* Thresholds may have changed, so the policy is rebuilt rather than left
	 * running with the old ones. */
	viewport_idle_finish(server);
	viewport_idle_init(server);

	viewport_keyboards_reconfigure(server);
	viewport_appearance_set_dark(server->appearance, server->config.dark_mode);
	if (server->xcursor_mgr != NULL && server->config.cursor_size > 0) {
		/* A new theme or size only reaches the pointer once a manager for it
		 * exists; the old one stays alive because the cursor may still be
		 * showing an image from it. */
		struct wlr_xcursor_manager *manager = wlr_xcursor_manager_create(
			server->config.cursor_theme, server->config.cursor_size);
		if (manager != NULL) {
			server->xcursor_mgr = manager;
		}
	}

	/* The shell decides layout, so it has to hear about a changed model. */
	viewport_ipc_notify_config(server);

	g_free(path);
	wlr_log(WLR_INFO, "config reloaded");
}

const struct viewport_output_config *viewport_output_config_for(
	struct viewport_server *server, const char *name)
{
	const struct viewport_output_config *wildcard = NULL;

	for (size_t i = 0; i < server->config.output_count; i++) {
		const struct viewport_output_config *entry = &server->config.outputs[i];
		if (entry->name == NULL) {
			continue;
		}
		if (strcmp(entry->name, name) == 0) {
			/* An exact name beats the wildcard, whichever order they were
			 * written in. */
			return entry;
		}
		if (strcmp(entry->name, "*") == 0) {
			wildcard = entry;
		}
	}

	return wildcard;
}

void viewport_config_finish(void)
{
	g_clear_pointer(&config_strings, g_ptr_array_unref);
}
