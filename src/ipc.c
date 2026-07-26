/* SPDX-License-Identifier: MIT
 *
 * The control channel.
 *
 * One JSON dispatch table, two transports:
 *
 *   the page      window.webkit.messageHandlers.viewport.postMessage(), with
 *                 replies delivered as a `viewport` CustomEvent. This is the
 *                 transport that matters — it works regardless of the shell's
 *                 origin, needs no port, and is unaffected by CORS or mixed
 *                 content rules when the shell is served over HTTPS.
 *
 *   a UNIX socket $XDG_RUNTIME_DIR/viewport-<display>.sock, newline-delimited
 *                 JSON. Same message set, for scripting and for debugging the
 *                 shell without a browser attached:
 *                     socat - UNIX:$XDG_RUNTIME_DIR/viewport-wayland-1.sock
 *
 * Messages are documented in README.md.
 */
#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <json-glib/json-glib.h>

#include <wlr/types/wlr_output.h>
#include <wlr/util/log.h>

#include "viewport.h"

struct viewport_ipc {
	struct viewport_server *server;
	int fd;
	char *path;
	struct wl_event_source *source;
	struct wl_list clients; /* ipc_client.link */
};

struct ipc_client {
	struct wl_list link;
	struct viewport_ipc *ipc;
	int fd;
	struct wl_event_source *source;

	/* Accumulates a partial line across reads. */
	char *buf;
	size_t len;
	size_t cap;
};

/* ------------------------------------------------------------------------
 * Outbound
 * --------------------------------------------------------------------- */

static void ipc_client_destroy(struct ipc_client *client)
{
	wl_list_remove(&client->link);
	if (client->source != NULL) {
		wl_event_source_remove(client->source);
	}
	close(client->fd);
	free(client->buf);
	free(client);
}

void viewport_ipc_broadcast(struct viewport_server *server, const char *json)
{
	if (server->web != NULL) {
		viewport_web_post_to_page(server->web, json);
	}

	if (server->ipc == NULL) {
		return;
	}

	size_t len = strlen(json);
	struct ipc_client *client, *tmp;
	wl_list_for_each_safe(client, tmp, &server->ipc->clients, link) {
		/* Non-blocking: a client that will not drain is a client we drop,
		 * rather than one that stalls the compositor. */
		if (write(client->fd, json, len) < 0 ||
				write(client->fd, "\n", 1) < 0) {
			if (errno != EAGAIN && errno != EWOULDBLOCK) {
				ipc_client_destroy(client);
			}
		}
	}
}

static void broadcast_builder(struct viewport_server *server,
	JsonBuilder *builder)
{
	JsonGenerator *generator = json_generator_new();
	JsonNode *root = json_builder_get_root(builder);
	json_generator_set_root(generator, root);

	char *text = json_generator_to_data(generator, NULL);
	viewport_ipc_broadcast(server, text);

	g_free(text);
	json_node_free(root);
	g_object_unref(generator);
}

/* Name of the output a new window should open on.
 *
 * The shell lays out per output; without this it has no way to know which
 * screen a window belongs to and ends up stretching one window across the
 * whole multi-monitor layout. The output under the cursor is the closest thing
 * to "where the user is looking". */
static const char *output_for_new_view(struct viewport_server *server)
{
	/* The shell's active output wins. Deciding from the cursor alone puts a
	 * window on the monitor the mouse happens to sit on, which is wrong the
	 * moment focus was moved with the keyboard — Mod4+l to the other monitor
	 * then launching something would open it back where the pointer was. */
	if (server->active_output != NULL) {
		return server->active_output;
	}

	struct wlr_output *wlr_output = wlr_output_layout_output_at(
		server->output_layout, server->cursor->x, server->cursor->y);
	if (wlr_output != NULL) {
		return wlr_output->name;
	}
	if (!wl_list_empty(&server->outputs)) {
		struct viewport_output *first =
			wl_container_of(server->outputs.next, first, link);
		return first->wlr_output->name;
	}
	return "";
}

