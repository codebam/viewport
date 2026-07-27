/* SPDX-License-Identifier: MIT
 *
 * WebKit boot, URL loading with an offline fallback, and the page bridge.
 */
#define _POSIX_C_SOURCE 200809L

#include <stdlib.h>
#include <string.h>
#include <xf86drm.h>

#include <json-glib/json-glib.h>

#include <wlr/render/wlr_renderer.h>
#include <wlr/util/log.h>

#include "web-internal.h"

/* ------------------------------------------------------------------------
 * Page bridge
 * --------------------------------------------------------------------- */

/* Re-encodes arbitrary text as a JSON string literal, which is also a valid
 * JavaScript string literal. Building the script with string concatenation
 * instead would let any window title containing a quote execute as code in
 * the shell's origin. */
static char *js_string_literal(const char *text)
{
	JsonNode *node = json_node_new(JSON_NODE_VALUE);
	json_node_set_string(node, text);

	JsonGenerator *generator = json_generator_new();
	json_generator_set_root(generator, node);
	char *literal = json_generator_to_data(generator, NULL);

	json_node_free(node);
	g_object_unref(generator);
	return literal;
}

void viewport_web_post_to_page(struct viewport_web *web, const char *json)
{
	if (web == NULL || web->web_view == NULL) {
		return;
	}

	char *literal = js_string_literal(json);
	char *script = g_strdup_printf(
		"window.dispatchEvent(new CustomEvent('viewport',"
		"{detail:JSON.parse(%s)}));", literal);

	webkit_web_view_evaluate_javascript(web->web_view, script, -1, NULL, NULL,
		NULL, NULL, NULL);

	g_free(script);
	g_free(literal);
}

static void handle_script_message(WebKitUserContentManager *manager,
	JSCValue *value, gpointer user_data)
{
	struct viewport_web *web = user_data;

	/* Accept either a JSON string or a live object, so page authors can call
	 * postMessage({...}) without stringifying by hand. */
	char *text = jsc_value_is_string(value)
		? jsc_value_to_string(value)
		: jsc_value_to_json(value, 0);
	if (text == NULL) {
		return;
	}

	/* No origin: the page is not one client among several, and an error it
	 * caused is one the shell must see on the broadcast channel it already
	 * listens to. */
	viewport_ipc_handle(web->server, text, strlen(text), NULL);
	g_free(text);
}

/* ------------------------------------------------------------------------
 * Load handling and fallback
 * --------------------------------------------------------------------- */

static void load_fallback(struct viewport_web *web, const char *reason)
{
	if (web->using_fallback) {
		return;
	}
	web->using_fallback = true;

	wlr_log(WLR_ERROR, "shell unavailable (%s); loading %s", reason,
		web->server->config.fallback_url);
	webkit_web_view_load_uri(web->web_view, web->server->config.fallback_url);
}

static gboolean handle_load_timeout(gpointer user_data)
{
	struct viewport_web *web = user_data;
	web->load_timeout_id = 0;

	/* The deadline is on the first *painted frame*, not on the load event. A
	 * server that accepts the connection and then stalls, or a shell whose JS
	 * throws before it renders, both leave the user staring at a black screen
	 * — and both are invisible to load-failed. */
	if (!web->first_frame_seen) {
		load_fallback(web, "first-paint timeout");
	}

	return G_SOURCE_REMOVE;
}

static void handle_load_failed(WebKitWebView *view, WebKitLoadEvent event,
	const char *failing_uri, GError *error, gpointer user_data)
{
	struct viewport_web *web = user_data;
	load_fallback(web, error != NULL ? error->message : "load failed");
}

/* A web process that dies takes the shell with it and says nothing: the last
 * frame it painted stays on screen, so the desktop looks alive while nothing
 * responds. Worth a loud line, because every other symptom points elsewhere. */
