//! Line protocol between the shell integration and `augur serve`.
//!
//! The two halves are installed separately, the engine with `cargo install` and
//! the integration when a shell sources it, so a shell that has been open for
//! days may speak an older protocol than the engine on disk. Both state
//! [`VERSION`]: the integration asks `augur protocol` before starting the
//! engine, and starts it with `--protocol`, which the engine refuses when it
//! speaks another one. Change [`VERSION`], and `_ble_augur_protocol` in
//! `shell/augur.bash`, with every change to the messages below.
//!
//! Every message is one line of tab-separated fields. The shell sends:
//!
//! ```text
//! Q <TAB> id <TAB> cwd <TAB> line <TAB> word_start <TAB> match <TAB> candidates
//! N <TAB> names
//! ```
//!
//! A query (`Q`) asks for a suggestion for `line`, typed in `cwd`, with the
//! cursor at its end. `word_start` is the character index where the shell's
//! completion sees the current word start, empty when unknown. `candidates` are
//! the words the completion offers for it, each after one character telling
//! what it is: `f` for a file name, `w` for any other word. `match` says how
//! they were matched: `prefix`, `fuzzy`, or empty when there are none. `N`
//! gives the functions, aliases and builtins the shell defines. Text fields are
//! escaped with [`crate::escape`], and list items are separated by `\x1f`.
//!
//! augur answers each query with:
//!
//! ```text
//! R <TAB> id <TAB> edit <TAB> text <TAB> source
//! ```
//!
//! where `edit` is `+` to append `text` at the cursor, `=` to replace the
//! current word with `text`, empty for no suggestion, or `!` when the query
//! could not be understood, `text` saying why. The shell abandons a query as
//! soon as the user types again, so responses carry the id of the query they
//! answer. `text` is not escaped: suggestions never contain control characters.

use crate::escape::unescape;
use crate::suggest::{Candidate, Edit, Kind, Suggestion};

/// The protocol described here.
pub const VERSION: u32 = 2;

const SEPARATOR: char = '\x1f';
const QUERY_FIELDS: usize = 5;

/// A request from the shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// A suggestion is wanted.
    Query(Query),
    /// The functions, aliases and builtins the shell defines.
    Names(Vec<String>),
}

/// A request for a suggestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// Identifier echoed in the response.
    pub id: u64,
    /// Working directory of the shell.
    pub cwd: String,
    /// The line typed so far; the cursor is at its end.
    pub line: String,
    /// Byte offset where the current word starts, as the shell's completion
    /// sees it.
    pub word_start: Option<usize>,
    /// Whether the candidates matched fuzzily rather than by prefix.
    pub fuzzy: bool,
    /// Words the shell's completion offers for the current word.
    pub candidates: Vec<Candidate>,
}

/// Why a request line could not be understood.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// The request kind is not known.
    #[error("unknown request kind {0:?}")]
    Kind(String),
    /// The request has missing, extra or invalid fields.
    #[error("malformed request: {reason}")]
    Malformed {
        /// The query id, when it could be read, so that the shell still gets an
        /// answer instead of waiting for its timeout.
        id: Option<u64>,
        /// What is wrong with it.
        reason: String,
    },
}

fn malformed(id: Option<u64>, reason: impl Into<String>) -> ParseError {
    ParseError::Malformed {
        id,
        reason: reason.into(),
    }
}

/// Parses one request line, without its trailing newline.
pub fn parse_request(line: &str) -> Result<Request, ParseError> {
    let fields: Vec<&str> = line.split('\t').collect();
    match fields.as_slice() {
        ["Q", id, rest @ ..] => {
            let Ok(id) = id.parse::<u64>() else {
                return Err(malformed(None, format!("invalid query id {id:?}")));
            };
            let malformed = |reason: String| malformed(Some(id), reason);
            let [cwd, text, word_start, matching, candidates] = rest else {
                return Err(malformed(format!(
                    "a query has {QUERY_FIELDS} fields after its id, found {}",
                    rest.len()
                )));
            };
            let line = unescape(text);
            let word_start = match *word_start {
                "" => None,
                chars => Some(
                    chars
                        .parse()
                        .ok()
                        .and_then(|chars| byte_offset(&line, chars))
                        .ok_or_else(|| malformed(format!("invalid word start {chars:?}")))?,
                ),
            };
            let fuzzy = match *matching {
                "" | "prefix" => false,
                "fuzzy" => true,
                other => return Err(malformed(format!("unknown match {other:?}"))),
            };
            let candidates = list(candidates)
                .map(|item| candidate(&item))
                .collect::<Option<_>>()
                .ok_or_else(|| malformed("a candidate without its kind".to_owned()))?;
            Ok(Request::Query(Query {
                id,
                cwd: unescape(cwd),
                line,
                word_start,
                fuzzy,
                candidates,
            }))
        }
        ["N", names] => Ok(Request::Names(list(names).collect())),
        ["N", ..] => Err(malformed(None, "names take one field")),
        [kind, ..] => Err(ParseError::Kind((*kind).to_owned())),
        [] => Err(malformed(None, "empty request")),
    }
}

