/* SPDX-License-Identifier: MIT */
#define _POSIX_C_SOURCE 200809L

#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <wlr/render/allocator.h>
#include <wlr/render/wlr_renderer.h>
#include <wlr/types/wlr_data_device.h>
#include <wlr/types/wlr_layer_shell_v1.h>
#include <wlr/types/wlr_xdg_decoration_v1.h>
#include <wlr/types/wlr_ext_image_capture_source_v1.h>
#include <wlr/types/wlr_ext_image_copy_capture_v1.h>
#include <wlr/types/wlr_linux_drm_syncobj_v1.h>
#include <wlr/types/wlr_presentation_time.h>
#include <wlr/types/wlr_primary_selection_v1.h>
#include <wlr/types/wlr_virtual_keyboard_v1.h>
#include <wlr/types/wlr_xdg_activation_v1.h>
#include <wlr/types/wlr_data_control_v1.h>
#include <wlr/types/wlr_ext_data_control_v1.h>
#include <wlr/types/wlr_fractional_scale_v1.h>
#include <wlr/types/wlr_gamma_control_v1.h>
#include <wlr/types/wlr_idle_inhibit_v1.h>
#include <wlr/types/wlr_idle_notify_v1.h>
#include <wlr/types/wlr_alpha_modifier_v1.h>
#include <wlr/types/wlr_content_type_v1.h>
#include <wlr/types/wlr_screencopy_v1.h>
#include <wlr/types/wlr_tearing_control_v1.h>
#include <wlr/types/wlr_xdg_activation_v1.h>
#include <wlr/types/wlr_session_lock_v1.h>
#include <wlr/types/wlr_single_pixel_buffer_v1.h>
#include <wlr/types/wlr_server_decoration.h>
#include <wlr/types/wlr_subcompositor.h>
#include <wlr/types/wlr_viewporter.h>
#include <wlr/types/wlr_xdg_output_v1.h>
#include <wlr/util/log.h>

#include "viewport.h"

static void handle_session_active(struct wl_listener *listener, void *data);

static void handle_renderer_lost(struct wl_listener *listener, void *data)
{
	/* GPU reset or device removal. Recreating the renderer means every
	 * imported texture — including WebKit's — is invalid. Bail loudly rather
	 * than limp along presenting garbage. */
	struct viewport_server *server =
		wl_container_of(listener, server, renderer_lost);
	wlr_log(WLR_ERROR, "renderer lost, shutting down");
	viewport_server_terminate(server);
}

