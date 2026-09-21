//! Offline evaluation: replays the history and measures, at every position of
//! every command, how often each strategy suggests something and how often the
//! suggestion is exactly what was typed next.
//!
//! Commands are replayed in order, against models that only know the commands
//! before them. Two measurements are taken for every command:
//!
//! - after every character, whether each strategy suggests something
//!   (coverage) and whether it is right (accuracy). Every strategy is measured
//!   at the same positions, so their numbers compare directly;
//! - typing the command with a user who accepts every right suggestion with one
//!   keystroke, the keystrokes saved.
//!
//! A suggestion is right when it is exactly what was typed next, up to the end
//! of a word. ble.sh's history source suggests whole lines; it is scored on the
//! part up to the end of the first word, which is what accepting word by word
//! (`M-f`) takes.
//!
//! The replay runs without a shell, so it cannot ask the shell's completion.
//! When given the installed [`Commands`], it offers them where a command name is
//! typed, as the completion does: to augur as candidates, and to ble.sh as the
//! first candidate its completion source suggests when no history line
//! matches. Arguments get no completion.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::time::{Duration, Instant};

use crate::commands::Commands;
use crate::history::Entry;
use crate::lexer::{self, LexState};
use crate::llm::Oracle;
use crate::repo::RepoLookup;
use crate::suggest::{self, Candidate, Edit, Engine, Plan, Request};
use crate::tokens::Position;

/// A way of producing suggestions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// ble.sh's history source: the rest of the most recent command that starts
    /// with the line, up to the end of the word.
    BleHistory,
    /// augur without the language model.
    Augur,
    /// augur with the language model.
    AugurModel,
}

impl Strategy {
    /// Short name for reports.
    pub fn label(self) -> &'static str {
        match self {
            Self::BleHistory => "ble.sh history",
            Self::Augur => "augur",
            Self::AugurModel => "augur + model",
        }
    }
}

/// What one strategy achieved over a replay.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metrics {
    /// Commands replayed.
    pub commands: u64,
    /// Characters in those commands: the keystrokes needed without help.
    pub characters: u64,
    /// Positions measured, one after each typed character but the last.
    pub positions: u64,
    /// Positions where a suggestion was shown.
    pub shown: u64,
    /// Positions where the suggestion was exactly what came next.
    pub correct: u64,
    /// Keystrokes a user accepting every right suggestion needed.
    pub keystrokes: u64,
    /// Suggestions computed.
    pub queries: u64,
    /// Time spent computing them.
    pub elapsed: Duration,
}

impl Metrics {
    /// Share of positions with a suggestion.
    pub fn coverage(&self) -> f64 {
        ratio(self.shown, self.positions)
    }

    /// Share of shown suggestions that were right.
    pub fn accuracy(&self) -> f64 {
        ratio(self.correct, self.shown)
    }

    /// Share of positions with a right suggestion.
    pub fn useful(&self) -> f64 {
        ratio(self.correct, self.positions)
    }

    /// Share of keystrokes saved compared to typing every character.
    pub fn savings(&self) -> f64 {
        1.0 - ratio(self.keystrokes, self.characters)
    }

    /// Mean time to compute one suggestion.
    pub fn mean_latency(&self) -> Duration {
        u32::try_from(self.queries)
            .ok()
            .filter(|&queries| queries > 0)
            .map_or(Duration::ZERO, |queries| self.elapsed / queries)
    }
}

