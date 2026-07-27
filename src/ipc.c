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
 * Messages are documented in docs/ipc.md.
 */
#define _GNU_SOURCE
#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

#include <json-glib/json-glib.h>

#include <wlr/types/wlr_output.h>
#include <wlr/util/log.h>

#include "json_util.h"
#include "viewport.h"

struct viewport_ipc {
	struct viewport_server *server;
	int fd;
	char *path;
	struct wl_event_source *source;
	struct wl_list clients; /* viewport_ipc_client.link */
	/* Clients torn down while a dispatch was still in flight; see
	 * ipc_client_close(). Freed from an idle callback. */
	struct wl_list dead; /* viewport_ipc_client.link */
	struct wl_event_source *reaper;
};

struct viewport_ipc_client {
	struct wl_list link;
	struct viewport_ipc *ipc;
	int fd;
	struct wl_event_source *source;
	/* Set once the client has been torn down. The struct outlives that until
	 * the reaper runs, so a handler part-way through dispatching this client's
	 * own message can notice and stop. */
	bool dead;

	/* Accumulates a partial line across reads. */
	char *buf;
	size_t len;
	size_t cap;

	/* Queued output. A message is appended here and the queue drained as far
	 * as the socket will take it; whatever is left waits for the fd to become
	 * writable again. */
	char *out;
	size_t out_len;
	size_t out_cap;
};

/* How much unsent output a client may accumulate before it is dropped.
 *
 * Generous: replaying the view list to a client that has not read in a while
 * is legitimately a few tens of kilobytes. Past this the client is not keeping
 * up at all, and buffering for it without limit would let one stuck reader
 * grow the compositor's heap until something dies. */
#define IPC_MAX_QUEUED (4u << 20)

/* ------------------------------------------------------------------------
 * Outbound
 * --------------------------------------------------------------------- */

static void ipc_reap(struct viewport_ipc *ipc)
{
	struct viewport_ipc_client *client, *tmp;
	wl_list_for_each_safe(client, tmp, &ipc->dead, link) {
		wl_list_remove(&client->link);
		free(client->buf);
		free(client->out);
		free(client);
	}
}

static void handle_reap(void *data)
{
	struct viewport_ipc *ipc = data;
	/* Idle sources remove themselves once dispatched. */
	ipc->reaper = NULL;
	ipc_reap(ipc);
}

/* Drop a client, without freeing it yet.
 *
 * A message from a socket client is dispatched from that client's own read
 * handler, and almost every message broadcasts a reply — so the write below can
 * fail on the very client whose handler is still on the stack. That is not an
 * exotic case: `socat` sending one command and exiting does exactly it, and the
 * reply then fails with EPIPE.
 *
 * Freeing here would have the read handler return into freed memory. So the
 * client is unlinked, its fd and event source released, and the struct itself
 * left alive until an idle callback runs — by which point no handler is still
 * holding a pointer to it. */
static void ipc_client_close(struct viewport_ipc_client *client)
{
	if (client->dead) {
		return;
	}
	client->dead = true;

	wl_list_remove(&client->link);
	if (client->source != NULL) {
		wl_event_source_remove(client->source);
		client->source = NULL;
	}
	if (client->fd >= 0) {
		close(client->fd);
		client->fd = -1;
	}

	struct viewport_ipc *ipc = client->ipc;
	wl_list_insert(&ipc->dead, &client->link);
	if (ipc->reaper == NULL) {
		ipc->reaper = wl_event_loop_add_idle(ipc->server->wl_event_loop,
			handle_reap, ipc);
	}
}

/* Push as much of the queue as the socket will take.
 *
 * A short write is not an error and not a rarity — it is what a stream socket
 * does when its buffer is nearly full. The old code tested the return only for
 * being negative, so a partial message was treated as sent and the newline that
 * followed was appended to a truncated line: the client then parsed one corrupt
 * message and every message after it was offset by the remainder. Nothing
 * logged, because from the compositor's side nothing had failed. */
static void ipc_client_flush(struct viewport_ipc_client *client)
{
	while (client->out_len > 0) {
		ssize_t n = write(client->fd, client->out, client->out_len);
		if (n > 0) {
			client->out_len -= (size_t)n;
			memmove(client->out, client->out + n, client->out_len);
			continue;
		}
		if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
			break;
		}
		if (n < 0 && errno == EINTR) {
			continue;
		}
		/* A client that will not take its output is one we drop, rather than
		 * one that stalls the compositor. */
		ipc_client_close(client);
		return;
	}

	/* Only ask to be told about writability while there is something waiting;
	 * an always-armed writable fd spins the event loop. */
	if (client->source != NULL) {
		wl_event_source_fd_update(client->source, client->out_len > 0
			? (WL_EVENT_READABLE | WL_EVENT_WRITABLE) : WL_EVENT_READABLE);
	}
}

/* Queue one newline-terminated message, then drain what we can. */
static void ipc_client_send(struct viewport_ipc_client *client,
	const char *json, size_t len)
{
	if (client->dead) {
		return;
	}

	size_t need = client->out_len + len + 1;
	if (need > IPC_MAX_QUEUED) {
		wlr_log(WLR_ERROR, "IPC client is not draining; dropping it");
		ipc_client_close(client);
		return;
	}

	if (need > client->out_cap) {
		size_t cap = client->out_cap ? client->out_cap : 8192;
		while (cap < need) {
			cap *= 2;
		}
		char *out = realloc(client->out, cap);
		if (out == NULL) {
			ipc_client_close(client);
			return;
		}
		client->out = out;
		client->out_cap = cap;
	}

	memcpy(client->out + client->out_len, json, len);
	client->out_len += len;
	client->out[client->out_len++] = '\n';

	ipc_client_flush(client);
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
	struct viewport_ipc_client *client, *tmp;
	wl_list_for_each_safe(client, tmp, &server->ipc->clients, link) {
		ipc_client_send(client, json, len);
	}
}