void viewport_ipc_notify_view_added(struct viewport_toplevel *toplevel)
{
	JsonBuilder *builder = json_builder_new();
	json_builder_begin_object(builder);
	json_builder_set_member_name(builder, "type");
	json_builder_add_string_value(builder, "view.added");
	json_builder_set_member_name(builder, "id");
	json_builder_add_int_value(builder, toplevel->id);
	json_builder_set_member_name(builder, "title");
	json_builder_add_string_value(builder,
		toplevel->xdg_toplevel->title ? toplevel->xdg_toplevel->title : "");
	json_builder_set_member_name(builder, "app_id");
	json_builder_add_string_value(builder,
		toplevel->xdg_toplevel->app_id ? toplevel->xdg_toplevel->app_id : "");
	json_builder_set_member_name(builder, "output");
	json_builder_add_string_value(builder,
		output_for_new_view(toplevel->server));
	/* So the shell can refuse to shrink a window past what it accepts. */
	json_builder_set_member_name(builder, "min_width");
	json_builder_add_int_value(builder,
		toplevel->xdg_toplevel->current.min_width);
	json_builder_set_member_name(builder, "min_height");
	json_builder_add_int_value(builder,
		toplevel->xdg_toplevel->current.min_height);
	json_builder_end_object(builder);

	broadcast_builder(toplevel->server, builder);
	g_object_unref(builder);
}

void viewport_ipc_notify_view_removed(struct viewport_toplevel *toplevel)
{
	JsonBuilder *builder = json_builder_new();
	json_builder_begin_object(builder);
	json_builder_set_member_name(builder, "type");
	json_builder_add_string_value(builder, "view.removed");
	json_builder_set_member_name(builder, "id");
	json_builder_add_int_value(builder, toplevel->id);
	json_builder_end_object(builder);

	broadcast_builder(toplevel->server, builder);
	g_object_unref(builder);
}

void viewport_ipc_notify_view_props(struct viewport_toplevel *toplevel)
{
	JsonBuilder *builder = json_builder_new();
	json_builder_begin_object(builder);
	json_builder_set_member_name(builder, "type");
	json_builder_add_string_value(builder, "view.props");
	json_builder_set_member_name(builder, "id");
	json_builder_add_int_value(builder, toplevel->id);
	json_builder_set_member_name(builder, "title");
	json_builder_add_string_value(builder,
		toplevel->xdg_toplevel->title ? toplevel->xdg_toplevel->title : "");
	json_builder_set_member_name(builder, "app_id");
	json_builder_add_string_value(builder,
		toplevel->xdg_toplevel->app_id ? toplevel->xdg_toplevel->app_id : "");
	json_builder_end_object(builder);

	broadcast_builder(toplevel->server, builder);
	g_object_unref(builder);
}

void viewport_ipc_notify_views(struct viewport_server *server)
{
	struct viewport_toplevel *toplevel;
	wl_list_for_each(toplevel, &server->toplevels, link) {
		if (toplevel->mapped) {
			viewport_ipc_notify_view_added(toplevel);
		}
	}
}

void viewport_ipc_notify_focus(struct viewport_server *server, uint32_t id)
{
	JsonBuilder *builder = json_builder_new();
	json_builder_begin_object(builder);
	json_builder_set_member_name(builder, "type");
	json_builder_add_string_value(builder, "view.focused");
	json_builder_set_member_name(builder, "id");
	json_builder_add_int_value(builder, id);
	json_builder_end_object(builder);

	broadcast_builder(server, builder);
	g_object_unref(builder);
}

