//! Chooses what to suggest for the word under the cursor.
//!
//! Candidates for the word come from the shell's completion (installed
//! commands, subcommands, flags, files and, depending on the completion
//! scripts, live resources such as pods or branches) and from the token model
//! (words used before in the same context). The engine suggests:
//!
//! 1. the likeliest word the history supports, preferring words the completion
//!    also offers;
//! 2. otherwise, the only word the completion offers, or among several the one
//!    the language model finds likeliest, the shortest that continues what has
//!    been typed standing in when the model does not answer;
//! 3. otherwise, when no command starts with a command name as typed, a
//!    correction;
//! 4. otherwise, the language model's own continuation.
//!
//! A suggestion is one word: the rest of the word being typed, or the next word
//! when the current one is complete.
//!
//! That the completion offers a word makes it valid, not likely. Once a part of
//! the word has been typed, the completion's words that continue it are few and
//! one of them is suggested. With nothing typed they are every valid word, so
//! one among several shows only when the history or the model points at it;
//! and file names show only when the history does, since the completion lists
//! the directory for any command it knows nothing about. A guess no completion backs, from
//! the history or the model, shows only when it is likely enough, and never
//! when it names a file that is not there.

use std::collections::{HashSet, VecDeque};

use std::path::{Path, PathBuf};

use crate::commands::Commands;
use crate::files::{self, FileLookup};
use crate::fuzzy;
use crate::history::Entry;
use crate::lexer;
use crate::llm::{self, LlmError, Oracle};
use crate::repo::RepoLookup;
use crate::tokens::{self, Context, Position, TokenModel};

/// Past commands kept to show the language model.
const MAX_PAST: usize = 5000;
/// Past lines of the same command shown to the language model, at most.
const SAME_COMMAND_LINES: usize = 8;
/// Latest lines shown to the language model, for the flow of the session.
const LATEST_LINES: usize = 4;
/// Candidates the language model chooses among, at most.
const MAX_CHOICES: usize = 200;
/// Longest suggestion, in bytes.
const MAX_SUGGESTION: usize = 120;
/// Guesses from the history looked up on disk for one suggestion, at most.
const MAX_LOOKUPS: usize = 32;
/// Longest command name, in bytes, that gets corrected.
const MAX_CORRECTED: usize = 64;
/// How much more a word the history supports weighs when the completion offers
/// it too.
const BACKED_WEIGHT: f64 = 1.5;

/// Tunable parameters of the engine.
#[derive(Debug, Clone, PartialEq)]
pub struct Params {
    /// Parameters of the token model.
    pub tokens: tokens::Params,
    /// Probability a whole next word guessed from the history, with no
    /// completion offering it, must reach to be suggested.
    pub min_next_word: f64,
    /// Probability the rest of a word guessed from the history, with no
    /// completion offering it, must reach to be suggested.
    pub min_rest_of_word: f64,
    /// Probability the language model's own continuation must reach.
    pub min_model: f64,
    /// Probability the language model must give a word the completion offers
    /// to have it suggested when nothing of the word has been typed.
    pub min_choice: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            tokens: tokens::Params::default(),
            min_next_word: 0.4,
            min_rest_of_word: 0.5,
            min_model: 0.5,
            min_choice: 0.25,
        }
    }
}

/// What a word offered by the shell's completion is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A command, a subcommand, an option, or whatever a completion script
    /// lists.
    Word,
    /// A file name.
    File,
}

/// A word the shell's completion offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The word, as it would be inserted.
    pub word: String,
    /// Whether it is a file name or any other word.
    pub kind: Kind,
}

impl Candidate {
    /// A candidate that is not a file name.
    pub fn word(word: impl Into<String>) -> Self {
        Self {
            word: word.into(),
            kind: Kind::Word,
        }
    }

    /// A file name.
    pub fn file(word: impl Into<String>) -> Self {
        Self {
            word: word.into(),
            kind: Kind::File,
        }
    }
}

/// A request for a suggestion.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    /// The line typed so far; the cursor is at its end.
    pub line: &'a str,
    /// The shell's working directory, when known.
    pub cwd: Option<&'a str>,
    /// The shell session typing the line.
    pub session: Option<&'a str>,
    /// Byte offset where the word being typed starts, as the shell's completion
    /// sees it; the engine finds it when `None`.
    pub word_start: Option<usize>,
    /// Words the shell's completion offers for the word being typed.
    pub candidates: &'a [Candidate],
    /// Whether the completion matched the candidates fuzzily rather than by
    /// prefix.
    pub fuzzy: bool,
}

/// A change to the line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// Text to append at the cursor.
    Append(String),
    /// A word to replace the word being typed with.
    Replace(String),
}

/// Where a suggestion came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The token model.
    History,
    /// The shell's completion.
    Completion,
    /// Typo correction.
    Correction,
    /// The language model.
    Model,
}

impl Source {
    /// Short name for logs and reports.
    pub fn name(self) -> &'static str {
        match self {
            Self::History => "history",
            Self::Completion => "completion",
            Self::Correction => "correction",
            Self::Model => "model",
        }
    }
}