/* Serialise what has been built. The caller owns the result. */
static char *builder_to_text(JsonBuilder *builder)
{
	JsonGenerator *generator = json_generator_new();
	JsonNode *root = json_builder_get_root(builder);
	json_generator_set_root(generator, root);

	char *text = json_generator_to_data(generator, NULL);

	json_node_free(root);
	g_object_unref(generator);
	return text;
}

static void broadcast_builder(struct viewport_server *server,
	JsonBuilder *builder)
{
	char *text = builder_to_text(builder);
	viewport_ipc_broadcast(server, text);
	g_free(text);
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

/* `replay` marks the copies sent when the shell reloads or asks for the window
 * list. They describe windows that have been open for a while, so the shell
 * must not treat them as newly opened — focusing on every view.added would move
 * focus to whichever window happened to be last in the list every time the
 * shell reloaded, which happens on every edit to it. */
static void notify_view_added(struct viewport_toplevel *toplevel, bool replay)
{
	JsonBuilder *builder = json_builder_new();
	json_builder_begin_object(builder);
	json_builder_set_member_name(builder, "type");
	json_builder_add_string_value(builder, "view.added");
	json_builder_set_member_name(builder, "id");
	json_builder_add_int_value(builder, toplevel->id);
	json_builder_set_member_name(builder, "title");
	json_builder_add_string_value(builder,
		viewport_view_title(toplevel));
	json_builder_set_member_name(builder, "app_id");
	json_builder_add_string_value(builder,
		viewport_view_app_id(toplevel));
	json_builder_set_member_name(builder, "output");
	json_builder_add_string_value(builder,
		output_for_new_view(toplevel->server));
	/* So the shell can refuse to shrink a window past what it accepts. */
	json_builder_set_member_name(builder, "min_width");
	json_builder_add_int_value(builder,
		toplevel->kind == VIEWPORT_VIEW_XDG
			? toplevel->xdg_toplevel->current.min_width : 0);
	json_builder_set_member_name(builder, "min_height");
	json_builder_add_int_value(builder,
		toplevel->kind == VIEWPORT_VIEW_XDG
			? toplevel->xdg_toplevel->current.min_height : 0);

	/* Dialogs and fixed-size windows want to float rather than be tiled. The
	 * compositor can see the signals for that — a parent toplevel, an X11
	 * window type — and the natural size is what to open them at. */
	int natural_width, natural_height;
	viewport_view_natural_size(toplevel, &natural_width, &natural_height);
	json_builder_set_member_name(builder, "replay");
	json_builder_add_boolean_value(builder, replay);
	json_builder_set_member_name(builder, "floating");
	json_builder_add_boolean_value(builder,
		viewport_view_wants_floating(toplevel));
	json_builder_set_member_name(builder, "width");
	json_builder_add_int_value(builder, natural_width);
	json_builder_set_member_name(builder, "height");
	json_builder_add_int_value(builder, natural_height);

	json_builder_end_object(builder);

	broadcast_builder(toplevel->server, builder);
	g_object_unref(builder);
}

void viewport_ipc_notify_view_added(struct viewport_toplevel *toplevel)
{
	notify_view_added(toplevel, false);
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
	viewport_foreign_view_props(toplevel);
	JsonBuilder *builder = json_builder_new();
	json_builder_begin_object(builder);
	json_builder_set_member_name(builder, "type");
	json_builder_add_string_value(builder, "view.props");
	json_builder_set_member_name(builder, "id");
	json_builder_add_int_value(builder, toplevel->id);
	json_builder_set_member_name(builder, "title");
	json_builder_add_string_value(builder,
		viewport_view_title(toplevel));
	json_builder_set_member_name(builder, "app_id");
	json_builder_add_string_value(builder,
		viewport_view_app_id(toplevel));
	json_builder_end_object(builder);

	broadcast_builder(toplevel->server, builder);
	g_object_unref(builder);
}

void viewport_ipc_notify_config(struct viewport_server *server)
{
	JsonBuilder *builder = json_builder_new();
	json_builder_begin_object(builder);
	json_builder_set_member_name(builder, "type");
	json_builder_add_string_value(builder, "config");
	json_builder_set_member_name(builder, "layout");
	json_builder_add_string_value(builder,
		server->config.layout != NULL ? server->config.layout : "tiling");

	json_builder_set_member_name(builder, "logo");
	json_builder_add_boolean_value(builder, server->config.logo);
	json_builder_set_member_name(builder, "tutorial");
	json_builder_add_boolean_value(builder, server->config.tutorial);

	if (server->config.bar != NULL) {
		json_builder_set_member_name(builder, "bar");
		json_builder_add_string_value(builder, server->config.bar);
	}

	/* Handed over as parsed JSON rather than a string, so the shell does not
	 * have to parse a second time inside a message it already parsed. */
	if (server->config.rules_json != NULL) {
		JsonParser *parser = json_parser_new();
		if (json_parser_load_from_data(parser, server->config.rules_json, -1,
				NULL)) {
			json_builder_set_member_name(builder, "rules");
			json_builder_add_value(builder,
				json_node_copy(json_parser_get_root(parser)));
		}
		g_object_unref(parser);
	}
	if (server->config.theme_json != NULL) {
		JsonParser *parser = json_parser_new();
		if (json_parser_load_from_data(parser, server->config.theme_json, -1,
				NULL)) {
			json_builder_set_member_name(builder, "theme");
			json_builder_add_value(builder,
				json_node_copy(json_parser_get_root(parser)));
		}
		g_object_unref(parser);
	}

	json_builder_end_object(builder);

	broadcast_builder(server, builder);
	g_object_unref(builder);
}

void viewport_ipc_notify_modifiers(struct viewport_server *server, bool logo)
{
	JsonBuilder *builder = json_builder_new();
	json_builder_begin_object(builder);
	json_builder_set_member_name(builder, "type");
	json_builder_add_string_value(builder, "modifiers");
	json_builder_set_member_name(builder, "logo");
	json_builder_add_boolean_value(builder, logo);
	json_builder_end_object(builder);

	broadcast_builder(server, builder);
	g_object_unref(builder);
}

/* Hand the stored layout back to the shell. Sent when the shell asks, which it
 * does once it is ready to apply it — not on connect, because a shell that has
 * not finished loading cannot do anything with it. */
void viewport_ipc_notify_session(struct viewport_server *server)
{
	char *state = viewport_session_load(server);

	JsonBuilder *builder = json_builder_new();
	json_builder_begin_object(builder);
	json_builder_set_member_name(builder, "type");
	json_builder_add_string_value(builder, "session.restore");
	json_builder_set_member_name(builder, "state");
	json_builder_add_string_value(builder, state != NULL ? state : "");
	json_builder_end_object(builder);

	broadcast_builder(server, builder);
	g_object_unref(builder);
	g_free(state);
}

/* A notification, on its way to the shell that will draw it. */
void viewport_ipc_notify_notification(struct viewport_server *server,
	uint32_t id, const char *app_name, const char *icon, const char *summary,
	const char *body, uint8_t urgency, int32_t timeout,
	const char *const *action_keys, const char *const *action_labels,
	size_t action_count)
{
	JsonBuilder *builder = json_builder_new();
	json_builder_begin_object(builder);
	json_builder_set_member_name(builder, "type");
	json_builder_add_string_value(builder, "notification.add");
	json_builder_set_member_name(builder, "id");
	json_builder_add_int_value(builder, id);
	json_builder_set_member_name(builder, "app_name");
	json_builder_add_string_value(builder, app_name ? app_name : "");
	json_builder_set_member_name(builder, "icon");
	json_builder_add_string_value(builder, icon ? icon : "");
	json_builder_set_member_name(builder, "summary");
	json_builder_add_string_value(builder, summary ? summary : "");
	json_builder_set_member_name(builder, "body");
	json_builder_add_string_value(builder, body ? body : "");
	json_builder_set_member_name(builder, "urgency");
	json_builder_add_int_value(builder, urgency);
	/* Negative means "the server decides"; zero means "never expire". Passed
	 * through as sent, because deciding is the shell's job. */
	json_builder_set_member_name(builder, "timeout");
	json_builder_add_int_value(builder, timeout);

	json_builder_set_member_name(builder, "actions");
	json_builder_begin_array(builder);
	for (size_t i = 0; i < action_count; i++) {
		json_builder_begin_object(builder);
		json_builder_set_member_name(builder, "key");
		json_builder_add_string_value(builder, action_keys[i]);
		json_builder_set_member_name(builder, "label");
		json_builder_add_string_value(builder, action_labels[i]);
		json_builder_end_object(builder);
	}
	json_builder_end_array(builder);

	json_builder_end_object(builder);

	broadcast_builder(server, builder);
	g_object_unref(builder);
}

void viewport_ipc_notify_notification_closed(struct viewport_server *server,
	uint32_t id)
{
	JsonBuilder *builder = json_builder_new();
	json_builder_begin_object(builder);
	json_builder_set_member_name(builder, "type");
	json_builder_add_string_value(builder, "notification.close");
	json_builder_set_member_name(builder, "id");
	json_builder_add_int_value(builder, id);
	json_builder_end_object(builder);

	broadcast_builder(server, builder);
	g_object_unref(builder);
}

void viewport_ipc_notify_views(struct viewport_server *server)
{
	struct viewport_toplevel *toplevel;
	wl_list_for_each(toplevel, &server->toplevels, link) {
		if (toplevel->mapped) {
			notify_view_added(toplevel, true);
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

void viewport_ipc_notify_fullscreen(struct viewport_toplevel *toplevel,
	bool on)
{
	char command[96];
	snprintf(command, sizeof(command), "window.fullscreen.set %u %d",
		toplevel->id, on ? 1 : 0);
	viewport_ipc_notify_shell_command(toplevel->server, command);
}

/* Hand the drag movement collected since the last frame to the shell.
 *
 * Called once per output frame rather than once per pointer event: the shell
 * cannot act on a delta faster than the animation frame it lays out in, so a
 * 1000Hz mouse reporting every motion separately built and evaluated a
 * thousand JavaScript programs a second to deliver the same three integers a
 * single message would have carried.
 *
 * The target is looked up by id because the window can be closed between the
 * motion that moved it and the frame that reports it. */
void viewport_ipc_flush_deltas(struct viewport_server *server)
{
	if (!server->delta_pending) {
		return;
	}
	server->delta_pending = false;

	int dx = (int)server->delta_x;
	int dy = (int)server->delta_y;
	server->delta_x = 0;
	server->delta_y = 0;

	/* Sub-pixel motion rounds to nothing; a message saying the window moved
	 * zero pixels would make the shell relayout for no change. */
	if (dx == 0 && dy == 0) {
		return;
	}
	if (viewport_server_find_toplevel(server, server->delta_id) == NULL) {
		return;
	}

	char command[96];
	snprintf(command, sizeof(command), "%s %u %d %d",
		server->delta_is_resize ? "layout.resize.delta" : "layout.move.delta",
		server->delta_id, dx, dy);
	viewport_ipc_notify_shell_command(server, command);
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

	/* The caller reads this out of a JSON object, where presence and type are
	 * separate questions: the call site used to gate on
	 * json_object_has_member() alone, so
	 * {"type":"output.configure","name":"eDP-1","transform":null} arrived here
	 * as a NULL and died in the strcmp below — one postMessage() from any page,
	 * the same way a non-string "type" used to be. viewport_json_string() now
	 * answers both questions at once, but it still answers NULL, and rejecting
	 * that here rather than at the call site closes it for whoever adds the
	 * next caller.
	 *
	 * An unknown name already means "leave the transform alone", and a
	 * wrong-typed one is not more meaningful than an unknown one. */
	if (name == NULL) {
		return false;
	}

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
		/* The area panels have not reserved. The shell places windows inside
		 * this rather than the full box, which is what keeps a bar from
		 * overlapping them. */
		json_builder_set_member_name(builder, "usable_x");
		json_builder_add_int_value(builder, output->usable_area.x);
		json_builder_set_member_name(builder, "usable_y");
		json_builder_add_int_value(builder, output->usable_area.y);
		json_builder_set_member_name(builder, "usable_width");
		json_builder_add_int_value(builder, output->usable_area.width);
		json_builder_set_member_name(builder, "usable_height");
		json_builder_add_int_value(builder, output->usable_area.height);

		/* So the shell can show which monitor is in HDR, and offer it only
		 * where the display will take it. */
		json_builder_set_member_name(builder, "hdr");
		json_builder_add_boolean_value(builder, output->hdr);
		json_builder_set_member_name(builder, "hdr_capable");
		json_builder_add_boolean_value(builder,
			viewport_output_hdr_capable(output));

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

/* Report a rejected message.
 *
 * `origin` is the client whose message caused this, where there is one. An
 * error belongs to the sender: broadcasting it tells every other connected
 * client — and the shell, which is drawing the desktop — about a mistake made
 * by a debugging socat session it has no knowledge of, and a shell that
 * surfaces errors to the user shows a stranger's typo as its own. A NULL
 * origin is a compositor-side error with no one message to blame, and still
 * goes to everybody. */
static void notify_error(struct viewport_server *server, const char *context,
	const char *message, struct viewport_ipc_client *origin)
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

	char *text = builder_to_text(builder);
	/* A dead origin has had its fd closed already; sending is a no-op, but
	 * falling back to a broadcast would leak the error to everyone else. */
	if (origin != NULL) {
		ipc_client_send(origin, text, strlen(text));
	} else {
		viewport_ipc_broadcast(server, text);
	}

	g_free(text);
	g_object_unref(builder);
}

/* ------------------------------------------------------------------------
 * Inbound
 * --------------------------------------------------------------------- */

static int object_int(JsonObject *object, const char *name, int fallback)
{
	int value;
	return viewport_json_int(object, name, &value) ? value : fallback;
}

static void handle_view_layout(struct viewport_server *server,
	JsonObject *object, struct viewport_ipc_client *origin G_GNUC_UNUSED)
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

	/* Optional: draw the window shrunk, without resizing its client.
	 *
	 * The overview needs every window on screen at once, and asking each client
	 * to resize itself to thumbnail dimensions would be both slow and often
	 * refused — plenty of windows have a minimum size larger than the thumbnail.
	 * So the client keeps the size it has and the compositor scales the buffer
	 * it produced. */
	double scale;
	toplevel->scale = viewport_json_double(object, "scale", &scale)
		? scale : 1.0;
	if (toplevel->scale <= 0.0 || toplevel->scale > 1.0) {
		toplevel->scale = 1.0;
	}

	/* Optional: the part of the window that is actually on its output. Absent
	 * means all of it, which is the ordinary tiled case. */
	JsonObject *clip = viewport_json_object(object, "clip");
	if (clip != NULL) {
		toplevel->clip = (struct wlr_box){
			.x = object_int(clip, "x", box.x),
			.y = object_int(clip, "y", box.y),
			.width = object_int(clip, "width", box.width),
			.height = object_int(clip, "height", box.height),
		};
		toplevel->has_clip = true;
	} else {
		toplevel->has_clip = false;
	}

	viewport_toplevel_set_box(toplevel, &box);
}

/* Per-window opacity, applied to the surface itself.
 *
 * The shell cannot fade a window by styling anything: the frame is DOM, the
 * contents are a surface this compositor draws. So the tween runs in the shell
 * and the value arrives here each frame. */
static void opacity_iterator(struct wlr_scene_buffer *buffer, int sx, int sy,
	void *data)
{
	wlr_scene_buffer_set_opacity(buffer, *(float *)data);
}

static void handle_view_opacity(struct viewport_server *server,
	JsonObject *object, struct viewport_ipc_client *origin G_GNUC_UNUSED)
{
	uint32_t id = (uint32_t)object_int(object, "id", 0);
	struct viewport_toplevel *toplevel =
		viewport_server_find_toplevel(server, id);
	if (toplevel == NULL || toplevel->surface_tree == NULL) {
		return;
	}

	double value;
	float opacity = viewport_json_double(object, "opacity", &value)
		? (float)value : 1.0f;
	if (opacity < 0.0f) {
		opacity = 0.0f;
	} else if (opacity > 1.0f) {
		opacity = 1.0f;
	}

	wlr_scene_node_for_each_buffer(&toplevel->surface_tree->node,
		opacity_iterator, &opacity);
}

static void handle_view_visible(struct viewport_server *server,
	JsonObject *object, struct viewport_ipc_client *origin G_GNUC_UNUSED)
{
	uint32_t id = (uint32_t)object_int(object, "id", 0);
	struct viewport_toplevel *toplevel = viewport_server_find_toplevel(server, id);
	if (toplevel == NULL || !toplevel->mapped) {
		return;
	}

	bool visible;
	if (!viewport_json_bool(object, "visible", &visible)) {
		visible = true;
	}

	/* Recorded, not just applied: directional focus needs to know which
	 * windows are actually on screen, or Mod4+l lands on something parked on
	 * a hidden workspace. */
	toplevel->visible = visible;
	wlr_scene_node_set_enabled(&toplevel->scene_tree->node,
		visible && toplevel->has_box);
}

/* How long an unconfirmed output change survives. Long enough to see whether
 * the screen came back, short enough that a blank monitor is not a lockout. */
#define OUTPUT_REVERT_SECONDS 12

void viewport_output_revert_cancel(struct viewport_server *server)
{
	struct viewport_output_revert *revert = server->output_revert;
	if (revert == NULL) {
		return;
	}
	server->output_revert = NULL;

	if (revert->timer != 0) {
		g_source_remove(revert->timer);
	}
	g_free(revert->name);
	free(revert);
}

static struct viewport_output *output_by_name(struct viewport_server *server,
	const char *name)
{
	struct viewport_output *output;
	wl_list_for_each(output, &server->outputs, link) {
		if (strcmp(output->wlr_output->name, name) == 0) {
			return output;
		}
	}
	return NULL;
}

static gboolean output_revert_fire(gpointer data)
{
	struct viewport_output_revert *revert = data;
	struct viewport_server *server = revert->server;

	revert->timer = 0;

	struct viewport_output *output = output_by_name(server, revert->name);
	if (output == NULL) {
		/* The output is gone; there is nothing left to put back. */
		viewport_output_revert_cancel(server);
		return G_SOURCE_REMOVE;
	}

	struct wlr_output *wlr_output = output->wlr_output;
	struct wlr_output_state state;
	wlr_output_state_init(&state);
	wlr_output_state_set_enabled(&state, revert->enabled);
	if (revert->enabled) {
		struct wlr_output_mode *match = NULL, *mode;
		wl_list_for_each(mode, &wlr_output->modes, link) {
			if (mode->width == revert->width &&
					mode->height == revert->height &&
					mode->refresh == revert->refresh) {
				match = mode;
				break;
			}
		}
		if (match != NULL) {
			wlr_output_state_set_mode(&state, match);
		} else if (revert->width > 0 && revert->height > 0) {
			wlr_output_state_set_custom_mode(&state, revert->width,
				revert->height, revert->refresh);
		}
		wlr_output_state_set_scale(&state, revert->scale);
		wlr_output_state_set_transform(&state, revert->transform);
		wlr_output_state_set_adaptive_sync_enabled(&state,
			revert->adaptive_sync);
	}

	bool ok = wlr_output_commit_state(wlr_output, &state);
	wlr_output_state_finish(&state);

	if (ok) {
		wlr_output_layout_add(server->output_layout, wlr_output, revert->x,
			revert->y);
		int width, height;
		viewport_layout_size(server, &width, &height);
		if (server->web != NULL) {
			viewport_web_resize(server->web, width, height);
		}
		viewport_ipc_notify_output_layout(server);
		viewport_output_manager_update(server);
	}

	wlr_log(WLR_INFO, "output %s not confirmed; reverted", revert->name);
	/* Nobody asked for this: it fires from a timer long after the message that
	 * armed it was answered, so everyone hears about it. */
	notify_error(server, "output.configure", "not confirmed; reverted", NULL);

	viewport_output_revert_cancel(server);
	return G_SOURCE_REMOVE;
}

/* Remember what the output looks like now, before it is changed. */
static void output_revert_arm(struct viewport_server *server,
	struct viewport_output *output)
{
	viewport_output_revert_cancel(server);

	struct viewport_output_revert *revert = calloc(1, sizeof(*revert));
	if (revert == NULL) {
		return;
	}

	struct wlr_output *wlr_output = output->wlr_output;
	struct wlr_box box;
	wlr_output_layout_get_box(server->output_layout, wlr_output, &box);

	revert->server = server;
	revert->name = g_strdup(wlr_output->name);
	revert->enabled = wlr_output->enabled;
	revert->width = wlr_output->width;
	revert->height = wlr_output->height;
	revert->refresh = wlr_output->refresh;
	revert->scale = wlr_output->scale;
	revert->transform = wlr_output->transform;
	revert->adaptive_sync =
		wlr_output->adaptive_sync_status == WLR_OUTPUT_ADAPTIVE_SYNC_ENABLED;
	revert->x = box.x;
	revert->y = box.y;

	revert->timer = g_timeout_add_seconds(OUTPUT_REVERT_SECONDS,
		output_revert_fire, revert);
	server->output_revert = revert;
}

static void handle_output_configure(struct viewport_server *server,
	JsonObject *object, struct viewport_ipc_client *origin)
{
	const char *name = viewport_json_string(object, "name");
	if (name == NULL) {
		notify_error(server, "output.configure", "missing output name", origin);
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
		notify_error(server, "output.configure", "no such output", origin);
		return;
	}

	struct wlr_output *wlr_output = output->wlr_output;
	struct wlr_output_state state;
	wlr_output_state_init(&state);

	bool enabled;
	if (viewport_json_bool(object, "enabled", &enabled)) {
		wlr_output_state_set_enabled(&state, enabled);
	}

	JsonObject *mode_object = viewport_json_object(object, "mode");
	if (mode_object != NULL) {
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

	double scale;
	if (viewport_json_double(object, "scale", &scale) && scale > 0.0) {
		wlr_output_state_set_scale(&state, (float)scale);
	}

	enum wl_output_transform transform;
	if (transform_from_name(viewport_json_string(object, "transform"),
			&transform)) {
		wlr_output_state_set_transform(&state, transform);
	}

	bool adaptive_sync;
	if (viewport_json_bool(object, "adaptive_sync", &adaptive_sync)) {
		wlr_output_state_set_adaptive_sync_enabled(&state, adaptive_sync);
	}

	/* Test before committing. A mode the hardware cannot drive fails here
	 * instead of blanking the screen the user is configuring it from. */
	if (!wlr_output_test_state(wlr_output, &state)) {
		wlr_output_state_finish(&state);
		notify_error(server, "output.configure", "configuration rejected",
			origin);
		return;
	}

	/* Captured before the commit, so a revert has the old values rather than
	 * the ones being applied. */
	output_revert_arm(server, output);

	bool ok = wlr_output_commit_state(wlr_output, &state);
	wlr_output_state_finish(&state);
	if (!ok) {
		viewport_output_revert_cancel(server);
		notify_error(server, "output.configure", "commit failed", origin);
		return;
	}

	int x, y;
	bool have_x = viewport_json_int(object, "x", &x);
	bool have_y = viewport_json_int(object, "y", &y);
	if (have_x || have_y) {
		struct wlr_box box;
		wlr_output_layout_get_box(server->output_layout, wlr_output, &box);
		wlr_output_layout_add(server->output_layout, wlr_output,
			have_x ? x : box.x, have_y ? y : box.y);
	}

	int width, height;
	viewport_layout_size(server, &width, &height);
	if (server->web != NULL) {
		viewport_web_resize(server->web, width, height);
	}
	viewport_ipc_notify_output_layout(server);
	viewport_output_manager_update(server);
}

static void handle_view_fullscreen(struct viewport_server *server,
	JsonObject *object, struct viewport_ipc_client *origin G_GNUC_UNUSED)
{
	/* Tell the client it is fullscreen. Resizing its hole is not enough:
	 * an application rearranges its own layout on the fullscreen state —
	 * hiding toolbars, switching a video to a fullscreen presentation —
	 * and never learns about it from a configure alone. */
	struct viewport_toplevel *toplevel = viewport_server_find_toplevel(
		server, (uint32_t)object_int(object, "id", 0));
	if (toplevel == NULL) {
		return;
	}

	bool on;
	if (!viewport_json_bool(object, "fullscreen", &on)) {
		on = false;
	}
	viewport_view_set_fullscreen(toplevel, on);
}

static void handle_view_focus(struct viewport_server *server,
	JsonObject *object, struct viewport_ipc_client *origin G_GNUC_UNUSED)
{
	struct viewport_toplevel *toplevel = viewport_server_find_toplevel(
		server, (uint32_t)object_int(object, "id", 0));
	if (toplevel != NULL) {
		viewport_toplevel_focus(toplevel);
	}
}

static void handle_view_close(struct viewport_server *server,
	JsonObject *object, struct viewport_ipc_client *origin G_GNUC_UNUSED)
{
	struct viewport_toplevel *toplevel = viewport_server_find_toplevel(
		server, (uint32_t)object_int(object, "id", 0));
	if (toplevel != NULL) {
		viewport_toplevel_close(toplevel);
	}
}

static void handle_view_query(struct viewport_server *server,
	JsonObject *object G_GNUC_UNUSED,
	struct viewport_ipc_client *origin G_GNUC_UNUSED)
{
	viewport_ipc_notify_config(server);
	viewport_ipc_notify_views(server);
}

static void handle_shell_focus(struct viewport_server *server,
	JsonObject *object G_GNUC_UNUSED,
	struct viewport_ipc_client *origin G_GNUC_UNUSED)
{
	viewport_focus_web(server);
}

static void handle_shell_overview(struct viewport_server *server,
	JsonObject *object, struct viewport_ipc_client *origin G_GNUC_UNUSED)
{
	/* While the overview is up the shell is drawing miniatures of every
	 * window, and a click on one means "go there" rather than reaching the
	 * client underneath. Input is routed to the shell for the duration. */
	bool active;
	server->overview = viewport_json_bool(object, "active", &active) && active;
	if (server->overview) {
		viewport_focus_web(server);
		return;
	}

	/* Clear every window's scale here rather than waiting for the shell
	 * to send each one a new rect.
	 *
	 * A window whose rect is unchanged gets no message, and the
	 * per-frame pass that maintains the scale stops the moment the
	 * overview closes — so the shrunken destination size stayed on the
	 * buffer until the client next painted. A terminal sitting idle
	 * does not paint, so it kept whatever size the overview gave it and
	 * came back magnified and cropped, righting itself only when
	 * something made it repaint. */
	int cleared = 0;
	struct viewport_toplevel *toplevel;
	wl_list_for_each(toplevel, &server->toplevels, link) {
		toplevel->scale = 1.0;
		viewport_toplevel_apply_crop(toplevel);
		cleared++;
	}
	viewport_scale_forget(server);
	wlr_log(WLR_DEBUG, "overview closed; scale cleared on %d windows", cleared);
}

static void handle_session_save(struct viewport_server *server,
	JsonObject *object, struct viewport_ipc_client *origin G_GNUC_UNUSED)
{
	/* The shell's own serialisation, stored verbatim. */
	const char *state = viewport_json_string(object, "state");
	if (state != NULL) {
		viewport_session_save(server, state);
	}
}

static void handle_session_query(struct viewport_server *server,
	JsonObject *object G_GNUC_UNUSED,
	struct viewport_ipc_client *origin G_GNUC_UNUSED)
{
	viewport_ipc_notify_session(server);
}

static void handle_notification_action(struct viewport_server *server,
	JsonObject *object, struct viewport_ipc_client *origin G_GNUC_UNUSED)
{
	/* The shell drew the buttons; the application is told which was pressed,
	 * by the key it supplied rather than the label it showed. */
	int id;
	if (viewport_json_int(object, "id", &id)) {
		viewport_notifications_action(server->notifications, (uint32_t)id,
			viewport_json_string(object, "action"));
	}
}

static void handle_notification_dismiss(struct viewport_server *server,
	JsonObject *object, struct viewport_ipc_client *origin G_GNUC_UNUSED)
{
	int id;
	if (viewport_json_int(object, "id", &id)) {
		viewport_notifications_dismissed(server->notifications, (uint32_t)id);
	}
}

static void handle_notification_expire(struct viewport_server *server,
	JsonObject *object, struct viewport_ipc_client *origin G_GNUC_UNUSED)
{
	int id;
	if (viewport_json_int(object, "id", &id)) {
		viewport_notifications_expired(server->notifications, (uint32_t)id);
	}
}

static void handle_output_hdr(struct viewport_server *server,
	JsonObject *object, struct viewport_ipc_client *origin)
{
	/* Named output, or the active one. Absent "enabled" toggles, which is
	 * what a keybinding wants and what a settings panel does not. */
	const char *name = viewport_json_string(object, "name");
	if (name == NULL) {
		name = server->active_output;
	}
	struct viewport_output *output = name != NULL
		? output_by_name(server, name) : NULL;
	if (output == NULL) {
		notify_error(server, "output.hdr", "no such output", origin);
		return;
	}

	bool enabled;
	if (!viewport_json_bool(object, "enabled", &enabled)) {
		enabled = !output->hdr;
	}
	if (!viewport_output_set_hdr(output, enabled)) {
		notify_error(server, "output.hdr",
			enabled ? "the display would not take HDR"
				: "could not leave HDR", origin);
	}
}

static void handle_output_confirm(struct viewport_server *server,
	JsonObject *object G_GNUC_UNUSED,
	struct viewport_ipc_client *origin G_GNUC_UNUSED)
{
	/* The shell saw the change land and the user accepted it, so the
	 * pending revert is called off. */
	viewport_output_revert_cancel(server);
}

static void handle_output_active(struct viewport_server *server,
	JsonObject *object, struct viewport_ipc_client *origin G_GNUC_UNUSED)
{
	const char *name = viewport_json_string(object, "name");
	if (name != NULL) {
		g_free(server->active_output);
		server->active_output = g_strdup(name);
	}
}

static void handle_output_query(struct viewport_server *server,
	JsonObject *object G_GNUC_UNUSED,
	struct viewport_ipc_client *origin G_GNUC_UNUSED)
{
	viewport_ipc_notify_output_layout(server);
}

static void handle_output_test_add(struct viewport_server *server,
	JsonObject *object G_GNUC_UNUSED, struct viewport_ipc_client *origin)
{
	/* Headless-only, and the callee re-checks: nothing on a real session's
	 * control socket should be able to plug or unplug a monitor. */
	if (!viewport_output_test_add(server)) {
		notify_error(server, "output.test_add",
			"headless hotplug is only available under --headless", origin);
	}
}

static void handle_output_test_remove(struct viewport_server *server,
	JsonObject *object, struct viewport_ipc_client *origin)
{
	/* Without a name, the first output in the list. */
	if (!viewport_output_test_remove(server,
			viewport_json_string(object, "name"))) {
		notify_error(server, "output.test_remove",
			"headless hotplug is only available under --headless", origin);
	}
}

static void handle_bind_add(struct viewport_server *server,
	JsonObject *object, struct viewport_ipc_client *origin)
{
	/* Runtime binds from the shell are additive and expendable; the ones
	 * that must survive a broken shell belong in the config file. */
	const char *chord = viewport_json_string(object, "chord");
	const char *action = viewport_json_string(object, "action");
	if (chord == NULL || action == NULL) {
		return;
	}

	char *spec = g_strdup_printf("%s=%s", chord, action);
	if (!viewport_binding_add(server, spec)) {
		notify_error(server, "bind.add", spec, origin);
	}
	g_free(spec);
}

static void handle_quit(struct viewport_server *server,
	JsonObject *object G_GNUC_UNUSED,
	struct viewport_ipc_client *origin G_GNUC_UNUSED)
{
	viewport_server_terminate(server);
}

/* One row per message type.
 *
 * This was a chain of two dozen strcmp() branches, four of which carried their
 * whole implementation inline — so the function that decided what a message
 * was also resized windows and armed keybindings, and every new message made
 * it longer. A table cannot be added to in the wrong place. */
static const struct {
	const char *type;
	void (*handle)(struct viewport_server *server, JsonObject *object,
		struct viewport_ipc_client *origin);
} ipc_handlers[] = {
	{ "view.layout", handle_view_layout },
	{ "view.visible", handle_view_visible },
	{ "view.fullscreen", handle_view_fullscreen },
	{ "view.focus", handle_view_focus },
	{ "view.close", handle_view_close },
	{ "view.opacity", handle_view_opacity },
	{ "view.query", handle_view_query },
	{ "shell.focus", handle_shell_focus },
	{ "shell.overview", handle_shell_overview },
	{ "session.save", handle_session_save },
	{ "session.query", handle_session_query },
	{ "notification.action", handle_notification_action },
	{ "notification.dismiss", handle_notification_dismiss },
	{ "notification.expire", handle_notification_expire },
	{ "output.configure", handle_output_configure },
	{ "output.hdr", handle_output_hdr },
	{ "output.confirm", handle_output_confirm },
	{ "output.active", handle_output_active },
	{ "output.query", handle_output_query },
	{ "output.test_add", handle_output_test_add },
	{ "output.test_remove", handle_output_test_remove },
	{ "bind.add", handle_bind_add },
	{ "quit", handle_quit },
};

void viewport_ipc_handle(struct viewport_server *server, const char *json,
	size_t len, struct viewport_ipc_client *origin)
{
	/* The first message the shell sends, once.
	 *
	 * "The shell did not lay anything out" has two very different causes: the
	 * page never ran, or it ran and its layout was wrong. Nothing in the log
	 * distinguished them — a working shell is silent, so an absence of output
	 * proved nothing either way. This line is the difference: if it appears,
	 * the page loaded and its scripts are talking. */
	static bool announced;
	if (!announced) {
		announced = true;
		wlr_log(WLR_INFO, "shell is talking to us");
	}

	JsonParser *parser = json_parser_new();
	GError *error = NULL;

	if (!json_parser_load_from_data(parser, json, (gssize)len, &error)) {
		wlr_log(WLR_ERROR, "malformed IPC message: %s", error->message);
		notify_error(server, "ipc", error->message, origin);
		g_error_free(error);
		g_object_unref(parser);
		return;
	}

	JsonNode *root = json_parser_get_root(parser);
	if (root == NULL || json_node_get_node_type(root) != JSON_NODE_OBJECT) {
		notify_error(server, "ipc", "JSON root is not an object", origin);
		g_object_unref(parser);
		return;
	}

	JsonObject *object = json_node_get_object(root);
	const char *type = viewport_json_string(object, "type");
	if (type == NULL) {
		/* Absent, or present and not a string; there is nothing to dispatch on
		 * either way. {"type":5}, {"type":null}, {"type":{}} and {"type":true}
		 * all used to satisfy the json_object_has_member() gate that stood
		 * here and then reach a strcmp() on the NULL the getter handed back —
		 * a page one postMessage() away from killing the compositor and every
		 * window it holds. */
		notify_error(server, "ipc", "missing or non-string 'type'", origin);
		g_object_unref(parser);
		return;
	}

	void (*handle)(struct viewport_server *, JsonObject *,
		struct viewport_ipc_client *) = NULL;
	for (size_t i = 0; i < sizeof(ipc_handlers) / sizeof(ipc_handlers[0]);
			i++) {
		if (strcmp(type, ipc_handlers[i].type) == 0) {
			handle = ipc_handlers[i].handle;
			break;
		}
	}

	if (handle != NULL) {
		handle(server, object, origin);
	} else {
		wlr_log(WLR_DEBUG, "unknown IPC message type '%s'", type);
		char msg[256];
		snprintf(msg, sizeof(msg), "unknown IPC message type '%s'", type);
		notify_error(server, type, msg, origin);
	}

	g_object_unref(parser);
}

/* ------------------------------------------------------------------------
 * UNIX socket transport
 * --------------------------------------------------------------------- */

static int handle_client_event(int fd, uint32_t mask, void *data)
{
	struct viewport_ipc_client *client = data;

	if (mask & (WL_EVENT_HANGUP | WL_EVENT_ERROR)) {
		ipc_client_close(client);
		return 0;
	}

	/* Drain the backlog first: the socket has just told us it has room, and
	 * this is the only thing that will send what a short write left behind. */
	if (mask & WL_EVENT_WRITABLE) {
		ipc_client_flush(client);
		if (client->dead) {
			return 0;
		}
	}

	if (!(mask & WL_EVENT_READABLE)) {
		return 0;
	}

	char chunk[4096];
	ssize_t n = read(fd, chunk, sizeof(chunk));
	if (n <= 0) {
		if (n < 0 && (errno == EAGAIN || errno == EINTR)) {
			return 0;
		}
		ipc_client_close(client);
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
			ipc_client_close(client);
			return 0;
		}
		char *buf = realloc(client->buf, cap);
		if (buf == NULL) {
			ipc_client_close(client);
			return 0;
		}
		client->buf = buf;
		client->cap = cap;
	}

	memcpy(client->buf + client->len, chunk, (size_t)n);
	client->len += (size_t)n;

	/* Dispatch each complete line, keeping any trailing partial.
	 *
	 * Handling a message can drop this very client — the reply is broadcast,
	 * and a client that has already gone away fails the write. Its buffer must
	 * not be touched again after that, neither for the next line nor for the
	 * compaction below. */
	size_t start = 0;
	for (size_t i = 0; i < client->len; i++) {
		if (client->buf[i] != '\n') {
			continue;
		}
		client->buf[i] = '\0';
		if (i > start) {
			viewport_ipc_handle(client->ipc->server, client->buf + start,
				i - start, client);
			if (client->dead) {
				return 0;
			}
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

	/* accept4(), not accept(): the listening socket's SOCK_CLOEXEC is not
	 * inherited by the connection, so a plain accept() hands every program
	 * launched from a keybinding a live IPC connection that already passed
	 * somebody else's credential check. */
	int client_fd = accept4(fd, NULL, NULL, SOCK_CLOEXEC | SOCK_NONBLOCK);
	if (client_fd < 0) {
		return 0;
	}

#ifdef SO_PEERCRED
	struct ucred cred;
	socklen_t len = sizeof(cred);
	if (getsockopt(client_fd, SOL_SOCKET, SO_PEERCRED, &cred, &len) == 0) {
		if (cred.uid != getuid() && cred.uid != 0) {
			wlr_log(WLR_ERROR, "IPC client connection refused: UID %u != %u",
				(unsigned)cred.uid, (unsigned)getuid());
			close(client_fd);
			return 0;
		}
	}
#endif

	struct viewport_ipc_client *client = calloc(1, sizeof(*client));
	if (client == NULL) {
		close(client_fd);
		return 0;
	}
	client->ipc = ipc;
	client->fd = client_fd;
	client->source = wl_event_loop_add_fd(ipc->server->wl_event_loop, client_fd,
		WL_EVENT_READABLE, handle_client_event, client);
	wl_list_insert(&ipc->clients, &client->link);

	/* Bring the newcomer up to date immediately: outputs, then every view
	 * that mapped before it connected. Each of these broadcasts, so the client
	 * may be gone by the last one — harmless, but nothing below may assume it
	 * is still there. */
	viewport_ipc_notify_output_layout(ipc->server);
	viewport_ipc_notify_config(ipc->server);
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
	wl_list_init(&ipc->dead);

	if (path != NULL) {
		ipc->path = strdup(path);
	} else {
		const char *runtime_dir = getenv("XDG_RUNTIME_DIR");
		if (runtime_dir == NULL) {
			runtime_dir = "/tmp";
		}
		/* Named after the Wayland display, not the pid.
		 *
		 * Both are unique per session, but only one is discoverable: a script
		 * already has WAYLAND_DISPLAY and would have to go hunting for the
		 * compositor's pid. It is also what this file, --help and the docs
		 * all said the path was, while the code used the pid — so the
		 * documented socat line named a socket that never existed.
		 *
		 * VIEWPORT_SOCKET is still exported for anything that would rather not
		 * assemble the path at all. */
		char buf[PATH_MAX];
		const char *display = server->socket_name != NULL
			? server->socket_name : getenv("WAYLAND_DISPLAY");
		if (display != NULL) {
			snprintf(buf, sizeof(buf), "%s/viewport-%s.sock", runtime_dir,
				display);
		} else {
			snprintf(buf, sizeof(buf), "%s/viewport-%d.sock", runtime_dir,
				(int)getpid());
		}
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
	/* bind() creates the node at 0755 and the chmod() below only narrows it
	 * afterwards, so without the umask there is a window in which anyone can
	 * connect. XDG_RUNTIME_DIR is 0700 and hides it, but the fallback above is
	 * /tmp, which everyone can walk into. The chmod() stays because some
	 * filesystems ignore the umask when creating a socket node. */
	mode_t old_umask = umask(0177);
	int bind_result = bind(ipc->fd, (struct sockaddr *)&addr, sizeof(addr));
	umask(old_umask);
	if (bind_result < 0) {
		wlr_log(WLR_ERROR, "bind %s: %s", ipc->path, strerror(errno));
		goto error_fd;
	}
	chmod(ipc->path, 0600);
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

	struct viewport_ipc_client *client, *tmp;
	wl_list_for_each_safe(client, tmp, &ipc->clients, link) {
		ipc_client_close(client);
	}
	/* Now rather than from the idle callback: that callback would fire after
	 * this struct is freed, and there is no dispatch left in flight to protect
	 * the clients from. */
	if (ipc->reaper != NULL) {
		wl_event_source_remove(ipc->reaper);
		ipc->reaper = NULL;
	}
	ipc_reap(ipc);

	if (ipc->source != NULL) {
		wl_event_source_remove(ipc->source);
	}
	close(ipc->fd);
	unlink(ipc->path);
	free(ipc->path);
	free(ipc);
}
