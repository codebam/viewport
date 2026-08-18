// SPDX-License-Identifier: GPL-3.0-or-later
//
// What was copied, kept.
//
// A Wayland selection is not a buffer anywhere: it is an offer from the client
// that owns it, and the data only exists while that client is running. Close
// the terminal you copied from and the clipboard is empty — which is why every
// desktop grows a clipboard manager, and why one is usually a second daemon
// holding a data-control connection open.
//
// The compositor is already that daemon. It brokers `wl_data_device`, the
// primary selection and `wlr-data-control`, so every selection on the session
// passes through it, and keeping the last few is a matter of reading what is
// already being offered rather than of standing up a client to do it.
//
// Text only, and only the clipboard. Recording the primary selection would
// mean an entry for every word dragged over with a mouse, and images are
// megabytes each with nowhere to draw them — a shell picker shows lines.
//
// Reading and writing both happen on threads, because both ends of a selection
// are a pipe to another process. A client that offers the clipboard and then
// stops reading its end would otherwise stall the compositor for as long as it
// felt like it.

use std::io::{Read, Write};

use viewport_ipc::event::ClipboardEntry;

/// The largest entry kept.
///
/// A clipboard holds what a person copied, and what a person copied fits on a
/// screen. This is generous for that and small enough that a run of them costs
/// nothing; a client that offers a hundred megabytes of text is offering
/// something nobody is going to paste into a picker.
const MAX_BYTES: usize = 256 * 1024;

/// How many entries are kept unless the configuration says otherwise.
const DEFAULT_LIMIT: usize = 25;

/// The mime types this will take, in the order they are preferred.
///
/// UTF-8 first, then the untyped one — which is Latin-1 by the letter of the
/// specification and UTF-8 in practice everywhere — then the two X11 spellings
/// that come through XWayland.
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain;charset=UTF-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];

/// What a reader thread sends back.
#[derive(Debug)]
pub enum Message {
    /// A selection, read to the end.
    Copied(String),
}

/// Who owns the selection the compositor has set.
///
/// The compositor sets one for two unrelated reasons, and what to do when a
/// client asks for the data is different in each: an X client's selection is
/// fetched back through the X connection, and the compositor's own is text it
/// is already holding. Before this there was one server-side selection and no
/// way to tell them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// An X client owns it. The bytes come back through the XWM.
    Xwayland,
    /// The compositor owns it, because something was pasted out of the
    /// history.
    History,
}

/// The history, and the current entry.
pub struct Clipboard {
    /// Newest first, which is the order a picker shows them in.
    entries: Vec<ClipboardEntry>,
    /// The next id. Ids are never reused within a session, so a picker's
    /// answer cannot land on an entry that has since moved.
    next: u32,
    limit: usize,
    /// What the compositor put on the clipboard itself, so the selection it
    /// caused is not recorded as a new copy — without this, pasting from the
    /// picker moves that entry to the top and the history slowly becomes one
    /// entry repeated.
    ours: Option<String>,
    reader: Option<smithay::reexports::calloop::channel::Sender<Message>>,
}

impl Default for Clipboard {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            next: 1,
            limit: DEFAULT_LIMIT,
            ours: None,
            reader: None,
        }
    }
}

impl Clipboard {
    /// Where a reader thread's answer goes. Called once, from the event loop.
    pub fn attach(&mut self, reader: smithay::reexports::calloop::channel::Sender<Message>) {
        self.reader = Some(reader);
    }