static void handle_web_process_terminated(WebKitWebView *view,
	WebKitWebProcessTerminationReason reason, gpointer user_data)
{
	static const char *const reasons[] = {
		[WEBKIT_WEB_PROCESS_CRASHED] = "crashed",
		[WEBKIT_WEB_PROCESS_EXCEEDED_MEMORY_LIMIT] = "exceeded its memory limit",
		[WEBKIT_WEB_PROCESS_TERMINATED_BY_API] = "was terminated by the API",
	};
	wlr_log(WLR_ERROR, "the shell's web process %s",
		reason <= WEBKIT_WEB_PROCESS_TERMINATED_BY_API
			? reasons[reason] : "died");
}

static void handle_load_changed(WebKitWebView *view, WebKitLoadEvent event,
	gpointer user_data)
{
	struct viewport_web *web = user_data;

	/* Which of these arrived is the whole diagnosis when the desktop comes up
	 * empty: a load that never commits is a URL or a sandbox problem, and one
	 * that finishes and still lays nothing out is the page's own fault. Silence
	 * here meant those two were indistinguishable in a log. */
	static const char *const names[] = {
		[WEBKIT_LOAD_STARTED] = "started",
		[WEBKIT_LOAD_REDIRECTED] = "redirected",
		[WEBKIT_LOAD_COMMITTED] = "committed",
		[WEBKIT_LOAD_FINISHED] = "finished",
	};
	wlr_log(WLR_INFO, "shell load %s: %s",
		event <= WEBKIT_LOAD_FINISHED ? names[event] : "?",
		webkit_web_view_get_uri(view));

	if (event == WEBKIT_LOAD_FINISHED) {
		/* Hand the shell its starting state as soon as it can receive it.
		 *
		 * Replaying the view list here is what makes clients that mapped
		 * before the page finished loading visible at all — and what makes a
		 * shell reload non-destructive rather than orphaning every window.
		 * The fallback page needs this just as much as the real shell, so
		 * this deliberately does not check using_fallback. */
		viewport_ipc_notify_output_layout(web->server);
		viewport_ipc_notify_views(web->server);
	}
}

/* ------------------------------------------------------------------------
 * Live reload
 *
 * Only for a file:// shell under --debug. A shell served over HTTP is somebody
 * else's dev server and already has its own reload story; watching the network
 * would mean polling. Locally, the compositor is the only thing that knows the
 * shell changed, so it may as well act on it.
 * --------------------------------------------------------------------- */

static gboolean handle_watch_debounce(gpointer user_data)
{
	struct viewport_web *web = user_data;
	web->watch_debounce_id = 0;

	wlr_log(WLR_INFO, "shell changed on disk, reloading");
	viewport_web_reload(web);

	return G_SOURCE_REMOVE;
}

static void handle_shell_changed(GFileMonitor *monitor, GFile *file,
	GFile *other, GFileMonitorEvent event, gpointer user_data)
{
	struct viewport_web *web = user_data;

	if (event != G_FILE_MONITOR_EVENT_CHANGES_DONE_HINT &&
			event != G_FILE_MONITOR_EVENT_CREATED &&
			event != G_FILE_MONITOR_EVENT_MOVED_IN) {
		return;
	}

	/* Editors save by writing a temp file and renaming, so one save can emit
	 * several events across several files. Collapse them into one reload. */
	if (web->watch_debounce_id != 0) {
		g_source_remove(web->watch_debounce_id);
	}
	web->watch_debounce_id = g_timeout_add(150, handle_watch_debounce, web);

	/* The frame-ack watchdog is deliberately left alone. Cancelling it here
	 * used to strand WebKit on an unacknowledged frame — it is the only thing
	 * that unblocks the handshake when no vblank is coming (see the commentary
	 * in wpe_view.c) — so a shell save could wedge the desktop until unrelated
	 * input happened to damage the scene. It also left ack_watchdog_id holding
	 * a dead source id for the next g_source_remove() to complain about. A file
	 * changing on disk has no bearing on the frame in flight. */
}

