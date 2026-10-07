/* SPDX-License-Identifier: MIT
 *
 * Drive wp-color-management-v1 the way a game under Proton does.
 *
 * Wine is the reason this protocol is spoken at all on a compositor's side of
 * a Desktop Linux session: a D3D swapchain's colour space is handed to the
 * compositor as an image description, and Wine decides whether to offer a game
 * an HDR mode from what the compositor says about its outputs. Two of those
 * descriptions are pre-defined by the protocol and named after Windows —
 * `create_windows_scrgb`, and `create_windows_bt2100` from version 3 — and
 * Wine reads the *features* advertising them as the answer to "can this
 * display do HDR at all".
 *
 * The modes:
 *
 *   advertise     the manager is bound at the advertised version, and the
 *                 advertisements a client picks from arrive before `done`:
 *                 both Windows features, ST 2084 PQ among the named transfer
 *                 functions, BT.2020 among the named primaries, and the sRGB
 *                 curve under the name that version has for it — the
 *                 deprecated pair below 2, `compound_power_2_4` from 2;
 *   windows-scrgb `create_windows_scrgb` delivers ready2 with a non-zero
 *                 identity, which is what Wine waits for;
 *   windows-bt2100 the same for `create_windows_bt2100`, the description an
 *                 HDR10 swapchain is attached with;
 *   parametric    the parametric creator still works at the new version —
 *                 BT.2020 with PQ, which is the path every other client uses;
 *   no-information `get_information` on a well-known description is the
 *                 protocol error `no_information`: those descriptions are
 *                 defined, not measured, and the mode passes when the
 *                 connection dies rather than when it is answered.
 *
 * Exits 0 when the compositor did what the mode asks, 2 if the global is
 * missing, 1 if it did not.
 */
#define _GNU_SOURCE

#include <errno.h>
#include <poll.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <wayland-client.h>
#include "color-management-v1-client-protocol.h"

struct state {
	struct wl_display *display;
	struct wl_registry *registry;
	struct wp_color_manager_v1 *manager;
	uint32_t manager_version;

	bool got_manager;
	bool done;
	uint32_t seen_features;
	bool seen_pq;
	bool seen_bt2020;
	/* The sRGB curve, by the name that exists at the version bound: the
	 * deprecated pair below version 2, `compound_power_2_4` from it. */
	bool seen_compound_power;
	bool seen_deprecated_srgb;
	/* Set when an advertisement arrives after done, which the protocol
	 * forbids: a client stops listening for them at that point. */
	bool advertised_after_done;
};

static void handle_supported_intent(void *data,
	struct wp_color_manager_v1 *manager, uint32_t intent)
{
	(void)data;
	(void)manager;
	(void)intent;
}

static void handle_supported_feature(void *data,
	struct wp_color_manager_v1 *manager, uint32_t feature)
{
	struct state *state = data;
	(void)manager;

	if (state->done) {
		state->advertised_after_done = true;
		return;
	}
	if (feature < 32) {
		state->seen_features |= 1u << feature;
	}
}

static void handle_supported_tf_named(void *data,
	struct wp_color_manager_v1 *manager, uint32_t tf)
{
	struct state *state = data;
	(void)manager;

	if (state->done) {
		state->advertised_after_done = true;
		return;
	}
	if (tf == WP_COLOR_MANAGER_V1_TRANSFER_FUNCTION_ST2084_PQ) {
		state->seen_pq = true;
	}
	if (tf == WP_COLOR_MANAGER_V1_TRANSFER_FUNCTION_COMPOUND_POWER_2_4) {
		state->seen_compound_power = true;
	}
	if (tf == WP_COLOR_MANAGER_V1_TRANSFER_FUNCTION_SRGB ||
		tf == WP_COLOR_MANAGER_V1_TRANSFER_FUNCTION_EXT_SRGB) {
		state->seen_deprecated_srgb = true;
	}
}

static void handle_supported_primaries_named(void *data,
	struct wp_color_manager_v1 *manager, uint32_t primaries)
{
	struct state *state = data;
	(void)manager;

