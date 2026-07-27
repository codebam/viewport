/* SPDX-License-Identifier: MIT
 *
 * Driving viewport_ipc_handle() with hostile input.
 *
 * This is the compositor's only untrusted entry point. Everything else it
 * parses is a config file the user wrote or a Wayland request from a client
 * the kernel vouched for; IPC messages arrive from whatever page happens to be
 * served at config.url, in a length-delimited buffer that the parser is handed
 * verbatim. Two crashes have already been fixed in there, both type confusions
 * that a hand-written test would only have found by guessing the same field
 * the fuzzer reaches in a second.
 *
 * The file has two front-ends over one entry point:
 *
 *   LLVMFuzzerTestOneInput   under -DVIEWPORT_FUZZ, i.e. `-Dfuzz=true`, which
 *                            needs a clang that can do -fsanitize=fuzzer.
 *   main()                   otherwise. Replays the built-in corpus below, or
 *                            the files named on the command line.
 *
 * The second exists because the first cannot be built here: a gcc-only machine
 * — which includes this project's own CI runner — could compile neither, so
 * the harness would ship as source nobody had ever executed. The replay build
 * needs no special compiler, is registered as an ordinary meson test, and runs
 * in milliseconds, so the stub below is known to survive every message type
 * rather than assumed to. When a fuzzer does find something, its reproducer
 * file drops straight into the corpus directory and the same binary replays it.
 *
 * WHAT THE STUB IS. A server with a real wl_display and a real wlr_seat, and
 * nothing else. web, ipc and notifications stay NULL so every reply the
 * handler tries to send is dropped at the first NULL check instead of reaching
 * a web view or a socket — what is under test is the parse and the dispatch.
 *
 * The display and seat are not optional, which is worth recording because the
 * obvious all-zero stub is wrong in a way that only shows up once the fuzzer
 * is already running: {"type":"shell.focus"} reaches viewport_focus_web(),
 * which calls wlr_seat_keyboard_notify_clear_focus(server->seat), and a NULL
 * seat faults there. Half the crashes reported would have been the harness's.
 *
 * The wl_lists are initialised for the same class of reason: an empty wl_list
 * is a self-pointing one, and a handler that walks a zeroed list dereferences
 * NULL before it has read a byte of input.
 */
#define _POSIX_C_SOURCE 200809L

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <glib.h>

#include <wlr/types/wlr_seat.h>

#include "viewport.h"

static struct viewport_server stub_server;
static struct wl_display *stub_display;
static struct wlr_seat *stub_seat;

/* Built once and reused, unlike the rest of the stub.
 *
 * wl_display_create() installs process-wide state and wlr_seat_create() adds a
 * global to it, so doing this per input would leak a display per iteration and
 * dominate the fuzzer's time. Neither object accumulates anything from an IPC
 * message — no client is ever connected, so the seat has no focus to hold —
 * which is what makes them safe to share where the server struct is not. */
static bool stub_globals_init(void)
{
	if (stub_display != NULL) {
		return true;
	}

	stub_display = wl_display_create();
	if (stub_display == NULL) {
		return false;
	}

	stub_seat = wlr_seat_create(stub_display, "seat0");
	return stub_seat != NULL;
}

static void stub_server_init(void)
{
	memset(&stub_server, 0, sizeof(stub_server));

	wl_list_init(&stub_server.outputs);
	wl_list_init(&stub_server.toplevels);
	wl_list_init(&stub_server.keyboards);
	wl_list_init(&stub_server.bindings);

	stub_server.wl_display = stub_display;
	stub_server.seat = stub_seat;
}

/* One message, against a server rebuilt for it.
 *
 * Rebuilt rather than carried over: some messages leave state behind — a
 * binding armed, a delta pending, an active output named — and a crash that
 * only happened because of what the previous input did would not reproduce
 * from the single file the fuzzer reports, which is the one thing a crash
 * report has to be able to do. */
static void handle_one(const uint8_t *data, size_t size)
{
	stub_server_init();
	viewport_ipc_handle(&stub_server, (const char *)data, size, NULL);

	/* Bindings are the only allocation a message can leave on the stub that
	 * outlives it: bind.add parses a chord into the list. Dropping them keeps
	 * a long fuzzing run from growing without bound and keeps the leak
	 * checker's reports about the compositor rather than about the harness. */
	viewport_bindings_finish(&stub_server);

	free(stub_server.active_output);
	free(stub_server.mode);
}

#ifdef VIEWPORT_FUZZ

int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size)
{
	if (!stub_globals_init()) {
		return 0;
	}
	handle_one(data, size);
	return 0;
}

#else

/* Every shape that has ever broken this parser, plus one of each kind it is
 * meant to reject.
 *
 * These are not a substitute for fuzzing — they are the regression floor under
 * it, so the cases already paid for stay fixed without waiting for a fuzzer to
 * rediscover them. The first two are the two real crashes. */