void viewport_ipc_notify_shell_command(struct viewport_server *server,
	const char *command)
{
	/* Split on whitespace: "workspace.switch 3" becomes a command plus args,
	 * so the shell does not have to parse a free-form string. */
	char **parts = g_strsplit_set(command, " \t", -1);

	JsonBuilder *builder = json_builder_new();
	json_builder_begin_object(builder);
	json_builder_set_member_name(builder, "type");
	json_builder_add_string_value(builder, "shell.command");
	json_builder_set_member_name(builder, "command");
	json_builder_add_string_value(builder, parts[0] ? parts[0] : "");
	json_builder_set_member_name(builder, "args");
	json_builder_begin_array(builder);
	for (int i = 1; parts[i] != NULL; i++) {
		if (parts[i][0] != '\0') {
			json_builder_add_string_value(builder, parts[i]);
		}
	}
	json_builder_end_array(builder);
	json_builder_end_object(builder);

	broadcast_builder(server, builder);
	g_object_unref(builder);
	g_strfreev(parts);
}

static const char *transform_name(enum wl_output_transform transform)
{
	switch (transform) {
	case WL_OUTPUT_TRANSFORM_90: return "90";
	case WL_OUTPUT_TRANSFORM_180: return "180";
	case WL_OUTPUT_TRANSFORM_270: return "270";
	case WL_OUTPUT_TRANSFORM_FLIPPED: return "flipped";
	case WL_OUTPUT_TRANSFORM_FLIPPED_90: return "flipped-90";
	case WL_OUTPUT_TRANSFORM_FLIPPED_180: return "flipped-180";
	case WL_OUTPUT_TRANSFORM_FLIPPED_270: return "flipped-270";
	default: return "normal";
	}
}

static bool transform_from_name(const char *name,
	enum wl_output_transform *out)
{
	static const struct {
		const char *name;
		enum wl_output_transform value;
	} table[] = {
		{ "normal", WL_OUTPUT_TRANSFORM_NORMAL },
		{ "90", WL_OUTPUT_TRANSFORM_90 },
		{ "180", WL_OUTPUT_TRANSFORM_180 },
		{ "270", WL_OUTPUT_TRANSFORM_270 },
		{ "flipped", WL_OUTPUT_TRANSFORM_FLIPPED },
		{ "flipped-90", WL_OUTPUT_TRANSFORM_FLIPPED_90 },
		{ "flipped-180", WL_OUTPUT_TRANSFORM_FLIPPED_180 },
		{ "flipped-270", WL_OUTPUT_TRANSFORM_FLIPPED_270 },
	};

	for (size_t i = 0; i < sizeof(table) / sizeof(table[0]); i++) {
		if (strcmp(name, table[i].name) == 0) {
			*out = table[i].value;
			return true;
		}
	}
	return false;
}

/* Full description of every output, so the shell can render a display
 * settings panel without asking any follow-up questions. */