/// Replays `entries` and measures every strategy on the last `window` commands
/// (all of them when `None`); earlier commands only train the models.
/// [`Strategy::AugurModel`] needs `oracle`, and behaves as [`Strategy::Augur`]
/// without it. `commands` stands in for the completion of command names.
pub fn replay(
    entries: &[Entry],
    params: &suggest::Params,
    strategies: &[Strategy],
    repos: impl RepoLookup + 'static,
    oracle: Option<&dyn Oracle>,
    window: Option<usize>,
    mut commands: Option<Commands>,
) -> Vec<Metrics> {
    let mut engine = Engine::new(params.clone(), repos, Commands::default());
    let mut lines = Lines::default();
    let mut metrics = vec![Metrics::default(); strategies.len()];
    let first_measured = window.map_or(0, |window| entries.len().saturating_sub(window));
    for (index, entry) in entries.iter().enumerate() {
        if index >= first_measured && !entry.command.contains('\n') {
            for (&strategy, m) in strategies.iter().zip(&mut metrics) {
                let mut suggest = |line: &str| match strategy {
                    Strategy::BleHistory => lines
                        .most_recent_extension(line)
                        .map(|rest| up_to_word_end(line, rest).to_owned())
                        .or_else(|| first_completion(&mut commands, line))
                        .map(Edit::Append),
                    Strategy::Augur | Strategy::AugurModel => {
                        let candidates: Vec<Candidate> = command_candidates(&mut commands, line)
                            .into_iter()
                            .map(Candidate::word)
                            .collect();
                        let request = Request {
                            line,
                            cwd: entry.cwd.as_deref(),
                            session: entry.session.as_deref(),
                            word_start: None,
                            candidates: &candidates,
                            fuzzy: false,
                        };
                        match engine.plan(&request) {
                            Plan::Ready(suggestion) => suggestion.map(|s| s.edit),
                            Plan::Ask { question, fallback } => {
                                let answer = match (strategy, oracle) {
                                    (Strategy::AugurModel, Some(oracle)) => {
                                        question.ask(oracle).ok().flatten()
                                    }
                                    _ => None,
                                };
                                answer.or(fallback).map(|s| s.edit)
                            }
                        }
                    }
                };
                measure(&entry.command, &mut suggest, m);
            }
        }
        engine.push(entry);
        lines.push(&entry.command);
    }
    metrics
}

/// The installed commands that start with the word typed, when it is a command
/// name, sorted as the shell's completion lists them.
fn command_candidates(commands: &mut Option<Commands>, line: &str) -> Vec<String> {
    let Some(commands) = commands else {
        return Vec::new();
    };
    let (before, partial) = line.split_at(lexer::current_word_start(line));
    if partial.is_empty() || !Position::of(before).is_command() {
        return Vec::new();
    }
    let mut names: Vec<String> = commands
        .names()
        .filter(|name| name.starts_with(partial))
        .map(str::to_owned)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// What ble.sh's completion source suggests: the rest of the first candidate.
fn first_completion(commands: &mut Option<Commands>, line: &str) -> Option<String> {
    let partial = &line[lexer::current_word_start(line)..];
    let first = command_candidates(commands, line).into_iter().next()?;
    first
        .strip_prefix(partial)
        .filter(|rest| !rest.is_empty())
        .map(str::to_owned)
}

fn measure(command: &str, suggest: &mut dyn FnMut(&str) -> Option<Edit>, m: &mut Metrics) {
    m.commands += 1;
    m.characters += command.chars().count() as u64;

    let mut timed = |line: &str, m: &mut Metrics| {
        let started = Instant::now();
        let edit = suggest(line);
        m.elapsed += started.elapsed();
        m.queries += 1;
        edit
    };

    for (offset, _) in command.char_indices().skip(1) {
        m.positions += 1;
        if let Some(edit) = timed(&command[..offset], m) {
            m.shown += 1;
            if accepted(command, offset, &edit).is_some() {
                m.correct += 1;
            }
        }
    }

    let mut typed = 0;
    while let Some(c) = command[typed..].chars().next() {
        typed += c.len_utf8();
        m.keystrokes += 1;
        if typed == command.len() {
            break;
        }
        if let Some(edit) = timed(&command[..typed], m)
            && let Some(end) = accepted(command, typed, &edit)
        {
            m.keystrokes += 1;
            typed = end;
        }
    }
}

/// Where the cursor ends up when `edit`, suggested with `command[..offset]`
/// typed, is exactly what came next up to the end of a word.
fn accepted(command: &str, offset: usize, edit: &Edit) -> Option<usize> {
    let (start, text) = match edit {
        Edit::Append(text) => (offset, text),
        Edit::Replace(word) => (lexer::current_word_start(&command[..offset]), word),
    };
    let end = start + text.len();
    (command.get(start..end) == Some(text.as_str()) && end > offset && ends_word(command, end))
        .then_some(end)
}

/// Whether a word of `command` ends at byte `end`.
fn ends_word(command: &str, end: usize) -> bool {
    match command[end..].chars().next() {
        None => true,
        Some(next) => LexState::of(&command[..end]).ends_token_before(next),
    }
}

/// The start of `suggestion`, which continues `line`, up to the end of its
/// first word.
fn up_to_word_end<'a>(line: &str, suggestion: &'a str) -> &'a str {
    let mut state = LexState::of(line);
    let mut in_word = false;
    for (i, c) in suggestion.char_indices() {
        if in_word && state.ends_token_before(c) {
            return &suggestion[..i];
        }
        state.feed(c);
        in_word |= !c.is_whitespace();
    }
    suggestion
}