bool viewport_server_init(struct viewport_server *server,
	const struct viewport_config *config)
{
	server->config = *config;
	server->next_view_id = 1;

	wl_list_init(&server->outputs);
	wl_list_init(&server->toplevels);
	wl_list_init(&server->keyboards);
	wl_list_init(&server->bindings);

	server->wl_display = wl_display_create();
	if (server->wl_display == NULL) {
		wlr_log(WLR_ERROR, "wl_display_create failed");
		return false;
	}
	server->wl_event_loop = wl_display_get_event_loop(server->wl_display);

	server->backend = wlr_backend_autocreate(server->wl_event_loop,
		&server->session);
	if (server->backend == NULL) {
		wlr_log(WLR_ERROR, "wlr_backend_autocreate failed");
		return false;
	}

	server->renderer = wlr_renderer_autocreate(server->backend);
	if (server->renderer == NULL) {
		wlr_log(WLR_ERROR, "wlr_renderer_autocreate failed");
		return false;
	}
	server->renderer_lost.notify = handle_renderer_lost;
	wl_signal_add(&server->renderer->events.lost, &server->renderer_lost);

	if (!wlr_renderer_init_wl_display(server->renderer, server->wl_display)) {
		wlr_log(WLR_ERROR, "wlr_renderer_init_wl_display failed");
		return false;
	}

	server->allocator = wlr_allocator_autocreate(server->backend,
		server->renderer);
	if (server->allocator == NULL) {
		wlr_log(WLR_ERROR, "wlr_allocator_autocreate failed");
		return false;
	}

	/* wl_compositor 6 gives clients wl_surface.preferred_buffer_scale and
	 * fractional-scale negotiation. */
	server->compositor = wlr_compositor_create(server->wl_display, 6,
		server->renderer);
	wlr_subcompositor_create(server->wl_display);
	wlr_data_device_manager_create(server->wl_display);
	wlr_viewporter_create(server->wl_display);
	wlr_presentation_create(server->wl_display, server->backend, 2);

	/* Note: the zero-copy import path for ordinary clients (GTK, Qt, Vulkan
	 * games) is already live at this point. wlr_renderer_init_wl_display()
	 * above sets up wl_shm and linux-dmabuf with per-device format feedback,
	 * so clients allocate buffers we can scan out directly. Calling
	 * wlr_linux_dmabuf_v1_create_with_renderer() as well double-registers the
	 * buffer factory:
	 *   wlr_resource_buffer_interface wlr_dmabuf_v1_buffer already registered
	 */

	/* Screen capture.
	 *
	 * Not a nicety: without one of these, nothing running *inside* the
	 * compositor can take a screenshot, so there is no way to see what the
	 * desktop actually looks like from within its own session — which is the
	 * only vantage point available once you are no longer nested inside
	 * another compositor. grim speaks both protocols; ext-image-copy-capture
	 * is the newer one and wlr-screencopy is kept for older clients.
	 *
	 * Capture goes through the renderer rather than reading our buffers, so
	 * this stays compatible with web_buffer.c refusing CPU access. */
	wlr_ext_image_copy_capture_manager_v1_create(server->wl_display, 1);
	wlr_ext_output_image_capture_source_manager_v1_create(server->wl_display, 1);
	wlr_screencopy_manager_v1_create(server->wl_display);

	/* Explicit sync. Vulkan clients hand us drm_syncobj timeline points
	 * instead of relying on implicit dma-buf fences, which is what makes the
	 * Vulkan renderer worth having. Only meaningful if the device supports
	 * timeline syncobjs, hence the fd check. */
	int drm_fd = wlr_renderer_get_drm_fd(server->renderer);
	if (drm_fd >= 0) {
		if (wlr_linux_drm_syncobj_manager_v1_create(server->wl_display, 1,
				drm_fd) == NULL) {
			wlr_log(WLR_INFO,
				"explicit sync unavailable; falling back to implicit fences");
		}
	}

	server->output_layout = wlr_output_layout_create(server->wl_display);
	wlr_xdg_output_manager_v1_create(server->wl_display, server->output_layout);

	server->scene = wlr_scene_create();
	server->scene_layout = wlr_scene_attach_output_layout(server->scene,
		server->output_layout);

	/* Layer order is the whole compositing policy. Creation order is stacking
	 * order, bottom first: the web shell is the desktop, layer-shell
	 * background and bottom sit on top of it, then app windows, then
	 * layer-shell top and overlay so a launcher can cover a window. */
	server->layer_web = wlr_scene_tree_create(&server->scene->tree);
	server->layer_bg = wlr_scene_tree_create(&server->scene->tree);
	server->layer_bottom = wlr_scene_tree_create(&server->scene->tree);
	server->layer_apps = wlr_scene_tree_create(&server->scene->tree);
	server->layer_top = wlr_scene_tree_create(&server->scene->tree);
	server->layer_overlay = wlr_scene_tree_create(&server->scene->tree);
	server->layer_lock = wlr_scene_tree_create(&server->scene->tree);
	wlr_scene_node_set_enabled(&server->layer_lock->node, false);

	server->new_output.notify = viewport_handle_new_output;
	wl_signal_add(&server->backend->events.new_output, &server->new_output);

	server->new_input.notify = viewport_handle_new_input;
	wl_signal_add(&server->backend->events.new_input, &server->new_input);

	server->xdg_shell = wlr_xdg_shell_create(server->wl_display, 6);
	if (server->xdg_shell == NULL) {
		wlr_log(WLR_ERROR, "wlr_xdg_shell_create failed");
		return false;
	}
	server->new_xdg_toplevel.notify = viewport_handle_new_xdg_toplevel;
	wl_signal_add(&server->xdg_shell->events.new_toplevel,
		&server->new_xdg_toplevel);
	server->new_xdg_popup.notify = viewport_handle_new_xdg_popup;
	wl_signal_add(&server->xdg_shell->events.new_popup,
		&server->new_xdg_popup);

	/* xdg-activation carries the token a launcher hands to the app it spawns
	 * so that app may raise itself. wmenu-run asserts on its absence:
	 *   wmenu-run: menu_run: Assertion `context->activation != NULL' failed
	 * so without this the launcher aborts before drawing anything, which looks
	 * exactly like a dead keybinding. */
	/* Handling request_activate is what makes "open this link" raise the
	 * browser you already have open, rather than the request being accepted
	 * and quietly discarded. */
	server->activation = wlr_xdg_activation_v1_create(server->wl_display);
	if (server->activation != NULL) {
		server->request_activate.notify = viewport_handle_request_activate;
		wl_signal_add(&server->activation->events.request_activate,
			&server->request_activate);
	}

	/* Virtual keyboard: lets wtype and accessibility tools inject key events,
	 * and makes keyboard-driven behaviour testable without a human at the
	 * keyboard — which is how the layer-shell focus path gets exercised. */
	struct wlr_virtual_keyboard_manager_v1 *virtual_keyboard =
		wlr_virtual_keyboard_manager_v1_create(server->wl_display);
	if (virtual_keyboard != NULL) {
		server->new_virtual_keyboard.notify =
			viewport_handle_new_virtual_keyboard;
		wl_signal_add(&virtual_keyboard->events.new_virtual_keyboard,
			&server->new_virtual_keyboard);
	}

	/* Middle-click paste. Terminals warn loudly without it. */
	wlr_primary_selection_v1_device_manager_create(server->wl_display);

	/* Launchers and panels are layer-shell clients. Without this, wmenu and
	 * friends fail to bind the protocol and exit before drawing anything. */
	server->layer_shell = wlr_layer_shell_v1_create(server->wl_display, 5);
	if (server->layer_shell != NULL) {
		server->new_layer_surface.notify = viewport_handle_new_layer_surface;
		wl_signal_add(&server->layer_shell->events.new_surface,
			&server->new_layer_surface);
	}

	/* KDE's older server-decoration protocol, in addition to xdg-decoration
	 * below. This is not redundant: GTK4 sees zxdg_decoration_manager_v1 in
	 * the registry and never binds it — verified with WAYLAND_DEBUG against
	 * ghostty, which observes the global three times and ignores it — but
	 * still honours the KDE protocol. sway advertises both, which is why the
	 * same client is undecorated there and decorated here. */
	struct wlr_server_decoration_manager *kde_decoration =
		wlr_server_decoration_manager_create(server->wl_display);
	if (kde_decoration != NULL) {
		wlr_server_decoration_manager_set_default_mode(kde_decoration,
			server->config.server_decorations
				? WLR_SERVER_DECORATION_MANAGER_MODE_SERVER
				: WLR_SERVER_DECORATION_MANAGER_MODE_CLIENT);
	}

	/* Server-side decorations. The shell already draws a titlebar; letting
	 * the client draw a second one is the duplicate-titlebar effect. */
	struct wlr_xdg_decoration_manager_v1 *decoration_manager =
		wlr_xdg_decoration_manager_v1_create(server->wl_display);
	if (decoration_manager != NULL) {
		server->new_decoration.notify = viewport_handle_new_decoration;
		wl_signal_add(&decoration_manager->events.new_toplevel_decoration,
			&server->new_decoration);
	}

	/* Screen locking, and a set of small protocols that clients expect to
	 * exist. Each is a single call, and their absence shows up as a tool that
	 * silently does nothing: no gamma control means no night-light, no data
	 * control means clipboard managers see an empty clipboard, no idle notify
	 * means an idle daemon never fires. */
	viewport_session_lock_init(server);
	viewport_output_manager_init(server);
	viewport_foreign_init(server);
	wlr_gamma_control_manager_v1_create(server->wl_display);
	wlr_data_control_manager_v1_create(server->wl_display);
	wlr_ext_data_control_manager_v1_create(server->wl_display, 1);
	wlr_idle_notifier_v1_create(server->wl_display);
	wlr_idle_inhibit_v1_create(server->wl_display);
	wlr_single_pixel_buffer_manager_v1_create(server->wl_display);
	wlr_fractional_scale_manager_v1_create(server->wl_display, 1);
	wlr_content_type_manager_v1_create(server->wl_display, 1);
	wlr_alpha_modifier_v1_create(server->wl_display);

	/* Tearing control lets a game opt out of vsync for its own surface, which
	 * is the difference between smooth input and a frame of added latency in a
	 * competitive title. Advertising it does not force tearing on anything. */
	wlr_tearing_control_manager_v1_create(server->wl_display, 1);

	/* Started lazily: no X server runs until an X11 client connects. */
	viewport_xwayland_init(server);

	server->seat = wlr_seat_create(server->wl_display, "seat0");
	if (server->seat == NULL) {
		wlr_log(WLR_ERROR, "wlr_seat_create failed");
		return false;
	}
	viewport_cursor_init(server);
	viewport_pointer_init(server);
	/* After the seat: the relay follows keyboard focus through the seat's own
	 * focus_change signal. */
	server->ime = viewport_ime_create(server);

	if (server->session != NULL) {
		server->session_active.notify = handle_session_active;
		wl_signal_add(&server->session->events.active,
			&server->session_active);
	}

	server->appearance = viewport_appearance_create(server,
		server->config.dark_mode);
	server->status = viewport_status_create(server);

	server->ipc = viewport_ipc_create(server, server->config.ipc_path);
	if (server->ipc == NULL) {
		wlr_log(WLR_ERROR, "control socket setup failed");
		return false;
	}

	return true;
}

