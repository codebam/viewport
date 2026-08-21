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

// The other interface served on the same bus name, out of the same session
// table: the screen share plus the input that drives it. It lives here rather
// than beside it because it is not a second portal so much as the same one
// with the keyboard added — see `remote.rs`.
pub mod remote;

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

    /// What to call this in a log line.
    ///
    /// Not `Debug`, which is what the line that says a screen was shared
    /// without asking used to print: an `Output` debugs as every mode it has,
    /// every instance bound to it and its whole xdg twin, which is fourteen
    /// hundred characters where a name was wanted. A line nobody can read is a
    /// line that is not there, and that one is the record of a share the user
    /// was never asked about.
    pub fn describe(&self) -> String {
        match self {
            Self::Output(output) => format!("the monitor {}", output.name()),
            Self::Window(id) => format!("window {id}"),
            Self::AllOutputs => "all monitors".to_owned(),
            Self::FollowWindow => "the focused window".to_owned(),
            Self::FollowOutput => "the active monitor".to_owned(),
        }
    }
}

/// A source written down, so the same thing can be shared again without
/// asking.
///
/// A recorder is set up once and used for months: OBS remembers which screen a
/// scene captures, and a portal that cannot answer "the same one as last time"
/// makes the user pick it again on every launch — and every compositor
/// restart. The interface has a way to say it, and this is what it says.
///
/// Not the live source. A [`Source::Output`] holds an `Output`, and a
/// [`Source::Window`] holds a view id, and neither survives the compositor
/// exiting: ids are handed out afresh and the output object is a new one. What
/// does survive is what a person would say — the name on the monitor, the
/// application in the window — so that is what is written down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remembered {
    /// A monitor, by the name the rest of the desktop calls it.
    Output(String),
    /// A window, by what is in it rather than by which one it was.
    ///
    /// Both, because neither is enough on its own: a browser has ten windows
    /// with one app id between them, and a title changes as soon as somebody
    /// types. The title picks the right one of ten when it still matches, and
    /// the app id is what is left to go on when it does not.
    Window {
        app_id: String,
        title: String,
    },
    AllOutputs,
    FollowWindow,
    FollowOutput,
}

impl Remembered {
    /// What the portal calls this kind of source, so a restored share can be
    /// checked against what the application asked for.
    pub fn kind(&self) -> u32 {
        match self {
            Self::Output(_) | Self::AllOutputs | Self::FollowOutput => SOURCE_MONITOR,
            Self::Window { .. } | Self::FollowWindow => SOURCE_WINDOW,
        }
    }
}

/// One open window, as much of it as choosing between them needs.
pub struct Open {
    pub id: u32,
    pub app_id: String,
    pub title: String,
}

/// Which open window a remembered one meant, if any of them.
///
/// The exact one first — same application, same title — and any window of the
/// same application after that, in the order they are given. A title is what
/// tells ten browser windows apart and it is also the first thing to change,
/// so it decides while it holds and is dropped when it does not.
///
/// Nothing when the application is not running. Sharing some other window
/// because it is the one left is how a screen share ends up showing a password
/// manager, and no answer here means the user is asked.
pub fn matching_window(app_id: &str, title: &str, open: &[Open]) -> Option<u32> {
    open.iter()
        .find(|window| window.app_id == app_id && window.title == title)
        // An empty app id matches every window that has none, which is every
        // window whose client never said. Those are told apart by title or not
        // at all.
        .or_else(|| {
            open.iter()
                .find(|window| !app_id.is_empty() && window.app_id == app_id)
        })
        .map(|window| window.id)
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
    /// What a remote-desktop session is asking to be able to drive, when this
    /// chooser is one of those; zero for a plain screen share.
    ///
    /// Carried on the chooser rather than left to the reply because it is the
    /// part the person has to read. Agreeing to be watched and agreeing to be
    /// typed into are different decisions, and a dialogue that showed the same
    /// list of windows for both would be asking the second question in the
    /// words of the first.
    pub devices: u32,
    /// The shortcuts an application is asking to be told about, when this
    /// chooser is one of those; empty otherwise.
    ///
    /// Carried here for the same reason `devices` is: it is what the person
    /// has to read. A list of chords is a third question — not what may be
    /// watched, not whether the machine may be driven, but which keys stop
    /// belonging to whatever has focus — and it is asked in its own message,
    /// `shortcuts.pick`.
    pub shortcuts: Vec<crate::shortcuts::Granted>,
    /// Which application is asking, for the sentence at the top of that one.
    pub app: String,
    /// Where the answer goes when there is one.
    pub reply: Reply,
}