void viewport_ipc_notify_output_layout(struct viewport_server *server)
{
	JsonBuilder *builder = json_builder_new();
	json_builder_begin_object(builder);
	json_builder_set_member_name(builder, "type");
	json_builder_add_string_value(builder, "output.layout");
	json_builder_set_member_name(builder, "outputs");
	json_builder_begin_array(builder);

	struct viewport_output *output;
	wl_list_for_each(output, &server->outputs, link) {
		struct wlr_output *wlr_output = output->wlr_output;
		struct wlr_box box;
		wlr_output_layout_get_box(server->output_layout, wlr_output, &box);

		json_builder_begin_object(builder);

		json_builder_set_member_name(builder, "name");
		json_builder_add_string_value(builder, wlr_output->name);
		json_builder_set_member_name(builder, "make");
		json_builder_add_string_value(builder,
			wlr_output->make ? wlr_output->make : "");
		json_builder_set_member_name(builder, "model");
		json_builder_add_string_value(builder,
			wlr_output->model ? wlr_output->model : "");
		json_builder_set_member_name(builder, "serial");
		json_builder_add_string_value(builder,
			wlr_output->serial ? wlr_output->serial : "");

		json_builder_set_member_name(builder, "enabled");
		json_builder_add_boolean_value(builder, wlr_output->enabled);
		json_builder_set_member_name(builder, "x");
		json_builder_add_int_value(builder, box.x);
		json_builder_set_member_name(builder, "y");
		json_builder_add_int_value(builder, box.y);
		json_builder_set_member_name(builder, "width");
		json_builder_add_int_value(builder, box.width);
		json_builder_set_member_name(builder, "height");
		json_builder_add_int_value(builder, box.height);
		json_builder_set_member_name(builder, "scale");
		json_builder_add_double_value(builder, wlr_output->scale);
		json_builder_set_member_name(builder, "transform");
		json_builder_add_string_value(builder,
			transform_name(wlr_output->transform));

		json_builder_set_member_name(builder, "modes");
		json_builder_begin_array(builder);
		struct wlr_output_mode *mode;
		wl_list_for_each(mode, &wlr_output->modes, link) {
			json_builder_begin_object(builder);
			json_builder_set_member_name(builder, "width");
			json_builder_add_int_value(builder, mode->width);
			json_builder_set_member_name(builder, "height");
			json_builder_add_int_value(builder, mode->height);
			json_builder_set_member_name(builder, "refresh");
			json_builder_add_int_value(builder, mode->refresh);
			json_builder_set_member_name(builder, "preferred");
			json_builder_add_boolean_value(builder, mode->preferred);
			json_builder_set_member_name(builder, "current");
			json_builder_add_boolean_value(builder,
				mode == wlr_output->current_mode);
			json_builder_end_object(builder);
		}
		json_builder_end_array(builder);

		json_builder_end_object(builder);
	}

	json_builder_end_array(builder);
	json_builder_end_object(builder);

	broadcast_builder(server, builder);
	g_object_unref(builder);
}

static void notify_error(struct viewport_server *server, const char *context,
	const char *message)
{
	JsonBuilder *builder = json_builder_new();
	json_builder_begin_object(builder);
	json_builder_set_member_name(builder, "type");
	json_builder_add_string_value(builder, "error");
	json_builder_set_member_name(builder, "context");
	json_builder_add_string_value(builder, context);
	json_builder_set_member_name(builder, "message");
	json_builder_add_string_value(builder, message);
	json_builder_end_object(builder);

	broadcast_builder(server, builder);
	g_object_unref(builder);
}

/* ------------------------------------------------------------------------
 * Inbound
 * --------------------------------------------------------------------- */

static int object_int(JsonObject *object, const char *name, int fallback)
{
	if (!json_object_has_member(object, name)) {
		return fallback;
	}
	return (int)json_object_get_int_member(object, name);
}

static void handle_view_layout(struct viewport_server *server,
	JsonObject *object)
{
	uint32_t id = (uint32_t)object_int(object, "id", 0);
	struct viewport_toplevel *toplevel = viewport_server_find_toplevel(server, id);
	if (toplevel == NULL) {
		return;
	}

	struct wlr_box box = {
		.x = object_int(object, "x", toplevel->box.x),
		.y = object_int(object, "y", toplevel->box.y),
		.width = object_int(object, "width", toplevel->box.width),
		.height = object_int(object, "height", toplevel->box.height),
	};

	if (box.width <= 0 || box.height <= 0) {
		return;
	}

	viewport_toplevel_set_box(toplevel, &box);
}

static void handle_view_visible(struct viewport_server *server,
	JsonObject *object)
{
	uint32_t id = (uint32_t)object_int(object, "id", 0);
	struct viewport_toplevel *toplevel = viewport_server_find_toplevel(server, id);
	if (toplevel == NULL || !toplevel->mapped) {
		return;
	}

	bool visible = json_object_has_member(object, "visible")
		? json_object_get_boolean_member(object, "visible") : true;

	/* Recorded, not just applied: directional focus needs to know which
	 * windows are actually on screen, or Mod4+l lands on something parked on
	 * a hidden workspace. */
	toplevel->visible = visible;
	wlr_scene_node_set_enabled(&toplevel->scene_tree->node,
		visible && toplevel->has_box);
}