void viewport_server_terminate(struct viewport_server *server)
{
	/* Order matters only in that both must happen: the GLib loop is what is
	 * actually blocking, and wl_display_terminate keeps the Wayland side
	 * consistent for anything that inspects it during teardown. */
	if (server->glib_loop != NULL) {
		viewport_glib_loop_quit(server->glib_loop);
	}
	if (server->wl_display != NULL) {
		wl_display_terminate(server->wl_display);
	}
}

static void handle_session_active(struct wl_listener *listener, void *data)
{
	struct viewport_server *server =
		wl_container_of(listener, server, session_active);

	if (server->session == NULL || !server->session->active) {
		return;
	}

	/* Returning from a VT switch leaves every output with nothing scheduled:
	 * the DRM fd is resumed and the buffers are valid, but no frame event is
	 * pending, so the scene never recomposites and the screen stays on
	 * whatever the console left behind. Kick one frame per output; the scene
	 * repaints fully because its swapchain buffers are new. */
	struct viewport_output *output;
	wl_list_for_each(output, &server->outputs, link) {
		wlr_output_schedule_frame(output->wlr_output);
	}

	wlr_log(WLR_DEBUG, "session resumed, repainting outputs");
}

static int handle_signal(int signal_number, void *data)
{
	struct viewport_server *server = data;
	wlr_log(WLR_INFO, "caught signal %d, shutting down", signal_number);
	viewport_server_terminate(server);
	return 0;
}