	if (state->done) {
		state->advertised_after_done = true;
		return;
	}
	if (primaries == WP_COLOR_MANAGER_V1_PRIMARIES_BT2020) {
		state->seen_bt2020 = true;
	}
}

static void handle_manager_done(void *data,
	struct wp_color_manager_v1 *manager)
{
	struct state *state = data;
	(void)manager;

	state->done = true;
}

static const struct wp_color_manager_v1_listener manager_listener = {
	.supported_intent = handle_supported_intent,
	.supported_feature = handle_supported_feature,
	.supported_tf_named = handle_supported_tf_named,
	.supported_primaries_named = handle_supported_primaries_named,
	.done = handle_manager_done,
};

/* What the description creation request delivered, as this client saw it. */
enum description_status {
	DESCRIPTION_PENDING = 0,
	DESCRIPTION_READY,
	DESCRIPTION_READY2,
	DESCRIPTION_FAILED,
};

struct description {
	enum description_status status;
	uint32_t identity_hi;
	uint32_t identity_lo;
	uint32_t cause;
};

static void handle_description_ready(void *data,
	struct wp_image_description_v1 *description, uint32_t identity)
{
	struct description *seen = data;
	(void)description;

	/* Deprecated from version 2, and this client binds at three, so a
	 * compositor that sends it is one version behind its own global. */
	seen->status = DESCRIPTION_READY;
	seen->identity_lo = identity;
}

static void handle_description_ready2(void *data,
	struct wp_image_description_v1 *description,
	uint32_t identity_hi, uint32_t identity_lo)
{
	struct description *seen = data;
	(void)description;

	seen->status = DESCRIPTION_READY2;
	seen->identity_hi = identity_hi;
	seen->identity_lo = identity_lo;
}

static void handle_description_failed(void *data,
	struct wp_image_description_v1 *description,
	uint32_t cause, const char *message)
{
	struct description *seen = data;
	(void)description;

	seen->status = DESCRIPTION_FAILED;
	seen->cause = cause;
	fprintf(stderr, "description failed: cause %u %s\n", cause,
		message ? message : "");
}

static const struct wp_image_description_v1_listener description_listener = {
	.ready = handle_description_ready,
	.ready2 = handle_description_ready2,
	.failed = handle_description_failed,
};

static void registry_global(void *data, struct wl_registry *registry,
	uint32_t name, const char *interface, uint32_t version)
{
	struct state *state = data;

	if (strcmp(interface, wp_color_manager_v1_interface.name) == 0) {
		/* Capped at three because that is what this client is built
		 * against; the compositor's own version is what matters, and
		 * the mode that checks it reads it back. */
		uint32_t bind = version < 3 ? version : 3;
		state->manager_version = bind;
		state->manager = wl_registry_bind(registry, name,
			&wp_color_manager_v1_interface, bind);
		wp_color_manager_v1_add_listener(state->manager,
			&manager_listener, state);
		state->got_manager = true;
	}
}

static void registry_global_remove(void *data, struct wl_registry *registry,
	uint32_t name)
{
	(void)data;
	(void)registry;
	(void)name;
}

static const struct wl_registry_listener registry_listener = {
	.global = registry_global,
	.global_remove = registry_global_remove,
};

/* Round-trip until the creation request has been answered either way. */
static int await_description(struct state *state, struct description *seen)
{
	(void)seen;

	while (wl_display_dispatch(state->display) != -1) {
		/* A local read of the status the listener just wrote: the
		 * dispatch above returns once events have been read, so the
		 * listener has run by the time it does. */
		if (seen->status != DESCRIPTION_PENDING) {
			return 0;
		}
	}
	return -1;
}

/* The connection must die: a refused request is a fatal protocol error, and a
 * compositor that accepted it and stayed up is the one failing here. */