/// A suggestion, where it came from, and how likely it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    /// The change to the line.
    pub edit: Edit,
    /// Where it came from.
    pub source: Source,
    /// Estimated probability that it is what comes next.
    pub confidence: f64,
}

/// What to answer: now, or after asking the language model.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// The answer is known.
    Ready(Option<Suggestion>),
    /// The language model decides; `fallback` stands when it cannot answer.
    Ask {
        /// What to ask.
        question: Question,
        /// The answer when the model has none.
        fallback: Option<Suggestion>,
    },
}

/// A question for the language model.
#[derive(Debug, Clone, PartialEq)]
pub enum Question {
    /// Which candidate is the word being typed. The prompt stops where the
    /// word starts: a prompt ending inside a word skews the model, whose tokens
    /// rarely split words where the user paused. When none of the model's
    /// likeliest tokens agrees with a candidate, `inside`, the prompt that ends
    /// with what has been typed of the word, gets a second chance.
    Choose {
        /// The model's prompt, ending with the line up to the word.
        prompt: String,
        /// The model's prompt ending with the whole line, when a part of the
        /// word has been typed.
        inside: Option<String>,
        /// What has been typed of the word.
        partial: String,
        /// Text the continuation starts with before the word: a blank when the
        /// prompt dropped the line's trailing blanks.
        lead: String,
        /// Words to choose from, each starting with `partial`.
        candidates: Vec<String>,
        /// Probability the chosen word must reach.
        min_confidence: f64,
    },
    /// How the line continues.
    Continue {
        /// The model's prompt, ending with the line typed so far.
        prompt: String,
        /// Text the continuation must start with, as in [`Question::Choose`].
        lead: String,
        /// Probability the continuation's first word must reach.
        min_confidence: f64,
    },
}

impl Question {
    /// Whether the answer is the model's own word, which no completion backs,
    /// rather than its choice among valid ones.
    pub fn is_guess(&self) -> bool {
        matches!(self, Self::Continue { .. })
    }

    /// Asks `oracle`; `Ok(None)` when its answer is not usable.
    pub fn ask(&self, oracle: &dyn Oracle) -> Result<Option<Suggestion>, LlmError> {
        match self {
            Self::Choose {
                prompt,
                inside,
                partial,
                lead,
                candidates,
                min_confidence,
            } => {
                let tokens = oracle.next_tokens(prompt)?;
                let mut choice = pick(candidates, &tokens, |candidate| {
                    Some(format!("{lead}{candidate}"))
                });
                if choice.is_none()
                    && let Some(inside) = inside
                {
                    let tokens = oracle.next_tokens(inside)?;
                    choice = pick(candidates, &tokens, |candidate| {
                        candidate
                            .strip_prefix(partial.as_str())
                            .filter(|rest| !rest.is_empty())
                            .map(str::to_owned)
                    });
                }
                Ok(choice
                    .filter(|(_, logprob)| logprob.exp() >= *min_confidence)
                    .and_then(|(word, logprob)| {
                        append(partial, word, Source::Model, logprob.exp())
                    }))
            }
            Self::Continue {
                prompt,
                lead,
                min_confidence,
            } => {
                let continuation = oracle.complete(prompt)?;
                let Some(word) = continuation
                    .text
                    .strip_prefix(lead.as_str())
                    .and_then(first_word)
                else {
                    return Ok(None);
                };
                let confidence = continuation.probability_of_prefix(lead.len() + word.len());
                Ok(
                    (displayable(word) && confidence >= *min_confidence).then(|| Suggestion {
                        edit: Edit::Append(word.to_owned()),
                        source: Source::Model,
                        confidence,
                    }),
                )
            }
        }
    }
}

/// Everything learned from the history, and the rules to suggest from it.
pub struct Engine {
    params: Params,
    tokens: TokenModel,
    commands: Commands,
    past: VecDeque<String>,
    files: Option<Files>,
}

/// Where to look file names up.
struct Files {
    lookup: Box<dyn FileLookup>,
    home: Option<PathBuf>,
}

impl Engine {
    /// An engine with nothing learned yet.
    pub fn new(params: Params, repos: impl RepoLookup + 'static, commands: Commands) -> Self {
        Self {
            tokens: TokenModel::new(params.tokens.clone(), repos),
            params,
            commands,
            past: VecDeque::new(),
            files: None,
        }
    }

    /// Looks up in `lookup` the file names the history or the model come up
    /// with, to never suggest one that is not there; `home` is what `~` stands
    /// for. Without this, as when replaying the history, none is looked up.
    pub fn with_files(mut self, lookup: impl FileLookup + 'static, home: Option<PathBuf>) -> Self {
        self.files = Some(Files {
            lookup: Box::new(lookup),
            home,
        });
        self
    }

    /// Learns from one executed command. Entries must arrive oldest first.
    pub fn push(&mut self, entry: &Entry) {
        self.tokens.push(entry);
        if !entry.command.contains('\n') {
            if self.past.len() == MAX_PAST {
                self.past.pop_front();
            }
            self.past.push_back(entry.command.clone());
        }
    }

    /// Replaces the functions, aliases and builtins the shell defines.
    pub fn set_shell_names(&mut self, names: impl IntoIterator<Item = String>) {
        self.commands.set_shell_names(names);
    }