    /// How many entries to keep. Zero turns the history off and empties it,
    /// which is what a configuration that does not want one asks for.
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
        self.trim();
    }

    pub fn enabled(&self) -> bool {
        self.limit > 0
    }

    pub fn entries(&self) -> &[ClipboardEntry] {
        &self.entries
    }

    /// The mime type to ask for, out of what a source offers.
    ///
    /// `None` for a selection with no text in it at all — an image, a file
    /// list, an application's own private type — which is not an error and not
    /// worth a log line: a screenshot tool puts one on the clipboard every
    /// time it runs.
    pub fn text_mime(offered: &[String]) -> Option<String> {
        TEXT_MIMES
            .iter()
            .find(|wanted| offered.iter().any(|mime| mime.eq_ignore_ascii_case(wanted)))
            .map(|mime| (*mime).to_owned())
    }

    /// Start reading a selection somebody just made.
    ///
    /// The pipe's write end goes to the client, which fills it and closes it;
    /// this end is read to EOF on a thread. Nothing here waits: a client is
    /// under no obligation to write promptly, and several write nothing at all
    /// until they are asked twice.
    pub fn capture(&self, read: std::os::unix::io::OwnedFd) {
        let Some(reader) = self.reader.clone() else {
            return;
        };
        let spawned = std::thread::Builder::new()
            .name("clipboard".to_owned())
            .spawn(move || {
                let mut file = std::fs::File::from(read);
                let mut buffer = Vec::new();
                // Bounded, and bounded by *reading* rather than by asking how
                // much there is: a pipe has no length, and a client offering
                // more than the cap is one whose first quarter-megabyte is
                // still worth keeping.
                let mut chunk = [0u8; 8192];
                loop {
                    match file.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            buffer.extend_from_slice(&chunk[..n]);
                            if buffer.len() >= MAX_BYTES {
                                buffer.truncate(MAX_BYTES);
                                break;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => return,
                    }
                }
                // Lossy, deliberately: a truncated read can cut a character in
                // half, and a picker showing one replacement mark is better
                // than an entry silently dropped.
                let text = String::from_utf8_lossy(&buffer).into_owned();
                let _ = reader.send(Message::Copied(text));
            });
        if let Err(e) = spawned {
            tracing::warn!("could not read the clipboard: {e}");
        }
    }

    /// Record what a reader thread came back with.
    ///
    /// Returns whether the history changed, so the caller knows whether the
    /// shell has anything new to be told.
    pub fn record(&mut self, text: String) -> bool {
        if self.limit == 0 {
            return false;
        }
        // Whitespace alone is what a double-click on an empty line offers, and
        // an entry a picker draws as a blank row is one nobody can choose on
        // purpose.
        if text.trim().is_empty() {
            return false;
        }
        // Our own paste coming back round. Cleared once it has: the next copy
        // of the same text is a real one, made by a person pressing a key.
        if self.ours.as_deref() == Some(text.as_str()) {
            self.ours = None;
            return false;
        }
        // Copying the same thing twice moves it to the top rather than filling
        // the history with it. Several applications set the selection more
        // than once for one copy — a browser does it as the page settles — and
        // without this a single ^C fills a quarter of the list.
        self.entries.retain(|entry| entry.text != text);

        let id = self.next;
        self.next = self.next.wrapping_add(1).max(1);
        self.entries.insert(0, ClipboardEntry { id, text });
        self.trim();
        true
    }

    /// The text of one entry, moved to the top because it has just been used.
    ///
    /// A picker's answer is what somebody chose, so it becomes the newest
    /// thing copied — which is what the next paste will find and what every
    /// clipboard manager does.
    pub fn take(&mut self, id: u32) -> Option<String> {
        let index = self.entries.iter().position(|entry| entry.id == id)?;
        let entry = self.entries.remove(index);
        let text = entry.text.clone();
        self.entries.insert(0, entry);
        self.ours = Some(text.clone());
        Some(text)
    }

    /// What a client pasting from the compositor's own selection is handed.
    pub fn current(&self) -> Option<String> {
        self.ours.clone()
    }

    /// Forget everything. What somebody asks for after copying a password.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.ours = None;
    }

    /// Forget one entry.
    pub fn remove(&mut self, id: u32) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.id != id);
        self.entries.len() != before
    }

    fn trim(&mut self) {
        self.entries.truncate(self.limit);
    }
}

/// The mime types the compositor offers when it owns the clipboard.
///
/// Every spelling it can answer, because a client asks for one of them and
/// takes silence for an empty clipboard: GTK asks for
/// `text/plain;charset=utf-8`, a terminal often asks for `text/plain`, and
/// XWayland asks for `UTF8_STRING`.
pub fn offered_mimes() -> Vec<String> {
    TEXT_MIMES.iter().map(|mime| (*mime).to_owned()).collect()
}

