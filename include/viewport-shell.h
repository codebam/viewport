/* SPDX-License-Identifier: MIT
 *
 * The shell, and what feeds it.
 *
 * The web engine, the JSON control channel both transports share, the GLib loop
 that WebKit needs and the Wayland loop nests inside, and the three sources of
 content the shell renders but does not own: status samples, notifications and
 the saved layout.
 *
 * Split out of viewport.h, which had grown to declare every function in the
 * project: a comment edited in it recompiled all twenty-nine translation
 * units, and nothing recorded which of them any given declaration was for.
 * The types stay there — struct viewport_server embeds them by value, so
 * everything needs it — and only the interfaces moved.
 */
#ifndef VIEWPORT_SHELL_H
#define VIEWPORT_SHELL_H

#include "viewport.h"

/* The layout, remembered across restarts. The blob is the shell's own format;
 * the compositor stores and returns it without interpreting it. */
void viewport_session_save(struct viewport_server *server, const char *state);
char *viewport_session_load(struct viewport_server *server);
void viewport_ipc_notify_session(struct viewport_server *server);

/* Notifications: the compositor claims org.freedesktop.Notifications and the
 * shell draws them, so their appearance is the stylesheet already in use. */
struct viewport_notifications *viewport_notifications_create(
	struct viewport_server *server);
void viewport_notifications_destroy(
	struct viewport_notifications *notifications);
void viewport_notifications_closed(struct viewport_notifications *notifications,
	uint32_t id, uint32_t reason);
void viewport_notifications_dismissed(
	struct viewport_notifications *notifications, uint32_t id);
void viewport_notifications_expired(
	struct viewport_notifications *notifications, uint32_t id);
void viewport_notifications_action(struct viewport_notifications *notifications,
	uint32_t id, const char *action);

void viewport_ipc_notify_notification(struct viewport_server *server,
	uint32_t id, const char *app_name, const char *icon, const char *summary,
	const char *body, uint8_t urgency, int32_t timeout,
	const char *const *action_keys, const char *const *action_labels,
	size_t action_count);
void viewport_ipc_notify_notification_closed(struct viewport_server *server,
	uint32_t id);

/* -------------------------------------------------------------------------
 * appearance.c
 *
 * Serves org.freedesktop.appearance/color-scheme so client applications pick
 * a dark or light theme. Styling the shell cannot do this: every toolkit asks
 * the portal, and with nothing answering they all default to light.
 * ---------------------------------------------------------------------- */

struct viewport_appearance *viewport_appearance_create(
	struct viewport_server *server, bool dark);
void viewport_appearance_destroy(struct viewport_appearance *appearance);
void viewport_appearance_set_dark(struct viewport_appearance *appearance,
	bool dark);
bool viewport_appearance_is_dark(struct viewport_appearance *appearance);

/* -------------------------------------------------------------------------
 * status.c
 *
 * Samples /proc for the shell's status bar. Only raw values — formatting,
 * icons and which modules exist are the shell's business.
 * ---------------------------------------------------------------------- */

struct viewport_status *viewport_status_create(struct viewport_server *server);
void viewport_status_destroy(struct viewport_status *status);

/* -------------------------------------------------------------------------
 * ipc.c
 * ---------------------------------------------------------------------- */

struct viewport_ipc *viewport_ipc_create(struct viewport_server *server,
	const char *path);
void viewport_ipc_destroy(struct viewport_ipc *ipc);

/* Handles one JSON message from either transport (UNIX socket or the page's
 * script message handler). `json` is a NUL-terminated UTF-8 document.
 *
 * `origin` is the socket client the message arrived on, or NULL for the page.
 * It exists so an error caused by one client's message is answered to that
 * client instead of broadcast: an unknown message type poked at the socket
 * used to write a console.error into the shell, which is not the shell's
 * problem and not something it can act on. */
void viewport_ipc_handle(struct viewport_server *server, const char *json,
	size_t len, struct viewport_ipc_client *origin);

/* Sends any accumulated drag delta and clears it. Called once per output
 * frame; see the delta_* fields on the server for why. */
void viewport_ipc_flush_deltas(struct viewport_server *server);

/* Pushes a JSON event to every listener: socket clients and the page. */
void viewport_ipc_broadcast(struct viewport_server *server, const char *json);

/* Convenience emitters used by the shell/xdg glue. */
void viewport_ipc_notify_view_added(struct viewport_toplevel *toplevel);
void viewport_ipc_notify_view_removed(struct viewport_toplevel *toplevel);
void viewport_ipc_notify_view_props(struct viewport_toplevel *toplevel);
void viewport_ipc_notify_output_layout(struct viewport_server *server);

