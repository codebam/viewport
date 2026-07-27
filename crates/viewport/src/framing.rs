// SPDX-License-Identifier: GPL-3.0-or-later
//
// Message framing on the IPC control socket. Ports handle_client_event() in
// src/ipc.c:1514.
//
// Split out from the socket itself so the framing rules — which are the part
// with the sharp edges — can be tested without a file descriptor.

/// The point at which a client that never sends a newline is disconnected
/// rather than allowed to grow the compositor's heap without bound
/// (`src/ipc.c:1552`).
pub const MAX_PENDING: usize = 1 << 20;

/// Accumulates bytes from one client and yields complete lines.
#[derive(Debug, Default)]
pub struct Framer {
    buf: Vec<u8>,
}

/// What the caller should do after feeding a chunk.
#[derive(Debug, PartialEq, Eq)]
pub enum Framed {
    /// Zero or more complete messages. Empty lines are not included: the C
    /// build skips them with `if (i > start)` rather than dispatching an empty
    /// string into the parser.
    Messages(Vec<Vec<u8>>),
    /// The client overran the accumulator and must be disconnected. Any
    /// messages completed in the same chunk are discarded with it, as they are
    /// in the C build.
    Overrun,
}

impl Framer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes held back waiting for a newline.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Framed {
        if self.buf.len() + chunk.len() > MAX_PENDING {
            return Framed::Overrun;
        }
        self.buf.extend_from_slice(chunk);

        let mut messages = Vec::new();
        let mut start = 0;
        for i in 0..self.buf.len() {
            if self.buf[i] != b'\n' {
                continue;
            }
            if i > start {
                messages.push(self.buf[start..i].to_vec());
            }
            start = i + 1;
        }
        if start > 0 {
            self.buf.drain(..start);
        }

        Framed::Messages(messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages(framed: Framed) -> Vec<Vec<u8>> {
        match framed {
            Framed::Messages(m) => m,
            Framed::Overrun => panic!("unexpected overrun"),
        }
    }

    #[test]
    fn one_line_is_one_message() {
        let mut framer = Framer::new();
        assert_eq!(messages(framer.push(b"{\"type\":\"quit\"}\n")).len(), 1);
        assert_eq!(framer.pending(), 0);
    }

    #[test]
    fn a_partial_line_is_held_until_its_newline() {
        let mut framer = Framer::new();
        assert!(messages(framer.push(b"{\"type\":")).is_empty());
        assert_eq!(framer.pending(), 8);

        let done = messages(framer.push(b"\"quit\"}\n"));
        assert_eq!(done, vec![b"{\"type\":\"quit\"}".to_vec()]);
        assert_eq!(framer.pending(), 0);
    }

    #[test]
    fn several_lines_in_one_chunk_all_dispatch() {
        let mut framer = Framer::new();
        let done = messages(framer.push(b"a\nbb\nccc\n"));
        assert_eq!(done, vec![b"a".to_vec(), b"bb".to_vec(), b"ccc".to_vec()]);
    }

    #[test]
    fn empty_lines_are_skipped_not_dispatched() {
        // An empty string would reach the JSON parser and come back as a
        // malformed-message error the shell never caused.
        let mut framer = Framer::new();
        assert!(messages(framer.push(b"\n\n\n")).is_empty());
        assert_eq!(framer.pending(), 0);

        let done = messages(framer.push(b"\na\n\n"));
        assert_eq!(done, vec![b"a".to_vec()]);
    }

    #[test]
    fn a_trailing_partial_survives_the_lines_before_it() {
        let mut framer = Framer::new();
        let done = messages(framer.push(b"first\nsecond\nthi"));
        assert_eq!(done, vec![b"first".to_vec(), b"second".to_vec()]);
        assert_eq!(framer.pending(), 3);
    }

    #[test]
    fn a_client_that_never_sends_a_newline_is_cut_off() {
        let mut framer = Framer::new();
        let chunk = vec![b'x'; 4096];
        loop {
            match framer.push(&chunk) {
                Framed::Overrun => break,
                Framed::Messages(m) => assert!(m.is_empty()),
            }
            assert!(
                framer.pending() <= MAX_PENDING,
                "accumulator grew past the cap"
            );
        }
    }
}
