/* SPDX-License-Identifier: MIT */
#define _POSIX_C_SOURCE 200809L

#include <getopt.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>
#include <unistd.h>

#include <glib-unix.h>

#include <wlr/util/log.h>

#include "viewport.h"

static const char usage[] =
	"usage: viewport [options]\n"
	"\n"
	"  -u, --url URL          shell UI endpoint (default: the bundled shell)\n"
	"  -f, --fallback URL     used when --url fails (default: bundled fallback.html)\n"
	"  -t, --timeout MS       first-paint deadline before falling back (default: 5000)\n"
	"  -s, --socket PATH      JSON control socket (default: $XDG_RUNTIME_DIR/viewport-<display>.sock)\n"
	"  -c, --config PATH      config file (default: ~/.config/viewport/config.json)\n"
	"  -T, --terminal CMD     command bound to Mod4+Return\n"
	"  -M, --menu CMD         command bound to Mod4+d\n"
	"  -b, --bind CHORD=ACT   add a keybinding; repeatable\n"
	"                         e.g. --bind 'Mod4+Shift+e=exit'\n"
	"                         actions: exec CMD | close | exit | reload\n"
	"  -e, --startup CMD      command to run once the compositor is up\n"
	"  -H, --headless         use the headless backend instead of DRM\n"
	"  -d, --debug            mirror the shell console, serve it uncached\n"
	"      --trace            per-frame placement logging (very noisy)\n"
	"  -h, --help             this message\n";

static gboolean handle_signal(gpointer data)
{
	struct viewport_server *server = data;
	wlr_log(WLR_INFO, "signal received; shutting down");
	viewport_server_terminate(server);
	return G_SOURCE_CONTINUE;
}