static void watch_shell_directory(struct viewport_web *web, const char *url)
{
	if (!g_str_has_prefix(url, "file://")) {
		return;
	}

	GFile *shell_file = g_file_new_for_uri(url);
	GFile *directory = g_file_get_parent(shell_file);
	g_object_unref(shell_file);

	if (directory == NULL) {
		return;
	}

	GError *error = NULL;
	web->watch = g_file_monitor_directory(directory, G_FILE_MONITOR_NONE, NULL,
		&error);
	if (web->watch == NULL) {
		wlr_log(WLR_INFO, "shell live reload unavailable: %s",
			error ? error->message : "unknown");
		g_clear_error(&error);
	} else {
		g_signal_connect(web->watch, "changed",
			G_CALLBACK(handle_shell_changed), web);
		char *path = g_file_get_path(directory);
		wlr_log(WLR_INFO, "watching %s for shell changes", path);
		g_free(path);
	}

	g_object_unref(directory);
}

/* ------------------------------------------------------------------------
 * Lifecycle
 * --------------------------------------------------------------------- */

static bool resolve_drm_nodes(int drm_fd, char **primary, char **render)
{
	drmDevicePtr device = NULL;
	if (drmGetDevice(drm_fd, &device) != 0 || device == NULL) {
		return false;
	}

	if (device->available_nodes & (1 << DRM_NODE_PRIMARY)) {
		*primary = strdup(device->nodes[DRM_NODE_PRIMARY]);
	}
	if (device->available_nodes & (1 << DRM_NODE_RENDER)) {
		*render = strdup(device->nodes[DRM_NODE_RENDER]);
	}

	drmFreeDevice(&device);
	return *render != NULL;
}

struct viewport_web *viewport_web_create(struct viewport_server *server)
{
	struct viewport_web *web = calloc(1, sizeof(*web));
	if (web == NULL) {
		return NULL;
	}
	web->server = server;
	wl_list_init(&web->buffers);

	int drm_fd = wlr_renderer_get_drm_fd(server->renderer);
	if (drm_fd < 0) {
		wlr_log(WLR_ERROR, "renderer has no DRM fd; cannot share buffers");
		goto error;
	}

	if (!resolve_drm_nodes(drm_fd, &web->primary_node, &web->render_node)) {
		wlr_log(WLR_ERROR, "could not resolve a DRM render node");
		goto error;
	}
	wlr_log(WLR_INFO, "WPE will allocate on %s", web->render_node);

	/* Timeline that carries WebKit's per-frame rendering fences. */
	web->timeline = wlr_drm_syncobj_timeline_create(drm_fd);
	if (web->timeline == NULL) {
		wlr_log(WLR_INFO,
			"no drm_syncobj timeline; shell frames will sync implicitly");
	}

	web->display = viewport_wpe_display_new(server, web->primary_node,
		web->render_node);

	GError *error = NULL;
	if (!wpe_display_connect(WPE_DISPLAY(web->display), &error)) {
		wlr_log(WLR_ERROR, "wpe_display_connect: %s",
			error ? error->message : "unknown");
		g_clear_error(&error);
		goto error;
	}

	/* The shell's node. Created empty; the first render_buffer fills it. */
	web->scene_buffer = wlr_scene_buffer_create(server->layer_web, NULL);
	if (web->scene_buffer == NULL) {
		goto error;
	}
	wlr_scene_node_lower_to_bottom(&web->scene_buffer->node);

	web->ucm = webkit_user_content_manager_new();
	webkit_user_content_manager_register_script_message_handler(web->ucm,
		"viewport", NULL);
	g_signal_connect(web->ucm, "script-message-received::viewport",
		G_CALLBACK(handle_script_message), web);

	web->web_view = g_object_new(WEBKIT_TYPE_WEB_VIEW,
		"display", WPE_DISPLAY(web->display),
		"user-content-manager", web->ucm,
		NULL);
	if (web->web_view == NULL) {
		goto error;
	}

	web->wpe_view = webkit_web_view_get_wpe_view(web->web_view);
	if (web->wpe_view == NULL) {
		wlr_log(WLR_ERROR, "WebKitWebView produced no WPEView");
		goto error;
	}
	viewport_wpe_view_set_web(VIEWPORT_WPE_VIEW(web->wpe_view), web);