static void handle_output_configure(struct viewport_server *server,
	JsonObject *object)
{
	const char *name = json_object_has_member(object, "name")
		? json_object_get_string_member(object, "name") : NULL;
	if (name == NULL) {
		notify_error(server, "output.configure", "missing output name");
		return;
	}

	struct viewport_output *output = NULL, *iter;
	wl_list_for_each(iter, &server->outputs, link) {
		if (strcmp(iter->wlr_output->name, name) == 0) {
			output = iter;
			break;
		}
	}
	if (output == NULL) {
		notify_error(server, "output.configure", "no such output");
		return;
	}

	struct wlr_output *wlr_output = output->wlr_output;
	struct wlr_output_state state;
	wlr_output_state_init(&state);

	if (json_object_has_member(object, "enabled")) {
		wlr_output_state_set_enabled(&state,
			json_object_get_boolean_member(object, "enabled"));
	}

	if (json_object_has_member(object, "mode")) {
		JsonObject *mode_object = json_object_get_object_member(object, "mode");
		int width = object_int(mode_object, "width", 0);
		int height = object_int(mode_object, "height", 0);
		int refresh = object_int(mode_object, "refresh", 0);

		/* Prefer an exact modeline the display advertised; fall back to a
		 * custom mode so unusual panels stay configurable. */
		struct wlr_output_mode *match = NULL, *mode;
		wl_list_for_each(mode, &wlr_output->modes, link) {
			if (mode->width == width && mode->height == height &&
					(refresh == 0 || mode->refresh == refresh)) {
				match = mode;
				break;
			}
		}
		if (match != NULL) {
			wlr_output_state_set_mode(&state, match);
		} else if (width > 0 && height > 0) {
			wlr_output_state_set_custom_mode(&state, width, height, refresh);
		}
	}

	if (json_object_has_member(object, "scale")) {
		double scale = json_object_get_double_member(object, "scale");
		if (scale > 0.0) {
			wlr_output_state_set_scale(&state, (float)scale);
		}
	}

	if (json_object_has_member(object, "transform")) {
		enum wl_output_transform transform;
		if (transform_from_name(
				json_object_get_string_member(object, "transform"), &transform)) {
			wlr_output_state_set_transform(&state, transform);
		}
	}

	if (json_object_has_member(object, "adaptive_sync")) {
		wlr_output_state_set_adaptive_sync_enabled(&state,
			json_object_get_boolean_member(object, "adaptive_sync"));
	}

	/* Test before committing. A mode the hardware cannot drive fails here
	 * instead of blanking the screen the user is configuring it from. */
	if (!wlr_output_test_state(wlr_output, &state)) {
		wlr_output_state_finish(&state);
		notify_error(server, "output.configure", "configuration rejected");
		return;
	}

	bool ok = wlr_output_commit_state(wlr_output, &state);
	wlr_output_state_finish(&state);
	if (!ok) {
		notify_error(server, "output.configure", "commit failed");
		return;
	}

	if (json_object_has_member(object, "x") ||
			json_object_has_member(object, "y")) {
		struct wlr_box box;
		wlr_output_layout_get_box(server->output_layout, wlr_output, &box);
		wlr_output_layout_add(server->output_layout, wlr_output,
			object_int(object, "x", box.x), object_int(object, "y", box.y));
	}

	int width, height;
	viewport_layout_size(server, &width, &height);
	if (server->web != NULL) {
		viewport_web_resize(server->web, width, height);
	}
	viewport_ipc_notify_output_layout(server);
}

