// SPDX-License-Identifier: GPL-3.0-or-later
//
// Bounding untrusted text without cutting a character in half.

/// A UTF-8-safe prefix of `text` no longer than `max` bytes.
///
/// Every D-Bus client is untrusted input: a string it hands over can be as
/// large as the bus allows, and storing or re-sending it unmodified is how one
/// application grows the compositor. Cutting on a byte offset that can fall
/// inside a character is worse than the oversized string: what comes out is
/// not text the shell can parse.
pub(crate) fn truncate(text: &str, max: usize, what: &str) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    tracing::debug!("{what} truncated from {} to {end} bytes", text.len());
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_never_splits_a_character() {
        // `é` is two bytes; a two-byte cut through it would hand the shell a
        // `String` that is not one.
        assert_eq!(truncate("aé", 2, "test"), "a");
        assert_eq!(truncate("aé", 3, "test"), "aé");
        assert_eq!(truncate("short", 64, "test"), "short");
    }
}