	/* A shell that throws during startup renders nothing and says nothing;
	 * the compositor just shows a blank desktop. Mirroring the page console
	 * into our log makes that failure mode debuggable. */
	if (server->config.debug) {
		webkit_settings_set_enable_write_console_messages_to_stdout(
			webkit_web_view_get_settings(web->web_view), TRUE);

		/* Serve the shell from the network every time. WebKit will otherwise
		 * happily reuse a cached shell.js across compositor restarts, so an
		 * edited shell appears to have no effect — and the stale line numbers
		 * in console output send you hunting through code that is not running. */
		webkit_web_context_set_cache_model(
			webkit_web_view_get_context(web->web_view),
			WEBKIT_CACHE_MODEL_DOCUMENT_VIEWER);
	}

	/* The shell is the desktop background: never let a page's body colour
	 * paint over the compositor's idea of what is behind it. */
	WebKitColor transparent = { 0.0, 0.0, 0.0, 0.0 };
	webkit_web_view_set_background_color(web->web_view, &transparent);

	g_signal_connect(web->web_view, "load-failed",
		G_CALLBACK(handle_load_failed), web);
	g_signal_connect(web->web_view, "load-changed",
		G_CALLBACK(handle_load_changed), web);
	g_signal_connect(web->web_view, "web-process-terminated",
		G_CALLBACK(handle_web_process_terminated), web);

	int width, height;
	viewport_layout_size(server, &width, &height);
	viewport_web_resize(web, width, height);

	wpe_view_map(web->wpe_view);
	wpe_view_focus_in(web->wpe_view);

	if (server->config.debug) {
		watch_shell_directory(web, server->config.url);
	}

	webkit_web_view_load_uri(web->web_view, server->config.url);

	if (server->config.load_timeout_ms > 0) {
		web->load_timeout_id = g_timeout_add(server->config.load_timeout_ms,
			handle_load_timeout, web);
	}

	return web;

error:
	viewport_web_destroy(web);
	return NULL;
}

void viewport_web_destroy(struct viewport_web *web)
{
	if (web == NULL) {
		return;
	}

	if (web->load_timeout_id != 0) {
		g_source_remove(web->load_timeout_id);
	}
	if (web->watch_debounce_id != 0) {
		g_source_remove(web->watch_debounce_id);
	}
	if (web->ack_watchdog_id != 0) {
		g_source_remove(web->ack_watchdog_id);
	}
	g_clear_object(&web->watch);

	/* Order is load-bearing.
	 *
	 * Our wlr_buffers call wpe_view_buffer_released() from their destroy
	 * handler. If WebKit is torn down first, the WPEView is already gone by
	 * then and that call lands on freed memory:
	 *   wpe_view_buffer_released: assertion 'WPE_IS_VIEW(view)' failed
	 *
	 * So drop the scene's reference first, while the view is still alive,
	 * then release the pending frame, then destroy WebKit. */
	if (web->scene_buffer != NULL) {
		wlr_scene_buffer_set_buffer(web->scene_buffer, NULL);
	}
	if (web->pending != NULL) {
		g_object_unref(web->pending);
		web->pending = NULL;
	}

	g_clear_object(&web->web_view);
	/* Borrowed from web_view, so it dangles the moment that is unreffed. */
	web->wpe_view = NULL;

	/* Anything still holding one of our buffers — the DRM backend keeps them
	 * as scanout framebuffers until wlr_backend_destroy() — must not follow
	 * this pointer once the struct is freed a few lines below. */
	viewport_web_buffers_detach(web);

	g_clear_object(&web->ucm);
	g_clear_object(&web->display);

	if (web->timeline != NULL) {
		wlr_drm_syncobj_timeline_unref(web->timeline);
	}

	free(web->primary_node);
	free(web->render_node);
	free(web);
}

bool viewport_web_has_pending(struct viewport_web *web)
{
	return web != NULL && web->pending != NULL;
}

struct wlr_scene_buffer *viewport_web_scene_buffer(struct viewport_web *web)
{
	return web->scene_buffer;
}