void viewport_ipc_handle(struct viewport_server *server, const char *json,
	size_t len)
{
	JsonParser *parser = json_parser_new();
	GError *error = NULL;

	if (!json_parser_load_from_data(parser, json, (gssize)len, &error)) {
		wlr_log(WLR_ERROR, "malformed IPC message: %s", error->message);
		g_error_free(error);
		g_object_unref(parser);
		return;
	}

	JsonNode *root = json_parser_get_root(parser);
	if (root == NULL || json_node_get_node_type(root) != JSON_NODE_OBJECT) {
		g_object_unref(parser);
		return;
	}

	JsonObject *object = json_node_get_object(root);
	if (!json_object_has_member(object, "type")) {
		g_object_unref(parser);
		return;
	}

	const char *type = json_object_get_string_member(object, "type");

	if (strcmp(type, "view.layout") == 0) {
		handle_view_layout(server, object);
	} else if (strcmp(type, "view.visible") == 0) {
		handle_view_visible(server, object);
	} else if (strcmp(type, "view.focus") == 0) {
		struct viewport_toplevel *toplevel = viewport_server_find_toplevel(
			server, (uint32_t)object_int(object, "id", 0));
		if (toplevel != NULL) {
			viewport_toplevel_focus(toplevel);
		}
	} else if (strcmp(type, "view.close") == 0) {
		struct viewport_toplevel *toplevel = viewport_server_find_toplevel(
			server, (uint32_t)object_int(object, "id", 0));
		if (toplevel != NULL) {
			viewport_toplevel_close(toplevel);
		}
	} else if (strcmp(type, "shell.focus") == 0) {
		viewport_focus_web(server);
	} else if (strcmp(type, "output.configure") == 0) {
		handle_output_configure(server, object);
	} else if (strcmp(type, "output.active") == 0) {
		if (json_object_has_member(object, "name")) {
			free(server->active_output);
			server->active_output = g_strdup(
				json_object_get_string_member(object, "name"));
		}
	} else if (strcmp(type, "output.query") == 0) {
		viewport_ipc_notify_output_layout(server);
	} else if (strcmp(type, "view.query") == 0) {
		viewport_ipc_notify_views(server);
	} else if (strcmp(type, "bind.add") == 0) {
		/* Runtime binds from the shell are additive and expendable; the ones
		 * that must survive a broken shell belong in the config file. */
		if (json_object_has_member(object, "chord") &&
				json_object_has_member(object, "action")) {
			char *spec = g_strdup_printf("%s=%s",
				json_object_get_string_member(object, "chord"),
				json_object_get_string_member(object, "action"));
			if (!viewport_binding_add(server, spec)) {
				notify_error(server, "bind.add", spec);
			}
			g_free(spec);
		}
	} else if (strcmp(type, "quit") == 0) {
		viewport_server_terminate(server);
	} else {
		wlr_log(WLR_DEBUG, "unknown IPC message type '%s'", type);
	}

	g_object_unref(parser);
}

/* ------------------------------------------------------------------------
 * UNIX socket transport
 * --------------------------------------------------------------------- */

static int handle_client_readable(int fd, uint32_t mask, void *data)
{
	struct ipc_client *client = data;

	if (mask & (WL_EVENT_HANGUP | WL_EVENT_ERROR)) {
		ipc_client_destroy(client);
		return 0;
	}

	char chunk[4096];
	ssize_t n = read(fd, chunk, sizeof(chunk));
	if (n <= 0) {
		if (n < 0 && (errno == EAGAIN || errno == EINTR)) {
			return 0;
		}
		ipc_client_destroy(client);
		return 0;
	}

	if (client->len + (size_t)n + 1 > client->cap) {
		size_t cap = client->cap ? client->cap * 2 : 8192;
		while (cap < client->len + (size_t)n + 1) {
			cap *= 2;
		}
		/* Cap the accumulator so a client that never sends a newline cannot
		 * grow the compositor's heap without bound. */
		if (cap > 1u << 20) {
			ipc_client_destroy(client);
			return 0;
		}
		char *buf = realloc(client->buf, cap);
		if (buf == NULL) {
			ipc_client_destroy(client);
			return 0;
		}
		client->buf = buf;
		client->cap = cap;
	}

	memcpy(client->buf + client->len, chunk, (size_t)n);
	client->len += (size_t)n;

	/* Dispatch each complete line, keeping any trailing partial. */
	size_t start = 0;
	for (size_t i = 0; i < client->len; i++) {
		if (client->buf[i] != '\n') {
			continue;
		}
		client->buf[i] = '\0';
		if (i > start) {
			viewport_ipc_handle(client->ipc->server, client->buf + start,
				i - start);
		}
		start = i + 1;
	}

	if (start > 0) {
		memmove(client->buf, client->buf + start, client->len - start);
		client->len -= start;
	}

	return 0;
}

