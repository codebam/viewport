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

/// A screen share waiting on the user to say what to share.
///
/// The shell draws the list and the compositor routes the keys. That split is
/// not a shortcut: the shell is a web page the compositor composites and it
/// receives no input of its own, so anything the user drives has to be steered
/// from here. The overview works the same way.
pub struct Picker {
    /// Which request this is. An answer for an older one is ignored rather
    /// than applied to whatever is open now.
    pub id: u32,
    /// What the application will accept, in the order they are offered.
    pub sources: Vec<Source>,
    pub selected: usize,
    /// What was focused when the chooser went up, so the keyboard goes back
    /// where it was. Taking focus is how the chooser gets the keys at all, and
    /// leaving it nowhere afterwards is a desktop that stops typing.
    pub restore: u32,
    /// Where the answer goes when there is one.
    pub reply: async_channel::Sender<Result<portal::Started, String>>,
}

impl Picker {
    /// Move the highlight, wrapping at both ends.
    ///
    /// Wrapping because the list is short and a chooser that stops at the
    /// bottom makes the last item harder to reach than the first for no
    /// reason.
    pub fn step(&mut self, delta: isize) {
        if self.sources.is_empty() {
            return;
        }
        let count = self.sources.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(count) as usize;
    }
}
