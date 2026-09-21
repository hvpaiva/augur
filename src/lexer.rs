//! Just enough of bash's tokenizer to split a command line into words and
//! operators the way the shell does.

/// Tokenizer state after reading a prefix of a command line.
///
/// Feed characters with [`LexState::feed`]; [`LexState::ends_token_before`] then
/// tells whether a given next character would end the current token. Quotes,
/// backslash escapes and ANSI-C `$'…'` strings are honoured, so `"fix the bug"`
/// is a single token.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LexState {
    quote: Quote,
    escaped: bool,
    kind: Kind,
    after_dollar: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Quote {
    #[default]
    None,
    Single,
    Double,
    AnsiC,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Kind {
    #[default]
    None,
    Word,
    Operator,
}

impl LexState {
    /// The state after reading `s`.
    pub fn of(s: &str) -> Self {
        let mut state = Self::default();
        s.chars().for_each(|c| state.feed(c));
        state
    }

    /// Advances the state past `c`.
    pub fn feed(&mut self, c: char) {
        let after_dollar = std::mem::take(&mut self.after_dollar);
        if self.escaped {
            self.escaped = false;
            return;
        }
        match self.quote {
            Quote::Single => {
                if c == '\'' {
                    self.quote = Quote::None;
                }
            }
            Quote::Double | Quote::AnsiC => match c {
                '\\' => self.escaped = true,
                '"' if self.quote == Quote::Double => self.quote = Quote::None,
                '\'' if self.quote == Quote::AnsiC => self.quote = Quote::None,
                _ => {}
            },
            Quote::None => {
                self.kind = if is_blank(c) {
                    Kind::None
                } else if is_operator(c) {
                    Kind::Operator
                } else {
                    Kind::Word
                };
                match c {
                    '\\' => self.escaped = true,
                    '\'' if after_dollar => self.quote = Quote::AnsiC,
                    '\'' => self.quote = Quote::Single,
                    '"' => self.quote = Quote::Double,
                    '$' => self.after_dollar = true,
                    _ => {}
                }
            }
        }
    }

    /// Whether `next` would end the current token, making the position before
    /// it a token boundary.
    pub fn ends_token_before(&self, next: char) -> bool {
        if self.escaped || self.quote != Quote::None {
            return false;
        }
        match self.kind {
            Kind::None => false,
            Kind::Word => is_blank(next) || is_operator(next),
            Kind::Operator => !is_operator(next),
        }
    }
}

/// A word or operator of a command line, as typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'a> {
    /// The token's text, quotes and escapes included.
    pub text: &'a str,
    /// Byte offset of the token in the line.
    pub start: usize,
    /// Whether the token is an operator (`|`, `&&`, `>`…) rather than a word.
    pub operator: bool,
}

impl Token<'_> {
    /// Whether the token separates commands (`|`, `&&`, `;`…), as opposed to a
    /// word or a redirection.
    pub fn is_separator(&self) -> bool {
        self.operator
            && self
                .text
                .chars()
                .all(|c| matches!(c, '|' | '&' | ';' | '(' | ')'))
    }
}

/// Splits `line` into its words and operators.
pub fn tokenize(line: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let mut state = LexState::default();
    let mut current: Option<(usize, Kind)> = None;
    for (i, c) in line.char_indices() {
        if state.ends_token_before(c)
            && let Some((start, kind)) = current.take()
        {
            tokens.push(Token {
                text: &line[start..i],
                start,
                operator: kind == Kind::Operator,
            });
        }
        state.feed(c);
        if current.is_none() && state.kind != Kind::None {
            current = Some((i, state.kind));
        }
    }
    if let Some((start, kind)) = current {
        tokens.push(Token {
            text: &line[start..],
            start,
            operator: kind == Kind::Operator,
        });
    }
    tokens
}

/// Byte offset where the word under the cursor starts, the cursor being at the
/// end of `line`: the line's length when the cursor is between words.
pub fn current_word_start(line: &str) -> usize {
    match tokenize(line).last() {
        Some(token) if !token.operator && token.start + token.text.len() == line.len() => {
            token.start
        }
        _ => line.len(),
    }
}

fn is_blank(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n')
}

fn is_operator(c: char) -> bool {
    matches!(c, '|' | '&' | ';' | '(' | ')' | '<' | '>')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(line: &str) -> Vec<&str> {
        tokenize(line).iter().map(|t| t.text).collect()
    }

    #[test]
    fn splits_words_and_operators() {
        assert_eq!(texts("git commit  -m x"), ["git", "commit", "-m", "x"]);
        assert_eq!(texts("ls|grep x"), ["ls", "|", "grep", "x"]);
        assert_eq!(
            texts("a && b >out 2>&1"),
            ["a", "&&", "b", ">", "out", "2", ">&", "1"]
        );
    }

    #[test]
    fn keeps_quoted_and_escaped_blanks_inside_a_word() {
        assert_eq!(
            texts(r#"git commit -m "fix the bug""#),
            ["git", "commit", "-m", r#""fix the bug""#]
        );
        assert_eq!(texts("echo 'a b'c"), ["echo", "'a b'c"]);
        assert_eq!(texts(r"echo $'it\'s here'"), ["echo", r"$'it\'s here'"]);
        assert_eq!(texts(r"echo a\ b"), ["echo", r"a\ b"]);
        assert_eq!(
            texts(r#"echo "unterminated str"#),
            ["echo", r#""unterminated str"#]
        );
    }

    #[test]
    fn records_offsets_and_separators() {
        let tokens = tokenize("cat f | wc -l; ls >x");
        let starts: Vec<_> = tokens.iter().map(|t| t.start).collect();
        assert_eq!(starts, [0, 4, 6, 8, 11, 13, 15, 18, 19]);
        let separators: Vec<_> = tokens
            .iter()
            .filter(|t| t.is_separator())
            .map(|t| t.text)
            .collect();
        assert_eq!(separators, ["|", ";"]);
    }

    #[test]
    fn finds_the_word_under_the_cursor() {
        assert_eq!(current_word_start("git sta"), 4);
        assert_eq!(current_word_start("git "), 4);
        assert_eq!(current_word_start("ls |"), 4);
        assert_eq!(current_word_start(r#"git commit -m "fix th"#), 14);
    }

    #[test]
    fn knows_where_the_current_token_ends() {
        assert!(LexState::of("git").ends_token_before(' '));
        assert!(!LexState::of("git ").ends_token_before(' '));
        assert!(!LexState::of("echo 'open").ends_token_before(' '));
    }
}