    /// Decides what to suggest for `request`.
    pub fn plan(&mut self, request: &Request<'_>) -> Plan {
        let line = request.line;
        let start = request
            .word_start
            .filter(|&start| line.is_char_boundary(start))
            .unwrap_or_else(|| lexer::current_word_start(line));
        let (before, partial) = line.split_at(start);
        let position = Position::of(before);
        let ctx = self.tokens.context(request.cwd, request.session);
        // Every valid word, and those worth suggesting on the completion's word
        // alone: not what has been typed already, and no file name until a part
        // of it has been typed.
        let mut completion: HashSet<&str> = HashSet::new();
        let mut proposable: Vec<&str> = Vec::new();
        if !request.fuzzy {
            for candidate in request.candidates {
                let word = without_trailing_blank(&candidate.word);
                if word.is_empty() || !word.starts_with(partial) || !completion.insert(word) {
                    continue;
                }
                if word != partial && (candidate.kind == Kind::Word || !partial.is_empty()) {
                    proposable.push(word);
                }
            }
        }

        // A line that looks like it carries a credential is not shown to the
        // model, as the past ones in its prompt are not.
        let secret = llm::looks_secret(line);

        let known = self.tokens.predict(&position, partial, &ctx);
        if let Some((word, probability)) = self.best_known(&known, &completion, request.cwd) {
            let backed = completion.contains(word);
            let threshold = if partial.is_empty() {
                self.params.min_next_word
            } else {
                self.params.min_rest_of_word
            };
            if backed || probability >= threshold {
                let source = if backed {
                    Source::Completion
                } else {
                    Source::History
                };
                return Plan::Ready(if word == partial {
                    self.next_word(line, &ctx, request.cwd)
                } else {
                    append(partial, word, source, probability)
                });
            }
        }

        match proposable.as_slice() {
            // What has been typed is a whole valid word, and the only one.
            [] if completion.contains(partial) => {
                return Plan::Ready(self.next_word(line, &ctx, request.cwd));
            }
            [] => {}
            [only] => return Plan::Ready(append(partial, only, Source::Completion, 1.0)),
            several => {
                // With nothing typed the shortest candidate predicts nothing;
                // it stands only once a prefix narrows the choice.
                let shortest = several
                    .iter()
                    .min_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)))
                    .filter(|_| !partial.is_empty());
                let even = 1.0 / several.len() as f64;
                let fallback =
                    shortest.and_then(|word| append(partial, word, Source::Completion, even));
                if secret {
                    return Plan::Ready(fallback);
                }
                let recent = self.prompt_lines(position.words.first().copied());
                let (prompt, lead) = model_prompt(request.cwd, &recent, before);
                let inside =
                    (!partial.is_empty()).then(|| model_prompt(request.cwd, &recent, line).0);
                return Plan::Ask {
                    question: Question::Choose {
                        prompt,
                        inside,
                        partial: partial.to_owned(),
                        lead,
                        candidates: several
                            .iter()
                            .take(MAX_CHOICES)
                            .map(|word| (*word).to_owned())
                            .collect(),
                        min_confidence: if partial.is_empty() {
                            self.params.min_choice
                        } else {
                            0.0
                        },
                    },
                    fallback,
                };
            }
        }

        if let Some(word) = self.correction(&position, partial, request) {
            return Plan::Ready(Some(Suggestion {
                edit: Edit::Replace(word),
                source: Source::Correction,
                confidence: 1.0,
            }));
        }

        if secret {
            return Plan::Ready(None);
        }
        let recent = self.prompt_lines(position.words.first().copied());
        let (prompt, lead) = model_prompt(request.cwd, &recent, line);
        Plan::Ask {
            question: Question::Continue {
                prompt,
                lead,
                min_confidence: self.params.min_model,
            },
            fallback: None,
        }
    }

    /// The word after `line`, which ends with a whole word, when it is likely
    /// enough.
    fn next_word(&self, line: &str, ctx: &Context, cwd: Option<&str>) -> Option<Suggestion> {
        let completed = format!("{line} ");
        let next = self.tokens.predict(&Position::of(&completed), "", ctx);
        let &(next, probability) = next
            .iter()
            .take(MAX_LOOKUPS)
            .find(|(word, _)| self.is_there(word, cwd))?;
        let text = format!(" {next}");
        (displayable(&text) && probability >= self.params.min_next_word).then_some(Suggestion {
            edit: Edit::Append(text),
            source: Source::History,
            confidence: probability,
        })
    }

    /// The likeliest word the history supports, with its probability. Words
    /// the completion also offers weigh more: a word it does not offer may not
    /// be valid here. `known` is sorted, most likely first.
    fn best_known<'a>(
        &self,
        known: &[(&'a str, f64)],
        completion: &HashSet<&str>,
        cwd: Option<&str>,
    ) -> Option<(&'a str, f64)> {
        let backed = known
            .iter()
            .find(|(word, _)| completion.contains(word))
            .copied();
        let guessed = known
            .iter()
            .filter(|(word, _)| !completion.contains(word))
            .take(MAX_LOOKUPS)
            .find(|(word, _)| self.is_there(word, cwd))
            .copied();
        match (backed, guessed) {
            (Some(backed), Some(guessed)) if guessed.1 > backed.1 * BACKED_WEIGHT => Some(guessed),
            (backed, guessed) => backed.or(guessed),
        }
    }

    /// Whether `word` may be suggested on a guess: false when it names a file
    /// that is not there. Words that name no file, and every word when no file
    /// lookup was given, pass.
    fn is_there(&self, word: &str, cwd: Option<&str>) -> bool {
        let (Some(files), Some(cwd)) = (&self.files, cwd) else {
            return true;
        };
        files::path_of(word, Path::new(cwd), files.home.as_deref())
            .is_none_or(|path| files.lookup.exists(&path))
    }

    /// What to suggest once the model answered a [`Plan::Ask`] about `line`:
    /// its `answer`, unless it is a guess ([`Question::is_guess`]) naming a
    /// file that is not there, and `fallback` otherwise.
    pub fn settle(
        &self,
        line: &str,
        cwd: Option<&str>,
        is_guess: bool,
        answer: Option<Suggestion>,
        fallback: Option<Suggestion>,
    ) -> Option<Suggestion> {
        answer
            .filter(|answer| !is_guess || self.admits(line, cwd, answer))
            .or(fallback)
    }

    fn admits(&self, line: &str, cwd: Option<&str>, suggestion: &Suggestion) -> bool {
        let Edit::Append(text) = &suggestion.edit else {
            return true;
        };
        let word = match text.strip_prefix([' ', '\t']) {
            Some(next) => next.trim_start_matches([' ', '\t']).to_owned(),
            None => format!("{}{text}", &line[lexer::current_word_start(line)..]),
        };
        self.is_there(&word, cwd)
    }

    /// The closest known command to a command name that matches nothing as
    /// typed. Arguments are never corrected: a word typed for the first time
    /// is usually new, not wrong. Neither is a very long word, which is pasted
    /// rather than mistyped, and costly to compare with every command.
    fn correction(
        &mut self,
        position: &Position<'_>,
        partial: &str,
        request: &Request<'_>,
    ) -> Option<String> {
        if partial.len() > MAX_CORRECTED || !position.is_command() {
            return None;
        }
        let matcher = fuzzy::Matcher::new(partial)?;
        let mut vocabulary = self.tokens.vocabulary(position);
        if request.fuzzy {
            vocabulary.extend(request.candidates.iter().map(|c| c.word.as_str()));
        }
        vocabulary.extend(self.commands.names());
        vocabulary
            .into_iter()
            .filter(|word| *word != partial)
            .filter_map(|word| matcher.distance(word).map(|distance| (word, distance)))
            .min_by(|a, b| {
                a.1.cmp(&b.1)
                    .then_with(|| a.0.len().cmp(&b.0.len()))
                    .then_with(|| a.0.cmp(b.0))
            })
            .map(|(word, _)| word.to_owned())
            .filter(|word| displayable(word))
    }

    /// Past lines shown to the model, oldest first: the latest distinct lines
    /// running `command`, which show how this user runs it, then the latest
    /// lines, which show what the session is doing.
    fn prompt_lines(&self, command: Option<&str>) -> Vec<&str> {
        let mut same: Vec<&str> = Vec::new();
        if let Some(command) = command {
            for line in self.past.iter().rev() {
                if same.len() == SAME_COMMAND_LINES {
                    break;
                }
                if line.split_whitespace().next() == Some(command) && !same.contains(&line.as_str())
                {
                    same.push(line);
                }
            }
        }
        let mut latest: Vec<&str> = self
            .past
            .iter()
            .rev()
            .take(LATEST_LINES)
            .map(String::as_str)
            .filter(|line| !same.contains(line))
            .collect();
        same.reverse();
        latest.reverse();
        same.extend(latest);
        same
    }
}

