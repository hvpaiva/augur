//! Field escaping shared by the wire protocol and the history log.
//!
//! Records are tab-separated and newline-terminated, so a field must not hold a
//! raw tab or newline. Backslash, tab, newline and carriage return are written as
//! `\\`, `\t`, `\n` and `\r`; every other character is kept verbatim.

/// Escapes `s` so it fits in a single tab-separated field.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '\t' => out.push_str(r"\t"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            c => out.push(c),
        }
    }
    out
}

/// Reverses [`escape`].
///
/// Unknown escape sequences and a trailing lone backslash are kept verbatim, so
/// decoding never fails.
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_special_characters() {
        let samples = [
            "",
            "plain",
            r"a\b",
            "tab\there",
            "multi\nline",
            "cr\r",
            r"\t is not a tab",
            r"trailing\",
            "ünïcödé ✓",
        ];
        for s in samples {
            let escaped = escape(s);
            assert!(!escaped.contains(['\t', '\n', '\r']), "{escaped:?}");
            assert_eq!(unescape(&escaped), s);
        }
    }

    /// `tests/shell.rs` runs the shell's encoder against this one.
    #[test]
    fn escapes_every_special_character_at_once() {
        assert_eq!(escape("a\\b\tc\nd\re & f \\t"), r"a\\b\tc\nd\re & f \\t");
    }

    #[test]
    fn keeps_unknown_sequences_verbatim() {
        assert_eq!(unescape(r"\x and \"), r"\x and \");
    }
}