static int expect_killed(struct state *state, const char *what)
{
	wl_display_flush(state->display);
	while (wl_display_get_error(state->display) == 0) {
		struct pollfd pfd = {
			.fd = wl_display_get_fd(state->display),
			.events = POLLIN,
		};
		int ready = poll(&pfd, 1, 2000);
		if (ready <= 0) {
			fprintf(stderr,
				"%s: compositor neither answered nor killed us (errno %d)\n",
				what, errno);
			return 1;
		}
		if (wl_display_dispatch(state->display) == -1) {
			break;
		}
	}

	if (wl_display_get_error(state->display) == EPROTO) {
		return 0;
	}
	fprintf(stderr, "%s: connection ended without a protocol error\n", what);
	return 1;
}

static int check_advertisements(struct state *state)
{
	if (!state->done) {
		fputs("the manager never sent done\n", stderr);
		return 1;
	}
	if (state->advertised_after_done) {
		fputs("an advertisement arrived after done\n", stderr);
		return 1;
	}
	/* Wine binds whatever it is offered and calls the request behind each
	 * feature it is told about. A compositor that advertises neither
	 * Windows description is one Wine reports as having no HDR display at
	 * all, whatever the monitor can do. */
	if (!(state->seen_features &
			(1u << WP_COLOR_MANAGER_V1_FEATURE_WINDOWS_SCRGB))) {
		fputs("windows_scrgb was not advertised\n", stderr);
		return 1;
	}
	if (state->manager_version >= 3 &&
		!(state->seen_features &
			(1u << WP_COLOR_MANAGER_V1_FEATURE_WINDOWS_BT2100))) {
		fputs("windows_bt2100 was not advertised at version 3\n",
			stderr);
		return 1;
	}
	if (!state->seen_pq) {
		fputs("st2084_pq was not advertised\n", stderr);
		return 1;
	}
	if (!state->seen_bt2020) {
		fputs("bt2020 was not advertised\n", stderr);
		return 1;
	}
	/* Version 2 deprecated `srgb` and `ext_srgb` and named the same IEC
	 * 61966-2-1 encoding `compound_power_2_4`. The protocol says a
	 * deprecated name must not be advertised to the version that
	 * deprecated it, and a client bound there is owed the replacement. */
	if (state->manager_version >= 2) {
		if (state->seen_deprecated_srgb) {
			fputs("a deprecated transfer function was advertised "
				"to a client bound at version 2 or later\n", stderr);
			return 1;
		}
		if (!state->seen_compound_power) {
			fputs("compound_power_2_4 was not advertised at "
				"version 2 or later\n", stderr);
			return 1;
		}
	}
	return 0;
}

static int mode_advertise(struct state *state)
{
	if (wl_display_dispatch(state->display) == -1) {
		perror("dispatch");
		return 1;
	}
	if (state->manager_version < 3) {
		fprintf(stderr, "manager bound at version %u, need 3: "
			"create_windows_bt2100 does not exist below it\n",
			state->manager_version);
		return 1;
	}
	return check_advertisements(state);
}

static int mode_windows(struct state *state, bool bt2100)
{
	if (wl_display_dispatch(state->display) == -1) {
		perror("dispatch");
		return 1;
	}
	if (check_advertisements(state) != 0) {
		return 1;
	}

	struct description seen = { .status = DESCRIPTION_PENDING };
	struct wp_image_description_v1 *description = bt2100
		? wp_color_manager_v1_create_windows_bt2100(state->manager)
		: wp_color_manager_v1_create_windows_scrgb(state->manager);
	if (!description) {
		fputs("the request returned nothing: the feature was advertised "
			"but the manager's version does not carry it\n", stderr);
		return 1;
	}
	wp_image_description_v1_add_listener(description, &description_listener,
		&seen);

	if (await_description(state, &seen) != 0) {
		perror("awaiting the description");
		return 1;
	}
	if (seen.status != DESCRIPTION_READY2) {
		fprintf(stderr, "expected ready2, saw status %d\n", seen.status);
		return 1;
	}
	/* Identity 0 is reserved by the protocol: a compositor that hands it
	 * out is one whose identities cannot be compared. */
	if (seen.identity_lo == 0) {
		fputs("ready2 carried the reserved identity 0\n", stderr);
		return 1;
	}
	wp_image_description_v1_destroy(description);
	return 0;
}