/// The model's prompt showing the `recent` lines and ending with `text`, and
/// the lead its continuation starts with. Trailing blanks move from the prompt
/// to the lead, since the model's tokens carry the blank before a word.
fn model_prompt(cwd: Option<&str>, recent: &[&str], text: &str) -> (String, String) {
    let mut prompt = llm::prompt(cwd, recent.iter().copied(), text);
    let kept = prompt.trim_end_matches([' ', '\t']).len();
    let lead = if kept < prompt.len() { " " } else { "" };
    prompt.truncate(kept);
    (prompt, lead.to_owned())
}

/// The candidate the model finds likeliest, with that log-probability. A
/// candidate scores the log-probability of the likeliest token that agrees with
/// the text the model would have to produce for it, `expected(candidate)`;
/// ties go to the shorter candidate.
fn pick<'a>(
    candidates: &'a [String],
    tokens: &[(String, f64)],
    expected: impl Fn(&str) -> Option<String>,
) -> Option<(&'a str, f64)> {
    candidates
        .iter()
        .filter_map(|candidate| {
            let expected = expected(candidate)?;
            let score = tokens
                .iter()
                .filter(|(token, _)| {
                    !token.is_empty()
                        && (expected.starts_with(token.as_str()) || token.starts_with(&expected))
                })
                .map(|&(_, logprob)| logprob)
                .fold(f64::NEG_INFINITY, f64::max);
            score.is_finite().then_some((candidate.as_str(), score))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.0.len().cmp(&a.0.len())))
}