static const char *const corpus[] = {
	/* A non-string "type". has_member() proved presence, the getter returned
	 * NULL, and the strcmp() that followed dereferenced it. */
	"{\"type\":5}",
	"{\"type\":null}",
	"{\"type\":{}}",
	"{\"type\":true}",
	"{\"type\":[]}",
	/* A non-string transform, which died the same way one layer down. */
	"{\"type\":\"output.configure\",\"name\":\"eDP-1\",\"transform\":null}",
	"{\"type\":\"output.configure\",\"name\":\"eDP-1\",\"transform\":7}",

	/* Not an object, or not JSON at all. */
	"",
	" ",
	"[]",
	"null",
	"true",
	"\"a string\"",
	"{",
	"{\"type\":",
	"not json at all",
	"\xff\xfe\xfd",

	/* Absent, wrong-typed and hostile members on each message that reads one.
	 * Every one of these must take the documented fallback rather than a
	 * json-glib default. */
	"{\"type\":\"view.layout\"}",
	"{\"type\":\"view.layout\",\"id\":{}}",
	"{\"type\":\"view.layout\",\"id\":1,\"width\":-1,\"height\":-1}",
	"{\"type\":\"view.layout\",\"id\":1,\"clip\":5}",
	"{\"type\":\"view.layout\",\"id\":1,\"scale\":\"big\"}",
	"{\"type\":\"view.layout\",\"id\":1,\"scale\":1}",
	"{\"type\":\"view.visible\",\"id\":1,\"visible\":\"yes\"}",
	"{\"type\":\"view.opacity\",\"id\":1,\"opacity\":[]}",
	"{\"type\":\"view.fullscreen\",\"id\":1,\"fullscreen\":3}",
	"{\"type\":\"view.focus\"}",
	"{\"type\":\"view.close\",\"id\":4294967295}",
	"{\"type\":\"view.query\"}",
	"{\"type\":\"shell.focus\"}",
	"{\"type\":\"shell.overview\",\"active\":true}",
	"{\"type\":\"shell.overview\",\"active\":\"yes\"}",
	"{\"type\":\"session.save\",\"state\":5}",
	"{\"type\":\"session.query\"}",
	"{\"type\":\"notification.action\",\"id\":1,\"action\":9}",
	"{\"type\":\"notification.dismiss\",\"id\":{}}",
	"{\"type\":\"notification.expire\",\"id\":1}",
	"{\"type\":\"output.configure\"}",
	"{\"type\":\"output.configure\",\"name\":5}",
	"{\"type\":\"output.configure\",\"name\":\"x\",\"mode\":5}",
	"{\"type\":\"output.configure\",\"name\":\"x\",\"scale\":-1}",
	"{\"type\":\"output.hdr\"}",
	"{\"type\":\"output.hdr\",\"name\":[]}",
	"{\"type\":\"output.confirm\"}",
	"{\"type\":\"output.active\",\"name\":5}",
	"{\"type\":\"output.query\"}",
	"{\"type\":\"output.test_add\"}",
	"{\"type\":\"output.test_remove\",\"name\":5}",
	"{\"type\":\"bind.add\"}",
	"{\"type\":\"bind.add\",\"chord\":5,\"action\":\"close\"}",
	"{\"type\":\"bind.add\",\"chord\":\"Mod4+z\",\"action\":\"exec true\"}",
	"{\"type\":\"bind.add\",\"chord\":\"nonsense\",\"action\":\"close\"}",
	"{\"type\":\"an unknown message type\"}",

	/* Deep nesting and a long string, which are the parser's own limits
	 * rather than the dispatcher's. */
	"{\"type\":\"view.layout\",\"clip\":{\"x\":{\"y\":{\"z\":{\"w\":1}}}}}",
};

/* NUL bytes mid-buffer, which the socket transport can deliver and which the
 * table above cannot express. Kept separate so its length is explicit. */
static const char embedded_nul[] = "{\"type\":\"view.\0layout\",\"id\":1}";

static int replay_file(const char *path)
{
	char *contents = NULL;
	gsize length = 0;
	GError *error = NULL;

	if (!g_file_get_contents(path, &contents, &length, &error)) {
		fprintf(stderr, "not ok  %s: %s\n", path,
			error != NULL ? error->message : "unreadable");
		g_clear_error(&error);
		return 1;
	}

	handle_one((const uint8_t *)contents, length);
	printf("ok    %s (%zu bytes)\n", path, (size_t)length);

	g_free(contents);
	return 0;
}

/* Survival is the whole assertion.
 *
 * There is nothing to compare against: a malformed message is supposed to be
 * ignored, and "ignored" has no observable result on a stub with no transport
 * attached. What this proves is that none of these faults, which is exactly
 * what the two fixed crashes did. Any failure arrives as a signal, so the
 * meson test's exit-code protocol reports it without this file's help. */
int main(int argc, char *argv[])
{
	if (!stub_globals_init()) {
		fprintf(stderr, "not ok  could not create a display and seat\n");
		return 1;
	}

	if (argc > 1) {
		int failures = 0;
		for (int i = 1; i < argc; i++) {
			failures += replay_file(argv[i]);
		}
		printf("%s   %d failure(s)\n", failures == 0 ? "ok  " : "not ok",
			failures);
		return failures == 0 ? 0 : 1;
	}

	for (size_t i = 0; i < sizeof(corpus) / sizeof(corpus[0]); i++) {
		handle_one((const uint8_t *)corpus[i], strlen(corpus[i]));
		printf("ok    %s\n", corpus[i]);
	}

	handle_one((const uint8_t *)embedded_nul, sizeof(embedded_nul) - 1);
	printf("ok    a NUL byte inside the document\n");

	printf("ok    %zu message(s), 0 failure(s)\n",
		sizeof(corpus) / sizeof(corpus[0]) + 1);
	return 0;
}

#endif
