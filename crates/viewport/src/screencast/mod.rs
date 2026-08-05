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
///
/// Three of these name nothing in particular. A share is a thing somebody set
/// up once and then went back to working, and what they meant to show is
/// usually "whatever I am doing" rather than the window that happened to be in
/// front when the browser asked. The compositor is the only place that can
/// answer that as it changes, so it is answered here rather than being frozen
/// at the moment of the choice — see [`Target`].
#[derive(Debug, Clone)]
pub enum Source {
    Output(Output),
    /// By view id rather than by window, because a window can close while a
    /// share is running and the id is what the shell and the compositor both
    /// speak.
    Window(u32),
    /// Every monitor at once, laid out as they are on the desk.
    ///
    /// One picture rather than one stream per screen: the interface hands back
    /// a list of streams and consumers overwhelmingly read the first, so a
    /// second stream is a screen nobody sees.
    AllOutputs,
    /// Whichever window has the keyboard, as that changes.
    FollowWindow,
    /// Whichever monitor is being worked on, as that changes.
    FollowOutput,
}

impl Source {
    /// What the portal calls this kind of source.
    ///
    /// The desk as a whole is a monitor as far as the interface is concerned:
    /// there is no source type for it, and "virtual" means a screen the
    /// compositor invented for the client rather than one it can point at.
    pub fn kind(&self) -> u32 {
        match self {
            Self::Output(_) | Self::AllOutputs | Self::FollowOutput => SOURCE_MONITOR,
            Self::Window(_) | Self::FollowWindow => SOURCE_WINDOW,
        }
    }
}

/// What a [`Source`] means right now.
///
/// The three following sources collapse into the two concrete ones plus the
/// whole desk, so everything downstream — sizing, compositing, reading back —
/// is written once against this rather than five times against the source.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Output(Output),
    Window(u32),
    AllOutputs,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn an_output() -> Output {
        Output::new(
            "TEST-1".to_owned(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "Viewport".into(),
                model: "Test".into(),
                serial_number: "Unknown".into(),
            },
        )
    }

    /// The number the portal answers with is what a browser draws its badge
    /// from — "you are sharing a window" against "you are sharing a screen".
    /// A following source that reported the wrong one would tell somebody they
    /// were sharing one window while the compositor handed over a monitor.
    #[test]
    fn a_following_source_is_the_kind_it_follows() {
        assert_eq!(Source::Output(an_output()).kind(), SOURCE_MONITOR);
        assert_eq!(Source::AllOutputs.kind(), SOURCE_MONITOR);
        assert_eq!(Source::FollowOutput.kind(), SOURCE_MONITOR);
        assert_eq!(Source::Window(1).kind(), SOURCE_WINDOW);
        assert_eq!(Source::FollowWindow.kind(), SOURCE_WINDOW);
    }

    /// Every source is one of the two the interface knows about. A third
    /// number here is one no frontend has a name for, and the share would be
    /// described to the user as nothing at all.
    #[test]
    fn no_source_invents_a_type() {
        for source in [
            Source::Output(an_output()),
            Source::Window(1),
            Source::AllOutputs,
            Source::FollowWindow,
            Source::FollowOutput,
        ] {
            let kind = source.kind();
            assert!(
                kind == SOURCE_MONITOR || kind == SOURCE_WINDOW,
                "{source:?} answers with {kind}"
            );
        }
    }
}