/// A completion candidate without the blank some completion scripts append:
/// git's `checkout ` reaches ble.sh's candidates as `checkout\ `.
fn without_trailing_blank(candidate: &str) -> &str {
    let mut word = candidate;
    loop {
        if let Some(rest) = word.strip_suffix("\\ ") {
            word = rest;
        } else if let Some(rest) = word.strip_suffix(' ') {
            word = rest;
        } else {
            return word;
        }
    }
}

fn append(partial: &str, word: &str, source: Source, confidence: f64) -> Option<Suggestion> {
    let rest = word.strip_prefix(partial).filter(|rest| !rest.is_empty())?;
    displayable(rest).then(|| Suggestion {
        edit: Edit::Append(rest.to_owned()),
        source,
        confidence,
    })
}

/// The start of `text` up to the end of its first word; leading blanks are
/// kept, so the result may start the next word.
fn first_word(text: &str) -> Option<&str> {
    let blanks = text.len() - text.trim_start_matches([' ', '\t']).len();
    let word = lexer::tokenize(&text[blanks..]).into_iter().next()?;
    // The lexer skips more than blanks; a word after anything else, such as a
    // newline, is not the continuation of this line.
    (word.start == 0).then(|| &text[..blanks + word.text.len()])
}

/// Whether `text` may be shown as a suggestion. Control characters would corrupt
/// the terminal, and what is accepted must be what was seen: no blank but the
/// space, which a no-break space passes for, and none of the characters that
/// take no room or reorder their neighbours.
fn displayable(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_SUGGESTION
        && !text
            .chars()
            .any(|c| c.is_control() || (c.is_whitespace() && c != ' ') || is_invisible(c))
}

