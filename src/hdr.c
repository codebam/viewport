/* SPDX-License-Identifier: MIT
 *
 * High dynamic range.
 *
 * Two halves, and both are needed before anything looks right.
 *
 * The output half is switching the monitor into an HDR mode: BT.2020 primaries
 * and the PQ transfer function, which is what the display expects when it lights
 * its panel past the SDR range. That is one call, and on its own it makes
 * everything look worse — every SDR window is suddenly being interpreted as if
 * it were HDR, which washes it out.
 *
 * The client half is colour management: a client says what its content actually
 * is, through wp-color-management-v1, and the renderer converts it into
 * whatever the output is in. Attaching that manager to the scene is what makes
 * the compositor convert rather than reinterpret — SDR windows stay looking
 * like SDR windows on an HDR screen, and a client that has real HDR content to
 * show can say so and have it passed through.
 *
 * Not every monitor can do it and not every one that can should. So this is per
 * output and off until asked: capability is checked against what the connector
 * reports, the change is tested before it is committed, and a refusal leaves
 * the display exactly as it was rather than dark.
 */
#define _POSIX_C_SOURCE 200809L

#include <stdlib.h>
#include <string.h>

#include <drm_fourcc.h>

#include <wlr/render/color.h>
#include <wlr/types/wlr_color_management_v1.h>
#include <wlr/types/wlr_output.h>
#include <wlr/types/wlr_scene.h>
#include <wlr/util/log.h>

#include "viewport.h"

/* What the compositor tells clients it can handle. Kept to the two curves and
 * two gamuts that matter — sRGB for everything that exists today, PQ and
 * BT.2020 for the content that needs this at all — rather than claiming
 * support for conversions that have never been exercised here. */
static const enum wp_color_manager_v1_render_intent render_intents[] = {
	WP_COLOR_MANAGER_V1_RENDER_INTENT_PERCEPTUAL,
};

/* sRGB is deliberately absent from both lists: the protocol treats it as
 * always supported and wlroots asserts if it is advertised again. What is
 * listed is the addition — PQ and BT.2020 — which is what a client needs told
 * before it can hand over HDR content. */
static const enum wp_color_manager_v1_transfer_function transfer_functions[] = {
	WP_COLOR_MANAGER_V1_TRANSFER_FUNCTION_ST2084_PQ,
};

static const enum wp_color_manager_v1_primaries primaries[] = {
	WP_COLOR_MANAGER_V1_PRIMARIES_BT2020,
};

bool viewport_output_hdr_capable(struct viewport_output *output)
{
	struct wlr_output *wlr_output = output->wlr_output;

	return (wlr_output->supported_primaries &
			WLR_COLOR_NAMED_PRIMARIES_BT2020) != 0 &&
		(wlr_output->supported_transfer_functions &
			WLR_COLOR_TRANSFER_FUNCTION_ST2084_PQ) != 0;
}

