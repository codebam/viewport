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

pub mod portal;

use smithay::output::Output;

/// What a client asked to watch.
///
/// A window is the reason this portal exists: xdg-desktop-portal-wlr can only
/// offer monitors, because wlr-screencopy can only capture outputs.
#[derive(Debug, Clone)]
pub enum Source {
    Output(Output),
    /// By view id rather than by window, because a window can close while a
    /// share is running and the id is what the shell and the compositor both
    /// speak.
    Window(u32),
}

impl Source {
    /// What the portal calls this kind of source.
    pub fn kind(&self) -> u32 {
        match self {
            Self::Output(_) => SOURCE_MONITOR,
            Self::Window(_) => SOURCE_WINDOW,
        }
    }
}

/// The source types the portal offers, as the interface numbers them.
pub const SOURCE_MONITOR: u32 = 1;
pub const SOURCE_WINDOW: u32 = 2;

/// One source a client is watching.
pub struct Cast {
    pub source: Source,
    pub stream: stream::Stream,
}