/// Format characters, fillers, variation selectors and tags.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{115F}'..='\u{1160}'
            | '\u{17B4}'..='\u{17B5}'
            | '\u{180B}'..='\u{180F}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0000}'..='\u{E0FFF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Continuation, LlmError};
    use crate::repo::NoRepos;

    /// An oracle with canned answers.
    struct Canned {
        next: Vec<(&'static str, f64)>,
        text: &'static str,
        logprob: f64,
    }

    impl Canned {
        fn continuing(text: &'static str, logprob: f64) -> Self {
            Self {
                next: Vec::new(),
                text,
                logprob,
            }
        }
    }

    impl Oracle for Canned {
        fn next_tokens(&self, _: &str) -> Result<Vec<(String, f64)>, LlmError> {
            Ok(self
                .next
                .iter()
                .map(|&(token, p)| (token.to_owned(), p))
                .collect())
        }

        fn complete(&self, _: &str) -> Result<Continuation, LlmError> {
            Ok(Continuation {
                text: self.text.to_owned(),
                tokens: vec![(self.text.to_owned(), self.logprob)],
            })
        }
    }

    fn engine_with(params: Params, history: &[&str]) -> Engine {
        let mut engine = Engine::new(params, NoRepos, Commands::default());
        for command in history {
            engine.push(&Entry {
                command: (*command).to_owned(),
                cwd: Some("/p".to_owned()),
                session: Some("s".to_owned()),
                exit: Some(0),
                ..Entry::default()
            });
        }
        engine
    }

    fn engine(history: &[&str]) -> Engine {
        engine_with(Params::default(), history)
    }

    fn plan(engine: &mut Engine, line: &str, candidates: &[&str]) -> Plan {
        let candidates: Vec<Candidate> = candidates.iter().copied().map(Candidate::word).collect();
        plan_among(engine, line, &candidates)
    }

    fn plan_files(engine: &mut Engine, line: &str, files: &[&str]) -> Plan {
        let candidates: Vec<Candidate> = files.iter().copied().map(Candidate::file).collect();
        plan_among(engine, line, &candidates)
    }

    fn plan_among(engine: &mut Engine, line: &str, candidates: &[Candidate]) -> Plan {
        engine.plan(&Request {
            line,
            cwd: Some("/p"),
            session: Some("s"),
            word_start: None,
            candidates,
            fuzzy: false,
        })
    }

    /// The files of `/p`.
    struct Listing(&'static [&'static str]);

    impl FileLookup for Listing {
        fn exists(&self, path: &Path) -> bool {
            self.0.iter().any(|name| Path::new("/p").join(name) == path)
        }
    }

    fn choosing(next: Vec<(&'static str, f64)>) -> Canned {
        Canned {
            next,
            text: "",
            logprob: 0.0,
        }
    }

    fn ready(plan: Plan) -> Option<Edit> {
        match plan {
            Plan::Ready(suggestion) => suggestion.map(|s| s.edit),
            Plan::Ask { .. } => panic!("expected an immediate answer, got {plan:?}"),
        }
    }

    fn answer(question: &Question, model: Canned) -> Option<Suggestion> {
        question.ask(&model).expect("an answer")
    }

    fn append_edit(text: &str) -> Option<Edit> {
        Some(Edit::Append(text.to_owned()))
    }

    #[test]
    fn completes_an_installed_command_the_history_prefers() {
        let mut engine = engine(&["docker ps", "docker compose up"]);
        let plan = plan(
            &mut engine,
            "dock",
            &["docker", "docker-compose", "dockerd"],
        );
        assert_eq!(ready(plan), append_edit("er"));
    }

    #[test]
    fn completes_only_the_word_being_typed() {
        let mut engine = engine(&["kubectl get pods -n web", "kubectl get pods -w"]);
        let plan = plan(
            &mut engine,
            "kubectl get po",
            &["pods", "podtemplates", "poddisruptionbudgets"],
        );
        assert_eq!(ready(plan), append_edit("ds"));
    }

    #[test]
    fn suggests_the_next_word_once_the_current_one_is_complete() {
        let mut engine = engine(&["git status", "git status"]);
        assert_eq!(ready(plan(&mut engine, "git", &[])), append_edit(" status"));
        assert_eq!(ready(plan(&mut engine, "git ", &[])), append_edit("status"));
    }

    #[test]
    fn holds_back_unlikely_guesses_that_no_completion_backs() {
        let strict = Params {
            min_next_word: 0.6,
            ..Params::default()
        };
        let mut engine = engine_with(strict, &["git push", "git pull", "git log"]);
        assert!(matches!(
            plan(&mut engine, "git ", &[]),
            Plan::Ask { fallback: None, .. }
        ));
        // The completion backs the same guess: it is a valid option, so it shows.
        let plan = plan(&mut engine, "git ", &["log", "pull", "push"]);
        assert!(matches!(
            plan,
            Plan::Ready(Some(Suggestion {
                source: Source::Completion,
                ..
            }))
        ));
    }

    #[test]
    fn drops_the_blank_completion_scripts_append() {
        assert_eq!(without_trailing_blank("checkout\\ "), "checkout");
        assert_eq!(without_trailing_blank("status "), "status");
        assert_eq!(without_trailing_blank("my\\ file"), "my\\ file");
        let mut engine = engine(&[]);
        assert_eq!(
            ready(plan(&mut engine, "git stas", &["stash\\ "])),
            append_edit("h")
        );
    }

    #[test]
    fn falls_back_to_the_only_completion_without_history() {
        let mut engine = engine(&[]);
        assert_eq!(
            ready(plan(&mut engine, "systemctl stat", &["status"])),
            append_edit("us")
        );
    }

    #[test]
    fn asks_the_model_to_choose_among_completions_without_history() {
        let mut engine = engine(&[]);
        let Plan::Ask { question, fallback } = plan(
            &mut engine,
            "git ch",
            &["checkout", "cherry", "cherry-pick"],
        ) else {
            panic!("expected a question");
        };
        assert_eq!(fallback.map(|s| s.edit), append_edit("erry"));
        let Question::Choose { prompt, lead, .. } = &question else {
            panic!("expected a choice");
        };
        assert!(
            prompt.ends_with("$ git"),
            "the prompt stops before the word: {prompt:?}"
        );
        assert_eq!(lead, " ");
        let model = Canned {
            next: vec![(" check", -0.2), (" cherry", -1.5)],
            text: "",
            logprob: 0.0,
        };
        assert_eq!(
            question.ask(&model).expect("an answer").map(|s| s.edit),
            append_edit("eckout")
        );
    }

    #[test]
    fn suggests_a_longer_word_when_what_was_typed_is_valid_too() {
        // cargo offers its alias `t` along with `test` and `tree`.
        let mut engine = engine(&[]);
        let Plan::Ask { question, fallback } = plan(&mut engine, "cargo t", &["t", "test", "tree"])
        else {
            panic!("expected a question");
        };
        assert_eq!(fallback.map(|s| s.edit), append_edit("est"));
        let Question::Choose { candidates, .. } = &question else {
            panic!("expected a choice");
        };
        assert_eq!(candidates, &["test", "tree"]);
        assert_eq!(
            answer(&question, choosing(vec![(" tree", -0.1)])).map(|s| s.edit),
            append_edit("ree")
        );
    }

    #[test]
    fn leaves_a_whole_valid_word_alone() {
        let mut engine = engine(&[]);
        // "cargo" is a prefix of "cargos", which a correction would offer.
        engine.set_shell_names(["cargos".to_owned()]);
        assert_eq!(ready(plan(&mut engine, "cargo", &["cargo"])), None);
    }

    #[test]
    fn suggests_the_only_valid_word_outright() {
        let mut engine = engine(&[]);
        assert_eq!(
            ready(plan(&mut engine, "git checkout ", &["main"])),
            append_edit("main")
        );
    }

    #[test]
    fn shows_the_model_no_line_that_looks_like_a_credential() {
        let mut engine = engine(&[]);
        let line = "curl -H 'Authorization: Bearer abc' https://api.example.com/";
        assert_eq!(ready(plan(&mut engine, line, &[])), None);
        // The completion still narrows a typed prefix down, without the model.
        let line = "mytool --token abc sta";
        assert_eq!(
            ready(plan(&mut engine, line, &["start", "status"])),
            append_edit("rt")
        );
    }

    #[test]
    fn corrects_no_pasted_blob() {
        let mut engine = engine(&[]);
        let blob = "docker".repeat(20);
        engine.set_shell_names([format!("{blob}x")]);
        assert!(matches!(
            plan(&mut engine, &blob, &[]),
            Plan::Ask { fallback: None, .. }
        ));
    }

    #[test]
    fn guesses_no_word_before_any_of_it_is_typed() {
        let mut engine = engine(&[]);
        let Plan::Ask { question, fallback } = plan(&mut engine, "git ", &["am", "status"]) else {
            panic!("expected a question");
        };
        assert_eq!(fallback, None, "the shortest valid word is no prediction");
        assert_eq!(answer(&question, choosing(vec![(" status", -2.5)])), None);
        assert_eq!(
            answer(&question, choosing(vec![(" status", -0.3)])).map(|s| s.edit),
            append_edit("status")
        );
    }

    #[test]
    fn lists_no_directory_for_a_command_it_knows_nothing_about() {
        let mut engine = engine(&[]);
        let files = [".git", "Cargo.toml"];
        assert!(matches!(
            plan_files(&mut engine, "kubectl get ", &files),
            Plan::Ask {
                question: Question::Continue { .. },
                fallback: None,
            }
        ));
        assert!(matches!(
            plan_files(&mut engine, "cat ", &[".git"]),
            Plan::Ask {
                question: Question::Continue { .. },
                fallback: None,
            }
        ));
    }

    #[test]
    fn suggests_files_the_history_points_at_or_a_prefix_selects() {
        let mut reader = engine(&["cat Cargo.toml", "cat Cargo.toml"]);
        let files = [".git", "Cargo.lock", "Cargo.toml"];
        assert_eq!(
            ready(plan_files(&mut reader, "cat ", &files)),
            append_edit("Cargo.toml")
        );
        // less never took Cargo.toml, but cat did, and a prefix was typed.
        assert_eq!(
            ready(plan_files(&mut reader, "less Carg", &files)),
            append_edit("o.toml")
        );
        assert_eq!(
            ready(plan_files(&mut reader, "less .g", &files)),
            append_edit("it")
        );
        // With no history for the word, the model chooses; the shortest
        // candidate stands in.
        let mut unrelated = engine(&["git status"]);
        let Plan::Ask { fallback, .. } = plan_files(&mut unrelated, "less Carg", &files) else {
            panic!("expected a question");
        };
        assert_eq!(fallback.map(|s| s.edit), append_edit("o.lock"));
    }

    #[test]
    fn never_guesses_a_file_that_is_not_there() {
        let history = ["cat status.txt", "cat status.txt", "cat status.txt"];
        let mut anywhere = engine(&history);
        assert_eq!(
            ready(plan(&mut anywhere, "cat s", &[])),
            append_edit("tatus.txt")
        );

        let mut elsewhere = engine(&history).with_files(Listing(&["notes.md"]), None);
        assert!(matches!(
            plan(&mut elsewhere, "cat s", &[]),
            Plan::Ask { fallback: None, .. }
        ));
        assert_eq!(ready(plan(&mut elsewhere, "cat", &[])), None);

        let mut here = engine(&history).with_files(Listing(&["status.txt"]), None);
        assert_eq!(
            ready(plan(&mut here, "cat s", &[])),
            append_edit("tatus.txt")
        );
        assert_eq!(
            ready(plan(&mut here, "cat", &[])),
            append_edit(" status.txt")
        );
    }

    #[test]
    fn still_guesses_words_that_name_no_file() {
        let mut engine =
            engine(&["kubectl get pods", "kubectl get pods"]).with_files(Listing(&[]), None);
        assert_eq!(
            ready(plan_files(
                &mut engine,
                "kubectl get ",
                &[".git", "Cargo.toml"]
            )),
            append_edit("pods")
        );
    }

    #[test]
    fn checks_the_files_the_model_comes_up_with() {
        let engine = engine(&[]).with_files(Listing(&["notes.md"]), None);
        let guess = |text: &str| Suggestion {
            edit: Edit::Append(text.to_owned()),
            source: Source::Model,
            confidence: 0.9,
        };
        assert!(!engine.admits("cat ", Some("/p"), &guess("ghost.txt")));
        assert!(!engine.admits("cat", Some("/p"), &guess(" ghost.txt")));
        assert!(!engine.admits("cat gh", Some("/p"), &guess("ost.txt")));
        assert!(engine.admits("cat no", Some("/p"), &guess("tes.md")));
        assert!(engine.admits("tar -x", Some("/p"), &guess("vf")));
    }

    #[test]
    fn asks_again_inside_the_word_when_no_candidate_is_likely() {
        let mut engine = engine(&[]);
        let Plan::Ask { question, .. } = plan(&mut engine, "git ch", &["checkout", "cherry"])
        else {
            panic!("expected a question");
        };
        // First stage: nothing agrees with " checkout" or " cherry"; second
        // stage, after "git ch": "eck" does.
        struct TwoStages;
        impl Oracle for TwoStages {
            fn next_tokens(&self, prompt: &str) -> Result<Vec<(String, f64)>, LlmError> {
                let token = if prompt.ends_with("git ch") {
                    "eck"
                } else {
                    " status"
                };
                Ok(vec![(token.to_owned(), -0.5)])
            }
            fn complete(&self, _: &str) -> Result<Continuation, LlmError> {
                Ok(Continuation::default())
            }
        }
        let suggestion = question.ask(&TwoStages).expect("an answer");
        assert_eq!(suggestion.map(|s| s.edit), append_edit("eckout"));
    }

    #[test]
    fn shows_the_model_how_this_user_runs_the_command() {
        let mut engine = engine(&[
            "git log -1",
            "ls",
            "git status",
            "cargo test",
            "pwd",
            "ls",
            "make",
            "du",
        ]);
        let Plan::Ask { question, .. } = plan(&mut engine, "git ch", &["checkout", "cherry"])
        else {
            panic!("expected a question");
        };
        let Question::Choose { prompt, .. } = question else {
            panic!("expected a choice");
        };
        let lines: Vec<&str> = prompt.lines().skip(1).collect();
        assert_eq!(
            lines,
            [
                "$ git log -1",
                "$ git status",
                "$ pwd",
                "$ ls",
                "$ make",
                "$ du",
                "$ git"
            ]
        );
    }

    #[test]
    fn scores_the_next_word_with_its_leading_blank() {
        let candidates = ["pods".to_owned(), "deployments".to_owned()];
        let tokens = [(" deploy".to_owned(), -0.3), (" pods".to_owned(), -0.9)];
        let chosen = pick(&candidates, &tokens, |candidate| {
            Some(format!(" {candidate}"))
        });
        assert_eq!(chosen, Some(("deployments", -0.3)));
    }

    #[test]
    fn corrects_a_command_name_that_matches_nothing() {
        let mut engine = engine(&[]);
        engine.set_shell_names([
            "bleopt".to_owned(),
            "ble-bind".to_owned(),
            "docker".to_owned(),
        ]);
        assert_eq!(
            ready(plan(&mut engine, "ble-o", &[])),
            Some(Edit::Replace("bleopt".to_owned()))
        );
        assert_eq!(
            ready(plan(&mut engine, "dokcer", &[])),
            Some(Edit::Replace("docker".to_owned()))
        );
    }

    #[test]
    fn never_corrects_arguments_or_short_words() {
        let mut engine = engine(&["scp notes.txt host:"]);
        engine.set_shell_names(["git".to_owned()]);
        assert!(matches!(
            plan(&mut engine, "scp notes.tx2", &[]),
            Plan::Ask { fallback: None, .. }
        ));
        assert!(matches!(
            plan(&mut engine, "gti", &[]),
            Plan::Ask { fallback: None, .. }
        ));
    }

    #[test]
    fn asks_the_model_to_continue_when_nothing_is_known() {
        let mut engine = engine(&[]);
        let Plan::Ask { question, fallback } = plan(&mut engine, "tar -x", &[]) else {
            panic!("expected a question");
        };
        assert_eq!(fallback, None);
        let model = Canned::continuing("vf backup.tar.gz", -0.1);
        assert_eq!(
            question.ask(&model).expect("an answer").map(|s| s.edit),
            append_edit("vf")
        );

        let Plan::Ask { question, .. } = plan(&mut engine, "ffmpeg -c:v ", &[]) else {
            panic!("expected a question");
        };
        let model = Canned::continuing(" libx264 -c:a aac", -0.1);
        assert_eq!(
            question.ask(&model).expect("an answer").map(|s| s.edit),
            append_edit("libx264")
        );
    }

    #[test]
    fn holds_back_an_unlikely_model_continuation() {
        let strict = Params {
            min_model: 0.5,
            ..Params::default()
        };
        let mut engine = engine_with(strict, &[]);
        let Plan::Ask { question, .. } = plan(&mut engine, "tar -x", &[]) else {
            panic!("expected a question");
        };
        assert_eq!(
            answer(&question, Canned::continuing("vf a.tar", -2.0)),
            None
        );
        assert!(answer(&question, Canned::continuing("vf a.tar", -0.2)).is_some());
    }

    #[test]
    fn shows_nothing_that_is_not_what_it_looks_like() {
        for disguised in [
            "rm\u{00A0}-rf",
            "ma\u{00AD}in",
            "main\u{206A}",
            "main\u{3164}",
            "main\u{E0021}",
            "ma\u{200B}in",
            "\u{202E}niam",
            "a\tb",
        ] {
            assert!(!displayable(disguised), "{disguised:?}");
        }
        assert!(displayable("my file-v1.2_final.tar.gz"));
        assert!(displayable("pão"));
    }

    #[test]
    fn takes_no_word_from_another_line_of_the_model() {
        assert_eq!(first_word("\né"), None);
        assert_eq!(first_word(" pods -n web"), Some(" pods"));
    }

    #[test]
    fn rejects_model_output_that_does_not_fit_or_is_unsafe() {
        let mut engine = engine(&[]);
        let Plan::Ask { question, .. } = plan(&mut engine, "ffmpeg -c:v ", &[]) else {
            panic!("expected a question");
        };
        assert_eq!(
            answer(&question, Canned::continuing("libx264", -0.1)),
            None,
            "a continuation must start after the typed blank"
        );
        assert_eq!(
            answer(&question, Canned::continuing(" \u{1b}[31mred", -0.1)),
            None
        );
    }
}