/// The lines seen so far, for ble.sh's history source.
#[derive(Debug, Default)]
struct Lines {
    last_seen: BTreeMap<String, u64>,
    seq: u64,
}

impl Lines {
    fn push(&mut self, line: &str) {
        self.last_seen.insert(line.to_owned(), self.seq);
        self.seq += 1;
    }

    /// The rest of the most recent line that extends `prefix`.
    fn most_recent_extension(&self, prefix: &str) -> Option<&str> {
        self.last_seen
            .range::<str, _>((Bound::Included(prefix), Bound::Unbounded))
            .take_while(|(line, _)| line.starts_with(prefix))
            .filter(|(line, _)| line.len() > prefix.len())
            .max_by_key(|&(_, &seq)| seq)
            .map(|(line, _)| &line[prefix.len()..])
    }
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::NoRepos;

    fn history(commands: &[&str]) -> Vec<Entry> {
        commands
            .iter()
            .map(|&command| Entry {
                command: command.to_owned(),
                cwd: Some("/p".to_owned()),
                session: Some("s".to_owned()),
                exit: Some(0),
                ..Entry::default()
            })
            .collect()
    }

    fn run(commands: &[&str], strategy: Strategy) -> Metrics {
        let entries = history(commands);
        let mut metrics = replay(
            &entries,
            &suggest::Params::default(),
            &[strategy],
            NoRepos,
            None,
            None,
            None,
        );
        metrics.remove(0)
    }

    #[test]
    fn scores_suggestions_word_by_word() {
        let m = run(&["git status", "git status"], Strategy::Augur);
        // Second command, after each of "g", "gi", "git", "git ", "git s", …:
        // "it", "t", " status", "status", "tatus", … are all right.
        assert_eq!((m.commands, m.characters, m.positions), (2, 20, 18));
        assert_eq!((m.shown, m.correct), (9, 9));
        // "g", → for "it", then " " (ble.sh suggests again only after a typed
        // character), → for "status".
        assert_eq!(m.keystrokes, 10 + 4);
    }

    #[test]
    fn scores_ble_sh_on_the_first_word_of_its_line() {
        let m = run(
            &["git push origin main", "git push origin dev"],
            Strategy::BleHistory,
        );
        // Right word by word until " main" and "main", suggested after
        // "git push origin" and "git push origin ": two wrong suggestions.
        assert_eq!(m.shown, m.correct + 2);
    }

    #[test]
    fn a_right_suggestion_ends_at_a_word_boundary() {
        assert_eq!(
            accepted("git status", 5, &Edit::Append("tatus".to_owned())),
            Some(10)
        );
        assert_eq!(
            accepted("git status", 5, &Edit::Append("tat".to_owned())),
            None
        );
        assert_eq!(
            accepted("git status -s", 3, &Edit::Append(" status".to_owned())),
            Some(10)
        );
        assert_eq!(
            accepted("bleopt x", 5, &Edit::Replace("bleopt".to_owned())),
            Some(6)
        );
    }

    #[test]
    fn truncates_a_line_suggestion_to_its_first_word() {
        assert_eq!(up_to_word_end("git sta", "tus --short"), "tus");
        assert_eq!(up_to_word_end("ls", " -la /tmp"), " -la");
        assert_eq!(
            up_to_word_end("git commit -m \"fix", " the bug\" -q"),
            " the bug\""
        );
    }
}
