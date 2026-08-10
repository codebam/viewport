/* SPDX-License-Identifier: MIT
 *
 * List the workspaces and ask to switch, the way a taskbar would.
 *
 * ext-workspace-v1 is the staging protocol bars use to show the workspaces
 * of a given output and switch between them. Here the workspaces belong to
 * the shell, so the compositor is a relay: the shell's `workspace.list` comes
 * in over the control socket and is republished as workspace_group and
 * workspace handles; a client's requests are forwarded back out as a
 * `workspace.request` event for the shell to act on.
 *
 *   workspace-client   bind, observe, activate + commit; pass
 *
 * This client:
 *
 *   - binds ext_workspace_manager_v1 v1 and waits for a workspace_group and
 *     a workspace handle — the shell's list, republished;
 *   - calls activate on the workspace handle and commit on the manager. The
 *     script that runs it is listening on the control socket for the
 *     `workspace.request` this must produce, which is the observable sign
 *     that the forwarding path works.
 *
 * Exits 0 on success, 2 if the compositor does not offer the global, 1 if
 * the workspaces were not published.
 */
#define _GNU_SOURCE

#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <poll.h>
#include <time.h>
#include <unistd.h>

#include <wayland-client.h>

#include "ext-workspace-v1-client-protocol.h"

struct state {
	struct ext_workspace_manager_v1 *manager;
	struct ext_workspace_group_handle_v1 *group;
	struct ext_workspace_handle_v1 *handle;
	bool group_seen;
	bool handle_seen;
	bool handled_done;
};

static void handle_group(void *data,
	struct ext_workspace_manager_v1 *manager,
	struct ext_workspace_group_handle_v1 *group)
{
	struct state *state = data;
	(void)manager;
	if (!state->group_seen) {
		state->group_seen = true;
		state->group = group;
	}
}

static void handle_workspace(void *data,
	struct ext_workspace_manager_v1 *manager,
	struct ext_workspace_handle_v1 *workspace)
{
	struct state *state = data;
	(void)manager;
	if (!state->handle_seen) {
		state->handle_seen = true;
		state->handle = workspace;
	}
}

static void handle_manager_done(void *data,
	struct ext_workspace_manager_v1 *manager)
{
	struct state *state = data;
	(void)manager;
	state->handled_done = true;
}

static void handle_manager_finished(void *data,
	struct ext_workspace_manager_v1 *manager)
{
	(void)data;
	(void)manager;
}

static const struct ext_workspace_manager_v1_listener manager_listener = {
	.workspace_group = handle_group,
	.workspace = handle_workspace,
	.done = handle_manager_done,
	.finished = handle_manager_finished,
};

static void handle_global(void *data, struct wl_registry *registry,
	uint32_t name, const char *interface, uint32_t version)
{
	struct state *state = data;

	if (strcmp(interface, ext_workspace_manager_v1_interface.name) == 0) {
		state->manager = wl_registry_bind(registry, name,
			&ext_workspace_manager_v1_interface, 1);
		ext_workspace_manager_v1_add_listener(state->manager,
			&manager_listener, state);
	}
}

static void handle_global_remove(void *data, struct wl_registry *registry,
	uint32_t name)
{
	(void)data;
	(void)registry;
	(void)name;
}

static const struct wl_registry_listener registry_listener = {
	.global = handle_global,
	.global_remove = handle_global_remove,
};

/* Dispatch with a deadline, rather than blocking on a quiet socket: nothing
 * more arrives once the interesting events have been read. Returns 0 if the
 * deadline was reached without error, -1 on a connection error. */
static int dispatch_until(struct wl_display *display, int timeout_ms)
{
	struct timespec start;
	clock_gettime(CLOCK_MONOTONIC, &start);

	/* Events may already be queued from the last read. */
	if (wl_display_dispatch_pending(display) < 0) {
		return -1;
	}
	if (wl_display_flush(display) < 0) {
		return -1;
	}

	while (true) {
		struct timespec now;
		clock_gettime(CLOCK_MONOTONIC, &now);
		long elapsed_ms = (now.tv_sec - start.tv_sec) * 1000
			+ (now.tv_nsec - start.tv_nsec) / 1000000;
		int remain = timeout_ms - (int)elapsed_ms;
		if (remain <= 0) {
			return 0;
		}

		if (wl_display_prepare_read(display) != 0) {
			if (wl_display_dispatch_pending(display) < 0) {
				return -1;
			}
			continue;
		}
		struct pollfd fds[1];
		fds[0].fd = wl_display_get_fd(display);
		fds[0].events = POLLIN;
		fds[0].revents = 0;

		int r = poll(fds, 1, remain);
		if (r < 0) {
			if (errno == EINTR) {
				wl_display_cancel_read(display);
				continue;
			}
			wl_display_cancel_read(display);
			return -1;
		}
		if (r == 0) {
			/* Deadline in this poll — the caller checks its flags. */
			wl_display_cancel_read(display);
			return 0;
		}
		if (wl_display_read_events(display) < 0) {
			return -1;
		}
		if (wl_display_dispatch_pending(display) < 0) {
			return -1;
		}
	}
}

int main(void)
{
	struct wl_display *display = wl_display_connect(NULL);
	if (display == NULL) {
		fprintf(stderr, "cannot connect to WAYLAND_DISPLAY\n");
		return 2;
	}

	struct state state = { 0 };
	struct wl_registry *registry = wl_display_get_registry(display);
	wl_registry_add_listener(registry, &registry_listener, &state);
	wl_display_roundtrip(display);

	if (state.manager == NULL) {
		fprintf(stderr, "compositor does not offer ext_workspace_manager_v1\n");
		wl_display_disconnect(display);
		return 2;
	}

	/* The script has already sent the shell's workspace.list over the
	 * control socket, so the group and the workspace must be published. */
	if (dispatch_until(display, 5000) < 0) {
		fprintf(stderr, "no workspace was published\n");
		wl_display_disconnect(display);
		return 1;
	}
	if (!state.group_seen) {
		fprintf(stderr, "no workspace_group was published\n");
		wl_display_disconnect(display);
		return 1;
	}
	if (!state.handle_seen) {
		fprintf(stderr, "no workspace handle was published\n");
		wl_display_disconnect(display);
		return 1;
	}
	printf("ok   the compositor published a workspace group and workspace\n");

	/* Ask to switch, and commit — the batch boundary. The script hears the
	 * resulting workspace.request on the control socket. */
	ext_workspace_handle_v1_activate(state.handle);
	ext_workspace_manager_v1_commit(state.manager);
	wl_display_flush(display);

	printf("ok   activate + commit sent\n");
	wl_display_disconnect(display);
	return 0;
}