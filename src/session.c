/* SPDX-License-Identifier: MIT
 *
 * Remembering the layout across a restart.
 *
 * Restarting the compositor kills every client with it — they are its clients,
 * and nothing survives that. So this is not session *resumption* in the sense
 * of processes being preserved; it is the layout being preserved. The shell
 * writes down where things were, and as the same applications come back they
 * are put where they were rather than piling up in the order they happen to
 * start.
 *
 * The state is the shell's, in the shell's own format — this file neither
 * parses nor understands it. That keeps the layout model where it belongs: the
 * compositor has no opinion about workspaces or columns and should not gain one
 * just to store them. All it does is hand the blob back on request.
 *
 * Written to $XDG_STATE_HOME rather than the config directory: it is state the
 * program maintains, not something a user edits, and the two should not be
 * confused. Written through a temporary file and renamed, so a compositor that
 * dies mid-write leaves the previous layout intact rather than half of a new
 * one — which would be worse than none, since a truncated file parses as a
 * broken layout rather than as an absent one.
 */
#define _POSIX_C_SOURCE 200809L

#include <stdlib.h>

#include <glib.h>

#include <wlr/util/log.h>

#include "viewport-shell.h"

static char *session_path(void)
{
	const char *state_dir = getenv("XDG_STATE_HOME");
	char *dir = state_dir != NULL && state_dir[0] != '\0'
		? g_strdup_printf("%s/viewport", state_dir)
		: NULL;

	if (dir == NULL) {
		const char *home = getenv("HOME");
		if (home == NULL) {
			return NULL;
		}
		dir = g_strdup_printf("%s/.local/state/viewport", home);
	}

	if (g_mkdir_with_parents(dir, 0700) != 0) {
		wlr_log(WLR_ERROR, "cannot create %s; layout will not be remembered",
			dir);
		g_free(dir);
		return NULL;
	}

	char *path = g_strdup_printf("%s/session.json", dir);
	g_free(dir);
	return path;
}

void viewport_session_save(struct viewport_server *server, const char *state)
{
	char *path = session_path();
	if (path == NULL) {
		return;
	}

	GError *error = NULL;
	/* g_file_set_contents writes to a temporary and renames, which is exactly
	 * the atomicity wanted here. */
	if (!g_file_set_contents(path, state, -1, &error)) {
		wlr_log(WLR_ERROR, "saving layout to %s: %s", path,
			error ? error->message : "failed");
		g_clear_error(&error);
	}

	g_free(path);
}

/* Returns the stored blob, or NULL when there is none. Caller frees. */
char *viewport_session_load(struct viewport_server *server)
{
	char *path = session_path();
	if (path == NULL) {
		return NULL;
	}

	char *contents = NULL;
	GError *error = NULL;
	if (!g_file_get_contents(path, &contents, NULL, &error)) {
		/* A missing file is the normal first run, not a problem. */
		wlr_log(WLR_DEBUG, "no saved layout at %s", path);
		g_clear_error(&error);
		g_free(path);
		return NULL;
	}

	wlr_log(WLR_INFO, "restoring layout from %s", path);
	g_free(path);
	return contents;
}
