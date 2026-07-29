// SPDX-License-Identifier: GPL-3.0-or-later
//
// The screencast portal: org.freedesktop.impl.portal.ScreenCast, and the
// PipeWire streams it hands out.
//
// xdg-desktop-portal-wlr can only offer monitors — wlr-screencopy captures
// outputs and nothing else — so a browser asking to share a window gets a
// screen instead. Owning the interface is what lets this compositor offer a
// window: it already composites them, it already publishes the list of them,
// and the only missing piece was the transport a consumer expects, which is
// PipeWire. niri and hyprland arrived at the same answer for the same reason.

pub mod stream;

use smithay::output::Output;

/// One source a client is watching.
pub struct Cast {
    pub output: Output,
    pub stream: stream::Stream,
}