bool viewport_output_set_hdr(struct viewport_output *output, bool enabled)
{
	struct wlr_output *wlr_output = output->wlr_output;

	if (enabled && !viewport_output_hdr_capable(output)) {
		wlr_log(WLR_ERROR, "%s does not support HDR", wlr_output->name);
		return false;
	}
	if (output->hdr == enabled) {
		return true;
	}

	/* SDR is sRGB primaries and the sRGB curve; HDR is BT.2020 and PQ. The
	 * luminance fields are left at zero, which means "unset" — the display's
	 * own capabilities are then used rather than numbers this compositor would
	 * be inventing. */
	struct wlr_output_image_description image_desc = {
		.primaries = enabled
			? WLR_COLOR_NAMED_PRIMARIES_BT2020
			: WLR_COLOR_NAMED_PRIMARIES_SRGB,
		.transfer_function = enabled
			? WLR_COLOR_TRANSFER_FUNCTION_ST2084_PQ
			: WLR_COLOR_TRANSFER_FUNCTION_SRGB,
	};

	struct wlr_output_state state;
	wlr_output_state_init(&state);
	if (!wlr_output_state_set_image_description(&state, &image_desc)) {
		wlr_output_state_finish(&state);
		wlr_log(WLR_ERROR, "%s rejected the image description",
			wlr_output->name);
		return false;
	}

	/* Ten bits per channel if the hardware will take it: eight cannot carry a
	 * PQ curve without visible banding in the dark gradients this exists for.
	 *
	 * But it is a preference, not a requirement. Which deep formats a plane
	 * accepts depends on the driver, the connector and what else is on screen,
	 * and a display that refuses one of them may well take another — or take
	 * HDR at eight bits, which is worth having even with the banding. So the
	 * options are tried in order of preference and the first that the hardware
	 * accepts is the one committed. Each is tested rather than committed
	 * hopefully: a mode the hardware will not take must leave the monitor
	 * showing what it was showing, not go dark while someone works out why. */
	static const uint32_t deep_formats[] = {
		DRM_FORMAT_XRGB2101010,
		DRM_FORMAT_XBGR2101010,
	};

	bool ok_test = false;
	uint32_t chosen = DRM_FORMAT_INVALID;

	if (enabled) {
		for (size_t i = 0; i < sizeof(deep_formats) / sizeof(deep_formats[0]);
				i++) {
			wlr_output_state_set_render_format(&state, deep_formats[i]);
			if (wlr_output_test_state(wlr_output, &state)) {
				ok_test = true;
				chosen = deep_formats[i];
				break;
			}
		}

		if (!ok_test) {
			/* Whatever the output is already rendering at. HDR at eight bits
			 * bands, but banded HDR beats no HDR. */
			wlr_output_state_set_render_format(&state, wlr_output->render_format);
			ok_test = wlr_output_test_state(wlr_output, &state);
			if (ok_test) {
				wlr_log(WLR_INFO,
					"%s would not take a 10-bit format; HDR at the current "
					"depth, expect banding in dark gradients",
					wlr_output->name);
			}
		}
	} else {
		ok_test = wlr_output_test_state(wlr_output, &state);
	}

	if (!ok_test) {
		wlr_output_state_finish(&state);
		wlr_log(WLR_ERROR, "%s will not take HDR in any tried format",
			wlr_output->name);
		return false;
	}
	if (chosen != DRM_FORMAT_INVALID) {
		wlr_log(WLR_DEBUG, "%s HDR at format 0x%08x", wlr_output->name, chosen);
	}

	bool ok = wlr_output_commit_state(wlr_output, &state);
	wlr_output_state_finish(&state);
	if (!ok) {
		wlr_log(WLR_ERROR, "%s failed to switch HDR %s", wlr_output->name,
			enabled ? "on" : "off");
		return false;
	}

	output->hdr = enabled;
	wlr_log(WLR_INFO, "%s HDR %s", wlr_output->name, enabled ? "on" : "off");

	viewport_ipc_notify_output_layout(output->server);
	return true;
}

void viewport_hdr_init(struct viewport_server *server)
{
	const struct wlr_color_manager_v1_options options = {
		/* Only what wlroots 0.20 actually implements. It asserts on the
		 * others rather than ignoring them, which is the better failure —
		 * advertising a feature the compositor cannot honour would have
		 * clients hand over descriptions nothing acts on. */
		.features = {
			.parametric = true,
		},
		.render_intents = render_intents,
		.render_intents_len =
			sizeof(render_intents) / sizeof(render_intents[0]),
		.transfer_functions = transfer_functions,
		.transfer_functions_len =
			sizeof(transfer_functions) / sizeof(transfer_functions[0]),
		.primaries = primaries,
		.primaries_len = sizeof(primaries) / sizeof(primaries[0]),
	};

	server->color_manager = wlr_color_manager_v1_create(server->wl_display, 1,
		&options);
	if (server->color_manager == NULL) {
		wlr_log(WLR_ERROR, "colour management unavailable; HDR will not work");
		return;
	}

	/* This is the half that makes HDR usable rather than merely on. Without it
	 * the scene has no idea what any buffer contains, so switching an output to
	 * PQ would reinterpret every SDR window as HDR instead of converting it. */
	wlr_scene_set_color_manager_v1(server->scene, server->color_manager);
}