static int mode_parametric(struct state *state)
{
	if (wl_display_dispatch(state->display) == -1) {
		perror("dispatch");
		return 1;
	}
	if (check_advertisements(state) != 0) {
		return 1;
	}

	struct wp_image_description_creator_params_v1 *creator =
		wp_color_manager_v1_create_parametric_creator(state->manager);
	if (!creator) {
		fputs("no parametric creator\n", stderr);
		return 1;
	}
	wp_image_description_creator_params_v1_set_primaries_named(creator,
		WP_COLOR_MANAGER_V1_PRIMARIES_BT2020);
	wp_image_description_creator_params_v1_set_tf_named(creator,
		WP_COLOR_MANAGER_V1_TRANSFER_FUNCTION_ST2084_PQ);

	struct description seen = { .status = DESCRIPTION_PENDING };
	struct wp_image_description_v1 *description =
		wp_image_description_creator_params_v1_create(creator);
	if (!description) {
		fputs("create returned nothing\n", stderr);
		return 1;
	}
	wp_image_description_v1_add_listener(description, &description_listener,
		&seen);

	if (await_description(state, &seen) != 0) {
		perror("awaiting the description");
		return 1;
	}
	if (seen.status != DESCRIPTION_READY2) {
		fprintf(stderr, "expected ready2, saw status %d\n", seen.status);
		return 1;
	}
	wp_image_description_v1_destroy(description);
	return 0;
}

static int mode_no_information(struct state *state)
{
	if (wl_display_dispatch(state->display) == -1) {
		perror("dispatch");
		return 1;
	}

	struct description seen = { .status = DESCRIPTION_PENDING };
	struct wp_image_description_v1 *description =
		wp_color_manager_v1_create_windows_scrgb(state->manager);
	if (!description) {
		fputs("no windows_scrgb description\n", stderr);
		return 1;
	}
	wp_image_description_v1_add_listener(description, &description_listener,
		&seen);
	if (await_description(state, &seen) != 0) {
		perror("awaiting the description");
		return 1;
	}
	if (seen.status != DESCRIPTION_READY2) {
		fprintf(stderr, "expected ready2, saw status %d\n", seen.status);
		return 1;
	}

	/* The protocol says these descriptions do not allow it, so the answer
	 * is `no_information` and the connection goes with it. */
	wp_image_description_v1_get_information(description);
	return expect_killed(state, "get_information on windows_scrgb");
}

int main(int argc, char **argv)
{
	if (argc != 2) {
		fprintf(stderr, "usage: %s MODE\n", argv[0]);
		return 2;
	}
	const char *mode = argv[1];

	struct state state = { 0 };
	state.display = wl_display_connect(NULL);
	if (!state.display) {
		perror("wl_display_connect");
		return 2;
	}
	state.registry = wl_display_get_registry(state.display);
	wl_registry_add_listener(state.registry, &registry_listener, &state);
	if (wl_display_roundtrip(state.display) == -1) {
		perror("roundtrip");
		return 2;
	}
	if (!state.got_manager) {
		fputs("the compositor does not advertise "
			"wp_color_manager_v1\n", stderr);
		return 2;
	}

	int status;
	if (strcmp(mode, "advertise") == 0) {
		status = mode_advertise(&state);
	} else if (strcmp(mode, "windows-scrgb") == 0) {
		status = mode_windows(&state, false);
	} else if (strcmp(mode, "windows-bt2100") == 0) {
		status = mode_windows(&state, true);
	} else if (strcmp(mode, "parametric") == 0) {
		status = mode_parametric(&state);
	} else if (strcmp(mode, "no-information") == 0) {
		status = mode_no_information(&state);
	} else {
		fprintf(stderr, "unknown mode %s\n", mode);
		status = 2;
	}

	if (wl_display_get_error(state.display) == 0) {
		wl_display_disconnect(state.display);
	}
	return status;
}