fn candidate(item: &str) -> Option<Candidate> {
    let mut chars = item.chars();
    let kind = match chars.next()? {
        'w' => Kind::Word,
        'f' => Kind::File,
        _ => return None,
    };
    Some(Candidate {
        word: chars.as_str().to_owned(),
        kind,
    })
}

/// Formats the response to query `id`, without the trailing newline.
pub fn format_response(id: u64, suggestion: Option<&Suggestion>) -> String {
    match suggestion {
        None => format!("R\t{id}\t\t\t"),
        Some(Suggestion { edit, source, .. }) => {
            let (kind, text) = match edit {
                Edit::Append(text) => ("+", text),
                Edit::Replace(word) => ("=", word),
            };
            format!("R\t{id}\t{kind}\t{text}\t{}", source.name())
        }
    }
}

/// Formats the response to a query `id` that could not be understood, without
/// the trailing newline.
pub fn format_error(id: u64, reason: &str) -> String {
    let reason: String = reason
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    format!("R\t{id}\t!\t{reason}\t")
}

fn list(field: &str) -> impl Iterator<Item = String> {
    field
        .split(SEPARATOR)
        .filter(|_| !field.is_empty())
        .map(unescape)
}

/// The byte offset of the `chars`-th character of `line`; its length when
/// `chars` is the character count.
fn byte_offset(line: &str, chars: usize) -> Option<usize> {
    line.char_indices()
        .map(|(offset, _)| offset)
        .chain(std::iter::once(line.len()))
        .nth(chars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::Source;

    #[test]
    fn parses_a_query_with_candidates() {
        let request =
            parse_request("Q\t7\t/home/u/a\\tb\tkubectl get po\t12\tprefix\twpods\x1ffpo.txt");
        assert_eq!(
            request,
            Ok(Request::Query(Query {
                id: 7,
                cwd: "/home/u/a\tb".to_owned(),
                line: "kubectl get po".to_owned(),
                word_start: Some(12),
                fuzzy: false,
                candidates: vec![Candidate::word("pods"), Candidate::file("po.txt")],
            }))
        );
    }

    #[test]
    fn converts_the_word_start_to_a_byte_offset() {
        let Ok(Request::Query(query)) = parse_request("Q\t1\t/\tcd café/pão\t8\t\t") else {
            panic!("expected a query");
        };
        assert_eq!(&query.line[query.word_start.expect("word start")..], "pão");
        assert!(query.candidates.is_empty());
    }

    #[test]
    fn parses_shell_names() {
        assert_eq!(
            parse_request("N\tbleopt\x1fble-bind"),
            Ok(Request::Names(vec![
                "bleopt".to_owned(),
                "ble-bind".to_owned()
            ]))
        );
    }

    #[test]
    fn keeps_the_id_of_a_malformed_query() {
        let id_of = |request: &str| match parse_request(request) {
            Err(ParseError::Malformed { id, .. }) => id,
            other => panic!("expected a malformed request, got {other:?}"),
        };
        assert_eq!(id_of("Q\t8\t/tmp"), Some(8));
        assert_eq!(
            id_of("Q\t9\t/\tls\t99\t\t"),
            Some(9),
            "word start past the end of the line"
        );
        assert_eq!(id_of("Q\tnope\t/\tls\t\t\t"), None);
        assert_eq!(parse_request("X\t1"), Err(ParseError::Kind("X".to_owned())));
    }

    #[test]
    fn rejects_the_queries_of_the_first_protocol() {
        // Four fields, and candidates without their kind.
        let Err(e) = parse_request("Q\t3\t/work\tkube") else {
            panic!("expected an error");
        };
        assert_eq!(
            e.to_string(),
            "malformed request: a query has 5 fields after its id, found 2"
        );
        assert!(matches!(
            parse_request("Q\t4\t/work\tkube\t0\tprefix\tkubectl"),
            Err(ParseError::Malformed { id: Some(4), .. })
        ));
    }

    #[test]
    fn formats_responses() {
        let append = Suggestion {
            edit: Edit::Append("ds".to_owned()),
            source: Source::History,
            confidence: 1.0,
        };
        let replace = Suggestion {
            edit: Edit::Replace("bleopt".to_owned()),
            source: Source::Correction,
            confidence: 1.0,
        };
        assert_eq!(format_response(3, Some(&append)), "R\t3\t+\tds\thistory");
        assert_eq!(
            format_response(4, Some(&replace)),
            "R\t4\t=\tbleopt\tcorrection"
        );
        assert_eq!(format_response(5, None), "R\t5\t\t\t");
        assert_eq!(format_error(6, "bad\tquery"), "R\t6\t!\tbad query\t");
    }
}