static int handle_socket_connection(int fd, uint32_t mask, void *data)
{
	struct viewport_ipc *ipc = data;

	int client_fd = accept(fd, NULL, NULL);
	if (client_fd < 0) {
		return 0;
	}

	int flags = fcntl(client_fd, F_GETFL, 0);
	fcntl(client_fd, F_SETFL, flags | O_NONBLOCK);

	struct ipc_client *client = calloc(1, sizeof(*client));
	if (client == NULL) {
		close(client_fd);
		return 0;
	}
	client->ipc = ipc;
	client->fd = client_fd;
	client->source = wl_event_loop_add_fd(ipc->server->wl_event_loop, client_fd,
		WL_EVENT_READABLE, handle_client_readable, client);
	wl_list_insert(&ipc->clients, &client->link);

	/* Bring the newcomer up to date immediately: outputs, then every view
	 * that mapped before it connected. */
	viewport_ipc_notify_output_layout(ipc->server);
	viewport_ipc_notify_views(ipc->server);
	return 0;
}

struct viewport_ipc *viewport_ipc_create(struct viewport_server *server,
	const char *path)
{
	struct viewport_ipc *ipc = calloc(1, sizeof(*ipc));
	if (ipc == NULL) {
		return NULL;
	}
	ipc->server = server;
	wl_list_init(&ipc->clients);

	if (path != NULL) {
		ipc->path = strdup(path);
	} else {
		const char *runtime_dir = getenv("XDG_RUNTIME_DIR");
		if (runtime_dir == NULL) {
			runtime_dir = "/tmp";
		}
		char buf[PATH_MAX];
		snprintf(buf, sizeof(buf), "%s/viewport-%d.sock", runtime_dir,
			(int)getpid());
		ipc->path = strdup(buf);
	}

	if (ipc->path == NULL) {
		free(ipc);
		return NULL;
	}

	struct sockaddr_un addr = { .sun_family = AF_UNIX };
	if (strlen(ipc->path) >= sizeof(addr.sun_path)) {
		wlr_log(WLR_ERROR, "control socket path too long: %s", ipc->path);
		goto error;
	}
	strcpy(addr.sun_path, ipc->path);

	ipc->fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
	if (ipc->fd < 0) {
		wlr_log(WLR_ERROR, "control socket: %s", strerror(errno));
		goto error;
	}

	unlink(ipc->path);
	if (bind(ipc->fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
		wlr_log(WLR_ERROR, "bind %s: %s", ipc->path, strerror(errno));
		goto error_fd;
	}
	if (listen(ipc->fd, 8) < 0) {
		wlr_log(WLR_ERROR, "listen %s: %s", ipc->path, strerror(errno));
		goto error_fd;
	}

	ipc->source = wl_event_loop_add_fd(server->wl_event_loop, ipc->fd,
		WL_EVENT_READABLE, handle_socket_connection, ipc);

	setenv("VIEWPORT_SOCKET", ipc->path, 1);
	wlr_log(WLR_INFO, "control socket at %s", ipc->path);
	return ipc;

error_fd:
	close(ipc->fd);
error:
	free(ipc->path);
	free(ipc);
	return NULL;
}

void viewport_ipc_destroy(struct viewport_ipc *ipc)
{
	if (ipc == NULL) {
		return;
	}

	struct ipc_client *client, *tmp;
	wl_list_for_each_safe(client, tmp, &ipc->clients, link) {
		ipc_client_destroy(client);
	}

	if (ipc->source != NULL) {
		wl_event_source_remove(ipc->source);
	}
	close(ipc->fd);
	unlink(ipc->path);
	free(ipc->path);
	free(ipc);
}