/// Who is waiting on the chooser.
///
/// Two shapes because the two interfaces answer differently — a screen share
/// hands back one stream, a remote-desktop session hands back the devices that
/// were granted and a stream only if one was asked for — and one chooser
/// because there is one keyboard and one person. Making `Picker` generic over
/// the answer would put a type parameter on the compositor's single chooser
/// slot for the sake of two variants.
///
/// Cloned rather than moved when a share is started, because starting one no
/// longer answers it: the reply has to be in two places at once — with the
/// promise that answers success, and here where the deadline can reach it to
/// answer failure — and each half needs its own sender.
#[derive(Debug, Clone)]
pub enum Reply {
    Cast(async_channel::Sender<Result<portal::Started, String>>),
    Remote(async_channel::Sender<Result<remote::Started, String>>),
    /// A global-shortcuts request: the answer is the list of chords the
    /// application may hear. Here rather than in a chooser of its own because
    /// there is one keyboard and one person — see `Picker`.
    Shortcuts(async_channel::Sender<Result<Vec<crate::shortcuts::Granted>, String>>),
}

impl Reply {
    /// Tell whoever is waiting that nothing was chosen.
    ///
    /// Both arms say the same thing, and they have to: an application left
    /// waiting on a chooser that has gone shows a share, or a remote session,
    /// that is forever about to start. Dropping the sender would also end the
    /// wait — the other end reads a closed channel as no answer — but saying
    /// it keeps the reason in one place.
    pub fn refuse(self, why: &str) {
        match self {
            Self::Cast(reply) => {
                let _ = reply.try_send(Err(why.to_owned()));
            }
            Self::Remote(reply) => {
                let _ = reply.try_send(Err(why.to_owned()));
            }
            Self::Shortcuts(reply) => {
                let _ = reply.try_send(Err(why.to_owned()));
            }
        }
    }

    /// Hand a finished share to whoever is waiting for it.
    ///
    /// Sent from wherever the news arrives — the PipeWire thread, the moment
    /// the daemon names the node — which is why everything the answer needs
    /// travels in owned data and senders, and nothing here reaches back into
    /// the compositor. The remote-desktop shape wraps the same stream, plus
    /// the devices granted alongside it; a shortcuts chooser never started a
    /// share, so there is nothing it could be handed.
    pub fn share(self, cast: portal::Started, devices: u32) {
        match self {
            Self::Cast(reply) => {
                let _ = reply.try_send(Ok(cast));
            }
            Self::Remote(reply) => {
                let _ = reply.try_send(Ok(remote::Started {
                    devices,
                    cast: Some(cast),
                }));
            }
            Self::Shortcuts(_) => {}
        }
    }
}

/// What is left of a chooser once the choice has been taken out of it.
///
/// `confirm_screencast_pick` moves the selected source out of `sources`, which
/// consumes the `Picker`; the three fields below are still needed after that —
/// what to answer, where the keyboard goes back to, and the shortcut list a
/// shortcuts request is agreeing to. A struct rather than three locals so that
/// adding a fourth is a change in one place.
pub struct Answered {
    pub shortcuts: Vec<crate::shortcuts::Granted>,
    pub restore: u32,
    pub reply: Reply,
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

    fn open(id: u32, app_id: &str, title: &str) -> Open {
        Open {
            id,
            app_id: app_id.to_owned(),
            title: title.to_owned(),
        }
    }

    /// The window that still has the same title is the one that was shared,
    /// not merely one of the same application. A browser with ten windows open
    /// is the ordinary case, and restoring the wrong one shares the wrong tab.
    #[test]
    fn a_remembered_window_is_matched_by_title_first() {
        let windows = [
            open(1, "firefox", "Mail"),
            open(2, "firefox", "The slides"),
            open(3, "foot", "a shell"),
        ];
        assert_eq!(matching_window("firefox", "The slides", &windows), Some(2));
    }

    /// A title that has changed still leaves the application to go on, which
    /// is the whole reason both are written down: a recorder set up on a
    /// browser window should not stop working because somebody switched tabs.
    #[test]
    fn a_renamed_window_falls_back_to_its_application() {
        let windows = [open(1, "foot", "a shell"), open(2, "firefox", "Something")];
        assert_eq!(matching_window("firefox", "The slides", &windows), Some(2));
    }

    /// An application that is not running matches nothing. Handing back
    /// whatever else is open would share a window nobody agreed to; no answer
    /// is what puts the chooser up instead.
    #[test]
    fn a_closed_application_matches_nothing() {
        let windows = [open(1, "foot", "a shell")];
        assert_eq!(matching_window("firefox", "The slides", &windows), None);
    }

    /// A window whose client never said what it is does not match every other
    /// nameless window. Empty is not an identity, and the fallback would
    /// otherwise hand back the first anonymous window on the desk.
    #[test]
    fn an_empty_application_matches_only_by_title() {
        let windows = [open(1, "", "Something else"), open(2, "", "The slides")];
        assert_eq!(matching_window("", "The slides", &windows), Some(2));
        assert_eq!(matching_window("", "A window that closed", &windows), None);
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