/* Settings the shell needs in order to render: which layout model to use.
 * Sent on connect and on page load, alongside the view replay. */
void viewport_ipc_notify_config(struct viewport_server *server);
/* Tell the shell whether Mod4 is held. Only sent when the bar is set to
 * "auto", because nothing else reacts to it and modifier traffic is otherwise
 * pure noise on the socket. */
void viewport_ipc_notify_modifiers(struct viewport_server *server, bool logo);

/* Replays view.added for every currently mapped toplevel.
 *
 * view.added is an edge, not a state: a client that maps before the shell has
 * finished loading — or before an external tool connects — would otherwise be
 * invisible to it forever. Sent on page load, on socket connect, and on
 * explicit view.query, which also makes shell reloads non-destructive. */
void viewport_ipc_notify_views(struct viewport_server *server);

/* Tells the shell which view now holds focus; id 0 means the shell itself. */
void viewport_ipc_notify_focus(struct viewport_server *server, uint32_t id);

/* Forwards a keybinding to the shell as {"type":"shell.command",...}, so
 * workspaces and other layout policy can live in JS where the layout already
 * is, while still being bound to a key in the config file. */
void viewport_ipc_notify_shell_command(struct viewport_server *server,
	const char *command);

/* Asks the shell to fullscreen a view, or to stop. Fullscreen is the shell's
 * decision — it owns the tiling tree — so every path that learns a client
 * wants it (xdg, Xwayland, foreign-toplevel, and the map-time replay for a
 * request that arrived before view.added) forwards it through here rather
 * than formatting the same shell.command by hand. */
void viewport_ipc_notify_fullscreen(struct viewport_toplevel *toplevel,
	bool on);

/* -------------------------------------------------------------------------
 * glib_loop.c
 *
 * WebKit needs a GMainContext; wlroots needs a wl_event_loop. We run GLib as
 * the outer loop and attach the Wayland loop's epoll fd to it as a GSource,
 * flushing clients in the prepare phase.
 * ---------------------------------------------------------------------- */

struct viewport_glib_loop;
struct viewport_status;
struct viewport_appearance;

struct viewport_glib_loop *viewport_glib_loop_create(
	struct wl_display *display);
void viewport_glib_loop_run(struct viewport_glib_loop *loop);
void viewport_glib_loop_quit(struct viewport_glib_loop *loop);
void viewport_glib_loop_destroy(struct viewport_glib_loop *loop);

/* -------------------------------------------------------------------------
 * web.c
 * ---------------------------------------------------------------------- */

struct viewport_web *viewport_web_create(struct viewport_server *server);
void viewport_web_destroy(struct viewport_web *web);

void viewport_web_resize(struct viewport_web *web, int width, int height);

/* Called once per presented output frame. Releases the frame WebKit is
 * waiting on, which is what unblocks it to paint the next one — the shell's
 * frame pacing is therefore driven by real vblank, not a timer. */
void viewport_web_notify_presented(struct viewport_web *web);

/* True while a WebKit frame is waiting to be acknowledged. Diagnostic only. */
bool viewport_web_has_pending(struct viewport_web *web);

/* Scene node backing the shell, used for hit-testing. */
struct wlr_scene_buffer *viewport_web_scene_buffer(struct viewport_web *web);

/* Re-fetches the shell from its URL, discarding the current document. */
void viewport_web_reload(struct viewport_web *web);

/* Sends a JSON string to the page as a `viewport` CustomEvent. */
void viewport_web_post_to_page(struct viewport_web *web, const char *json);

/* Routes an input event to the shell. Coordinates are layout-space. */
void viewport_web_pointer_motion(struct viewport_web *web, uint32_t time_msec,
	double lx, double ly);
void viewport_web_pointer_button(struct viewport_web *web, uint32_t time_msec,
	double lx, double ly, uint32_t button, bool pressed);
void viewport_web_pointer_axis(struct viewport_web *web, uint32_t time_msec,
	double lx, double ly, double dx, double dy, bool precise);
void viewport_web_keyboard_key(struct viewport_web *web, uint32_t time_msec,
	uint32_t keycode, uint32_t keysym, bool pressed, uint32_t modifiers);
void viewport_web_focus(struct viewport_web *web, bool focused);

/* web_buffer.c wraps a WPEBufferDMABuf in a wlr_buffer without copying. It is
 * declared in src/web-internal.h rather than here: nothing outside the web
 * engine has any business making one, and the signature needs WPE types. */

#endif /* VIEWPORT_SHELL_H */