int main(int argc, char *argv[])
{
	struct viewport_config config = {
		/* The bundled shell, so an installed copy works with no arguments and
		 * no server running. Anyone developing a shell passes --url and points
		 * it at their own; that is the unusual case, and it was the default for
		 * as long as this was only ever run from a checkout. */
		.url = "file://" VIEWPORT_PKGDATADIR "/shell/index.html",
		.fallback_url = "file://" VIEWPORT_PKGDATADIR "/fallback.html",
		.load_timeout_ms = 5000,
		.startup_cmd = NULL,
		.ipc_path = NULL,
		.headless = false,
		/* Match sway: ask clients to let the compositor own the frame. The
		 * shell already draws one, so a client titlebar is a duplicate. */
		.server_decorations = true,
		/* Dark by default: the shell is dark, and a light Firefox next to it
		 * looks like a bug. Override with "dark_mode": false in the config. */
		.dark_mode = true,
		/* The empty desktop explains itself until told not to. */
		.logo = true,
		.tutorial = true,
	};
	enum { OPT_TRACE = 1000 };
	enum wlr_log_importance log_level = WLR_INFO;

	static const struct option options[] = {
		{ "url", required_argument, NULL, 'u' },
		{ "fallback", required_argument, NULL, 'f' },
		{ "timeout", required_argument, NULL, 't' },
		{ "socket", required_argument, NULL, 's' },
		{ "config", required_argument, NULL, 'c' },
		{ "terminal", required_argument, NULL, 'T' },
		{ "menu", required_argument, NULL, 'M' },
		{ "bind", required_argument, NULL, 'b' },
		{ "startup", required_argument, NULL, 'e' },
		{ "headless", no_argument, NULL, 'H' },
		{ "debug", no_argument, NULL, 'd' },
		/* Long-only: every sensible short letter is taken, and this is not a
		 * flag anyone wants to reach for by accident. */
		{ "trace", no_argument, NULL, OPT_TRACE },
		{ "help", no_argument, NULL, 'h' },
		{ 0 },
	};

	int c;
	/* Binds are collected rather than applied, because they need a live
	 * server to attach to and that does not exist until after init. */
	const char *config_path = NULL;
	const char *bind_specs[64];
	size_t bind_count = 0;

	/* Which settings came from the command line.
	 *
	 * Needed because a flag and the config file write to the same fields, and
	 * the file is read after the flags are parsed. Comparing the value against
	 * the built-in default cannot tell the two apart — it did once, against a
	 * URL that has since changed, which silently made "url" in the config file
	 * do nothing at all for every user who set it. */
	/* --socket is absent: the control socket is opened inside
	 * viewport_server_init(), before the config file is read, so it is already
	 * whatever the flag said and there is nothing to re-apply. */
	struct {
		bool url, fallback, timeout, terminal, menu;
	} from_flag = { 0 };

	while ((c = getopt_long(argc, argv, "u:f:t:s:e:c:T:M:b:Hdh", options,
			NULL)) != -1) {
		switch (c) {
		case 'u':
			config.url = optarg;
			from_flag.url = true;
			break;
		case 'f':
			config.fallback_url = optarg;
			from_flag.fallback = true;
			break;
		case 't':
			config.load_timeout_ms = (unsigned)strtoul(optarg, NULL, 10);
			from_flag.timeout = true;
			break;
		case 's':
			config.ipc_path = optarg;
			break;
		case 'c':
			config_path = optarg;
			break;
		case 'T':
			config.terminal = optarg;
			from_flag.terminal = true;
			break;
		case 'M':
			config.menu = optarg;
			from_flag.menu = true;
			break;
		case 'b':
			if (bind_count < sizeof(bind_specs) / sizeof(bind_specs[0])) {
				bind_specs[bind_count++] = optarg;
			}
			break;
		case 'e':
			config.startup_cmd = optarg;
			break;
		case 'H':
			config.headless = true;
			break;
		case 'd':
			log_level = WLR_DEBUG;
			config.debug = true;
			break;
		case OPT_TRACE:
			/* Implies --debug: tracing without the shell's console is a much
			 * less useful half of the picture. */
			log_level = WLR_DEBUG;
			config.debug = true;
			config.trace = true;
			break;
		case 'h':
			fputs(usage, stdout);
			return EXIT_SUCCESS;
		default:
			fputs(usage, stderr);
			return EXIT_FAILURE;
		}
	}

	if (optind < argc) {
		fputs(usage, stderr);
		return EXIT_FAILURE;
	}

	wlr_log_init(log_level, NULL);

	/* The Vulkan renderer is the point of the exercise: it gives us explicit
	 * sync (zwp_linux_drm_syncobj_v1) against Vulkan clients without the
	 * implicit-fence round trips the GLES path needs. Honour an explicit
	 * WLR_RENDERER from the environment so it stays debuggable. */
	/* Portals route requests to the backend matching the current desktop, so
	 * ours has to be named before any client starts.
	 *
	 * Two names, colon-separated, which is how this variable is read.
	 * "viewport" is what selects our own Settings backend. "wlroots" is what
	 * xdg-desktop-portal-wlr lists in UseIn — along with sway, river and the
	 * rest, but naturally not a compositor that did not exist when it was
	 * written. Without it the portal finds no ScreenCast implementation at
	 * all, and screen sharing fails everywhere it is offered: Firefox and
	 * Chromium hand back an empty source list, OBS shows no capture, and
	 * nothing logs anything that points at a desktop name. The protocols were
	 * never the missing part — this string was. */
	setenv("XDG_CURRENT_DESKTOP", "viewport:wlroots", 0);

	if (getenv("WLR_RENDERER") == NULL) {
		setenv("WLR_RENDERER", "vulkan", 1);
	}

	if (config.headless && getenv("WLR_BACKENDS") == NULL) {
		setenv("WLR_BACKENDS", "headless", 1);
	}

	struct viewport_server server = { 0 };
	int status = EXIT_FAILURE;
	char *default_config_path = NULL;

	if (!viewport_server_init(&server, &config)) {
		wlr_log(WLR_ERROR, "compositor initialisation failed");
		goto out;
	}

	/* Config file first, then command-line binds on top of it, then the
	 * built-in defaults only if nothing has defined a keymap. Reversing that
	 * order would let a stale config silently shadow an explicit flag. */
	server.config_path = config_path;
	if (config_path != NULL) {
		viewport_config_load(&server, &server.config, config_path, true);
	} else {
		default_config_path = viewport_config_default_path();
		if (default_config_path != NULL) {
			viewport_config_load(&server, &server.config,
				default_config_path, false);
		}
	}

	/* Re-apply flags: the config file must not override an explicit flag.
	 * Every flag that shares a field with a config key belongs here, or the
	 * documented precedence only holds for whichever ones were remembered. */
	if (from_flag.url) {
		server.config.url = config.url;
	}
	if (from_flag.fallback) {
		server.config.fallback_url = config.fallback_url;
	}
	if (from_flag.timeout) {
		server.config.load_timeout_ms = config.load_timeout_ms;
	}
	if (from_flag.terminal) {
		server.config.terminal = config.terminal;
	}
	if (from_flag.menu) {
		server.config.menu = config.menu;
	}

	/* Command-line binds are additive, as --help says they are. They are added
	 * before the defaults and bindings match front-to-back, so a --bind for a
	 * chord that also has a default shadows it without the defaults having to
	 * be suppressed. Suppressing them was the old behaviour, and it meant one
	 * --bind silently took away Mod4+Return, exit, focus and the media keys. */
	for (size_t i = 0; i < bind_count; i++) {
		viewport_binding_add(&server, bind_specs[i]);
	}

	/* Only an explicit "binds" object in the config file replaces the defaults
	 * wholesale — that is what makes an empty one mean "no keymap at all". */
	if (!server.config.binds_from_config) {
		viewport_bindings_add_defaults(&server, server.config.terminal,
			server.config.menu);
	}

	/* The backend starts before the web engine, not after.
	 *
	 * Starting it is what brings the outputs into existence, and WebKit asks
	 * for its buffer formats once, while it is being created. Asked before any
	 * output exists, the compositor has nothing to say about what the display
	 * hardware can scan out, so the shell was offered rendering formats only —
	 * and then allocated a buffer that has to be composited rather than
	 * flipped, for the life of the session. The log line in wpe_display.c is
	 * what made that visible; the ordering is what caused it.
	 *
	 * It also means the shell is sized to the real layout at once, rather than
	 * to the 1920x1080 placeholder and then resized when the first output
	 * arrives. */
	if (!viewport_server_start(&server)) {
		wlr_log(WLR_ERROR, "backend start failed");
		goto out;
	}

	server.web = viewport_web_create(&server);
	if (server.web == NULL) {
		wlr_log(WLR_ERROR, "web engine initialisation failed");
		goto out;
	}

	/* The URL actually in force, not the one the flag carried: the config file
	 * may have supplied it, and a log line naming the wrong shell is worse
	 * than none when the desktop comes up empty. */
	wlr_log(WLR_INFO, "WAYLAND_DISPLAY=%s  shell=%s",
		server.socket_name, server.config.url);

	/* Through the same spawner as every keybinding, rather than a bare fork.
	 * A single fork leaves a zombie for the life of the session — the
	 * compositor never reaps — and makes the startup command a child of the
	 * compositor, so it also shares its session and its controlling terminal.
	 * viewport_spawn() double-forks and setsid()s for exactly those reasons. */
	if (server.config.startup_cmd != NULL) {
		viewport_spawn(server.config.startup_cmd);
	}

	/* Without a handler SIGTERM kills the process where it stands: the web
	 * process is orphaned, and on a TTY the DRM master and session are dropped
	 * by the kernel rather than released, which is how a compositor leaves a
	 * black screen behind. Ending the loop instead runs the same teardown as
	 * Mod4+Shift+e. These are GLib signal sources because GLib owns the outer
	 * loop, so the handler runs between iterations rather than in async-signal
	 * context. */
	guint sigterm = g_unix_signal_add(SIGTERM, handle_signal, &server);
	guint sigint = g_unix_signal_add(SIGINT, handle_signal, &server);
	signal(SIGCHLD, SIG_IGN);

	viewport_server_run(&server);

	g_source_remove(sigterm);
	g_source_remove(sigint);
	status = EXIT_SUCCESS;

out:
	viewport_server_finish(&server);
	g_free(server.config.outputs);
	viewport_config_finish();
	free(default_config_path);
	return status;
}