void viewport_web_reload(struct viewport_web *web)
{
	if (web == NULL || web->web_view == NULL) {
		return;
	}
	/* Bypass the cache: the entire point of a reload bind is to pick up an
	 * edited shell, and a cached document defeats that. */
	web->using_fallback = false;
	webkit_web_view_reload_bypass_cache(web->web_view);
}

void viewport_web_resize(struct viewport_web *web, int width, int height)
{
	if (web == NULL || (web->width == width && web->height == height)) {
		return;
	}
	web->width = width;
	web->height = height;

	if (web->scene_buffer != NULL) {
		wlr_scene_buffer_set_dest_size(web->scene_buffer, width, height);
	}
	if (web->wpe_view != NULL) {
		wpe_view_resized(web->wpe_view, width, height);
	}
}

/* ------------------------------------------------------------------------
 * Input
 * --------------------------------------------------------------------- */

void viewport_web_pointer_motion(struct viewport_web *web, uint32_t time_msec,
	double lx, double ly)
{
	if (web->wpe_view == NULL) {
		return;
	}

	/* Negative coordinates are input.c's signal that the pointer moved onto a
	 * client window; WebKit needs a leave or :hover state sticks. */
	WPEEventType type = (lx < 0 || ly < 0)
		? WPE_EVENT_POINTER_LEAVE : WPE_EVENT_POINTER_MOVE;

	WPEEvent *event = wpe_event_pointer_move_new(type, web->wpe_view,
		WPE_INPUT_SOURCE_MOUSE, time_msec, 0, lx, ly, 0, 0);
	wpe_view_event(web->wpe_view, event);
	wpe_event_unref(event);
}

void viewport_web_pointer_button(struct viewport_web *web, uint32_t time_msec,
	double lx, double ly, uint32_t button, bool pressed)
{
	if (web->wpe_view == NULL) {
		return;
	}

	/* evdev BTN_LEFT/RIGHT/MIDDLE are 0x110-0x112; WPE numbers buttons from
	 * one, in the order left, middle, right. */
	guint wpe_button;
	switch (button) {
	case 0x110: wpe_button = 1; break;
	case 0x112: wpe_button = 2; break;
	case 0x111: wpe_button = 3; break;
	default: wpe_button = button - 0x10f; break;
	}

	WPEEvent *event = wpe_event_pointer_button_new(
		pressed ? WPE_EVENT_POINTER_DOWN : WPE_EVENT_POINTER_UP,
		web->wpe_view, WPE_INPUT_SOURCE_MOUSE, time_msec, 0, wpe_button,
		lx, ly, pressed ? 1 : 0);
	wpe_view_event(web->wpe_view, event);
	wpe_event_unref(event);
}

void viewport_web_pointer_axis(struct viewport_web *web, uint32_t time_msec,
	double lx, double ly, double dx, double dy, bool precise)
{
	if (web->wpe_view == NULL) {
		return;
	}

	WPEEvent *event = wpe_event_scroll_new(web->wpe_view,
		WPE_INPUT_SOURCE_MOUSE, time_msec, 0, -dx, -dy, precise, FALSE,
		lx, ly);
	wpe_view_event(web->wpe_view, event);
	wpe_event_unref(event);
}

void viewport_web_keyboard_key(struct viewport_web *web, uint32_t time_msec,
	uint32_t keycode, uint32_t keysym, bool pressed, uint32_t modifiers)
{
	if (web->wpe_view == NULL) {
		return;
	}

	WPEEvent *event = wpe_event_keyboard_new(
		pressed ? WPE_EVENT_KEYBOARD_KEY_DOWN : WPE_EVENT_KEYBOARD_KEY_UP,
		web->wpe_view, WPE_INPUT_SOURCE_KEYBOARD, time_msec, modifiers,
		keycode, keysym);
	wpe_view_event(web->wpe_view, event);
	wpe_event_unref(event);
}

void viewport_web_focus(struct viewport_web *web, bool focused)
{
	if (web == NULL || web->wpe_view == NULL) {
		return;
	}
	if (focused) {
		wpe_view_focus_in(web->wpe_view);
	} else {
		wpe_view_focus_out(web->wpe_view);
	}
}