bool viewport_server_start(struct viewport_server *server)
{
	server->socket_name = wl_display_add_socket_auto(server->wl_display);
	if (server->socket_name == NULL) {
		wlr_log(WLR_ERROR, "wl_display_add_socket_auto failed");
		return false;
	}

	if (!wlr_backend_start(server->backend)) {
		wlr_log(WLR_ERROR, "wlr_backend_start failed");
		return false;
	}

	/* Without these, Ctrl+C leaves the compositor running with a detached
	 * terminal, because GLib's loop does not install any default handling. */
	wl_event_loop_add_signal(server->wl_event_loop, SIGINT, handle_signal,
		server);
	wl_event_loop_add_signal(server->wl_event_loop, SIGTERM, handle_signal,
		server);

	setenv("WAYLAND_DISPLAY", server->socket_name, 1);
	return true;
}

void viewport_server_run(struct viewport_server *server)
{
	/* WebKit owns the outer loop; the Wayland loop nests inside it. See the
	 * commentary in glib_loop.c for why it is arranged this way. */
	struct viewport_glib_loop *loop =
		viewport_glib_loop_create(server->wl_display);
	if (loop == NULL) {
		wlr_log(WLR_ERROR, "event loop setup failed");
		return;
	}

	server->glib_loop = loop;
	viewport_glib_loop_run(loop);
	server->glib_loop = NULL;

	viewport_glib_loop_destroy(loop);
}