/// Hand text to a client that is pasting.
///
/// On a thread, because the other end of this pipe is a program that may not
/// be reading yet: a pipe's buffer is 64 KiB and an entry may be four times
/// that, so the write blocks until the client gets round to it. That is fine
/// on a thread and is a frozen desktop anywhere else.
pub fn serve(text: String, fd: std::os::unix::io::OwnedFd) {
    let spawned = std::thread::Builder::new()
        .name("clipboard-write".to_owned())
        .spawn(move || {
            let mut file = std::fs::File::from(fd);
            // The error is dropped rather than logged: a client that asks for
            // the selection and exits before reading it closes the pipe, and
            // EPIPE here is that and nothing else.
            let _ = file.write_all(text.as_bytes());
        });
    if let Err(e) = spawned {
        tracing::warn!("could not hand over the clipboard: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clipboard() -> Clipboard {
        Clipboard::default()
    }

    /// The mime type asked for is the best of what was offered, and a
    /// selection with no text in it is not an error.
    #[test]
    fn the_text_type_is_chosen_out_of_what_was_offered() {
        let mimes =
            |list: &[&str]| -> Vec<String> { list.iter().map(|m| (*m).to_owned()).collect() };
        assert_eq!(
            Clipboard::text_mime(&mimes(&[
                "image/png",
                "text/plain",
                "text/plain;charset=utf-8"
            ])),
            Some("text/plain;charset=utf-8".to_owned())
        );
        assert_eq!(
            Clipboard::text_mime(&mimes(&["text/plain"])),
            Some("text/plain".to_owned())
        );
        // The X11 spellings, which arrive through XWayland.
        assert_eq!(
            Clipboard::text_mime(&mimes(&["UTF8_STRING"])),
            Some("UTF8_STRING".to_owned())
        );
        assert_eq!(Clipboard::text_mime(&mimes(&["image/png"])), None);
        assert_eq!(Clipboard::text_mime(&[]), None);
    }

    /// Newest first, and copying the same thing twice moves it rather than
    /// listing it twice — several applications set the selection more than
    /// once for a single copy.
    #[test]
    fn the_newest_copy_is_first_and_duplicates_move() {
        let mut clipboard = clipboard();
        assert!(clipboard.record("one".to_owned()));
        assert!(clipboard.record("two".to_owned()));
        assert_eq!(clipboard.entries()[0].text, "two");

        assert!(clipboard.record("one".to_owned()));
        assert_eq!(clipboard.entries().len(), 2);
        assert_eq!(clipboard.entries()[0].text, "one");
    }

    /// Nothing worth showing is not recorded: a picker row that looks empty is
    /// one nobody can choose on purpose.
    #[test]
    fn blank_selections_are_not_history() {
        let mut clipboard = clipboard();
        assert!(!clipboard.record(String::new()));
        assert!(!clipboard.record("   \n\t".to_owned()));
        assert!(clipboard.entries().is_empty());
    }

    /// The limit is a limit, and zero means the feature is off.
    #[test]
    fn the_history_is_bounded_and_can_be_turned_off() {
        let mut clipboard = clipboard();
        clipboard.set_limit(2);
        for text in ["one", "two", "three"] {
            clipboard.record(text.to_owned());
        }
        assert_eq!(clipboard.entries().len(), 2);
        assert_eq!(clipboard.entries()[0].text, "three");

        clipboard.set_limit(0);
        assert!(clipboard.entries().is_empty());
        assert!(!clipboard.record("four".to_owned()));
        assert!(!clipboard.enabled());
    }

    /// Pasting from the picker puts the entry on the clipboard, and the
    /// selection that causes is not recorded as a new copy — or the history
    /// would slowly become one entry repeated.
    #[test]
    fn a_paste_does_not_come_back_as_a_copy() {
        let mut clipboard = clipboard();
        clipboard.record("one".to_owned());
        clipboard.record("two".to_owned());
        let id = clipboard.entries()[1].id;

        assert_eq!(clipboard.take(id).as_deref(), Some("one"));
        assert_eq!(
            clipboard.entries()[0].text,
            "one",
            "and it moves to the top"
        );
        assert_eq!(clipboard.current().as_deref(), Some("one"));

        assert!(!clipboard.record("one".to_owned()), "our own selection");
        assert_eq!(clipboard.entries().len(), 2);

        // But a person copying the same text again is a real copy.
        assert!(clipboard.record("one".to_owned()));
    }

    /// Forgetting: one entry, or everything, which is what somebody asks for
    /// after copying a password.
    #[test]
    fn entries_can_be_forgotten() {
        let mut clipboard = clipboard();
        clipboard.record("one".to_owned());
        clipboard.record("two".to_owned());
        let id = clipboard.entries()[0].id;
        assert!(clipboard.remove(id));
        assert!(!clipboard.remove(id), "and only once");
        assert_eq!(clipboard.entries().len(), 1);

        clipboard.clear();
        assert!(clipboard.entries().is_empty());
        assert_eq!(clipboard.current(), None);
    }

    /// What is offered when the compositor owns the clipboard is every
    /// spelling of plain text it can answer, because a client asks for one of
    /// them and reads silence as an empty clipboard.
    #[test]
    fn what_is_offered_covers_the_spellings_clients_ask_for() {
        let offered = offered_mimes();
        for wanted in [
            "text/plain;charset=utf-8",
            "text/plain",
            "UTF8_STRING",
            "STRING",
        ] {
            assert!(offered.iter().any(|mime| mime == wanted), "{wanted}");
        }
    }

    /// Ids are not positions: an entry keeps its own for as long as it is in
    /// the history, so a picker's answer cannot land on the wrong row when
    /// something is copied while it is open.
    #[test]
    fn an_id_names_the_entry_rather_than_where_it_sits() {
        let mut clipboard = clipboard();
        clipboard.record("one".to_owned());
        let id = clipboard.entries()[0].id;
        clipboard.record("two".to_owned());
        assert_eq!(clipboard.entries()[1].id, id);
        assert_eq!(clipboard.take(id).as_deref(), Some("one"));
    }
}