void viewport_server_finish(struct viewport_server *server)
{
	if (server->wl_display == NULL) {
		return;
	}

	/* WebKit holds wlr_buffers; tear it down before the renderer goes. */
	if (server->web != NULL) {
		viewport_web_destroy(server->web);
		server->web = NULL;
	}
	if (server->status != NULL) {
		viewport_status_destroy(server->status);
		server->status = NULL;
	}
	if (server->appearance != NULL) {
		viewport_appearance_destroy(server->appearance);
		server->appearance = NULL;
	}
	if (server->ipc != NULL) {
		viewport_ipc_destroy(server->ipc);
		server->ipc = NULL;
	}
	if (server->output_revert != NULL) {
		/* Its GLib timer would otherwise fire into a destroyed server. */
		viewport_output_revert_cancel(server);
	}
	if (server->ime != NULL) {
		/* Before the seat goes: its focus_change listener hangs off it. */
		viewport_ime_destroy(server->ime);
		server->ime = NULL;
	}

	wl_display_destroy_clients(server->wl_display);

	/* Listeners must come off before the objects they are attached to are
	 * destroyed: wlr_cursor_destroy() asserts its signal lists are empty, so
	 * skipping this turns every clean shutdown into an abort. Each block is
	 * guarded because initialisation may have failed partway through, leaving
	 * the corresponding listeners never added — and wl_list_remove() on a
	 * zeroed link dereferences NULL. */
	if (server->xdg_shell != NULL) {
		wl_list_remove(&server->new_xdg_toplevel.link);
		wl_list_remove(&server->new_xdg_popup.link);
	}
	if (server->backend != NULL) {
		wl_list_remove(&server->new_output.link);
		wl_list_remove(&server->new_input.link);
	}
	if (server->layer_shell != NULL) {
		wl_list_remove(&server->new_layer_surface.link);
	}
	if (server->pointer_constraints != NULL) {
		wl_list_remove(&server->new_constraint.link);
	}
	if (server->session_lock_manager != NULL) {
		wl_list_remove(&server->new_session_lock.link);
	}
	if (server->activation != NULL) {
		wl_list_remove(&server->request_activate.link);
	}
	if (server->cursor_shape != NULL) {
		wl_list_remove(&server->request_set_shape.link);
	}
	if (server->output_manager != NULL) {
		wl_list_remove(&server->output_manager_apply.link);
		wl_list_remove(&server->output_manager_test.link);
	}
	if (server->xwayland != NULL) {
		wl_list_remove(&server->new_xwayland_surface.link);
		wl_list_remove(&server->xwayland_ready.link);
		wlr_xwayland_destroy(server->xwayland);
		server->xwayland = NULL;
	}
	if (server->session != NULL) {
		wl_list_remove(&server->session_active.link);
	}
	if (server->new_virtual_keyboard.notify != NULL) {
		/* wlr_virtual_keyboard_manager asserts its signal list is empty when
		 * the display is destroyed, exactly like wlr_cursor does. */
		wl_list_remove(&server->new_virtual_keyboard.link);
	}
	if (server->new_decoration.notify != NULL) {
		wl_list_remove(&server->new_decoration.link);
	}
	/* Seat signals, not cursor ones. They were removed under the cursor's guard
	 * before, which both mixed up the ownership and missed three of them —
	 * wlr_seat_destroy() asserts its listener lists are empty, so every clean
	 * shutdown aborted at the first one it reached. */
	if (server->seat != NULL) {
		wl_list_remove(&server->request_cursor.link);
		wl_list_remove(&server->request_set_selection.link);
		wl_list_remove(&server->request_set_primary_selection.link);
		wl_list_remove(&server->request_start_drag.link);
		wl_list_remove(&server->start_drag.link);
	}
	if (server->cursor != NULL) {
		wl_list_remove(&server->cursor_motion.link);
		wl_list_remove(&server->cursor_motion_absolute.link);
		wl_list_remove(&server->cursor_button.link);
		wl_list_remove(&server->cursor_axis.link);
		wl_list_remove(&server->cursor_frame.link);
		wl_list_remove(&server->touch_down.link);
		wl_list_remove(&server->touch_up.link);
		wl_list_remove(&server->touch_motion.link);
		wl_list_remove(&server->touch_frame.link);
		wl_list_remove(&server->touch_cancel.link);

		wlr_cursor_destroy(server->cursor);
		server->cursor = NULL;
	}
	if (server->xcursor_mgr != NULL) {
		wlr_xcursor_manager_destroy(server->xcursor_mgr);
	}
	if (server->allocator != NULL) {
		wlr_allocator_destroy(server->allocator);
	}
	if (server->renderer != NULL) {
		wl_list_remove(&server->renderer_lost.link);
		wlr_renderer_destroy(server->renderer);
	}
	if (server->backend != NULL) {
		wlr_backend_destroy(server->backend);
	}

	free(server->active_output);
	free(server->mode);
	viewport_bindings_finish(server);

	wl_display_destroy(server->wl_display);
	server->wl_display = NULL;
}

struct viewport_toplevel *viewport_server_find_toplevel(
	struct viewport_server *server, uint32_t id)
{
	struct viewport_toplevel *toplevel;
	wl_list_for_each(toplevel, &server->toplevels, link) {
		if (toplevel->id == id) {
			return toplevel;
		}
	}
	return NULL;
}
