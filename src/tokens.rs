//! The token model: predicts the word being typed from the words before it.
//!
//! Past commands are split into simple commands (at `|`, `&&`, `;`…) and those
//! into words. For every word the model counts the contexts it appeared in,
//! from the most specific (the exact words before it) to the most general (any
//! argument of the same command). To predict, it looks the same contexts up for
//! the line being typed, keeps the words that start with what has been typed of
//! the current one, and mixes the distributions of the contexts that have any.
//! A line never typed before still gets predictions from the general contexts:
//! in `kubectl -n x get po`, `pods` comes from what followed `get` in other
//! kubectl commands.
//!
//! | context     | an argument appeared…                                |
//! |-------------|------------------------------------------------------|
//! | `prefix`    | after exactly the same words                         |
//! | `dir_tail`  | after the same command and word, in this directory   |
//! | `repo_tail` | after the same command and word, in this repository  |
//! | `tail2`     | after the same command and last two words            |
//! | `tail`      | after the same command and last word                 |
//! | `after`     | after the same last word, in any command             |
//! | `args`      | anywhere among the same command's arguments          |
//!
//! A command name is predicted from the commands run in this directory
//! (`dir_start`) or repository (`repo_start`), the commands that followed the
//! previous one (`follows`: across `|`, `&&` or consecutive lines of a
//! session), and all commands (`start`).

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;

use crate::history::Entry;
use crate::lexer::{self, Token};
use crate::repo::RepoLookup;

/// Words learned from one simple command, at most. The exact words before each
/// one are hashed again for every word, so a pasted line of thousands of words
/// would cost seconds at every start of the engine, and nobody types its end.
const MAX_LEARNED_WORDS: usize = 64;

/// Mixture weight of each context described in the [module documentation](self).
#[derive(Debug, Clone, PartialEq)]
pub struct Weights {
    /// Weight of `prefix`.
    pub prefix: f64,
    /// Weight of `dir_tail`.
    pub dir_tail: f64,
    /// Weight of `repo_tail`.
    pub repo_tail: f64,
    /// Weight of `tail2`.
    pub tail2: f64,
    /// Weight of `tail`.
    pub tail: f64,
    /// Weight of `after`.
    pub after: f64,
    /// Weight of `args`.
    pub args: f64,
    /// Weight of `dir_start`.
    pub dir_start: f64,
    /// Weight of `repo_start`.
    pub repo_start: f64,
    /// Weight of `follows`.
    pub follows: f64,
    /// Weight of `start`.
    pub start: f64,
}

/// Tunable parameters of the token model.
#[derive(Debug, Clone, PartialEq)]
pub struct Params {
    /// Mixture weights of the contexts.
    pub weights: Weights,
    /// Commands after which the recency bonus of a word halves.
    pub half_life: f64,
    /// How much a failed run counts, relative to a successful one.
    pub failure_weight: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            weights: Weights {
                prefix: 4.0,
                dir_tail: 2.0,
                repo_tail: 1.0,
                tail2: 2.0,
                tail: 1.5,
                after: 0.3,
                args: 0.5,
                dir_start: 2.0,
                repo_start: 1.0,
                follows: 1.5,
                start: 1.0,
            },
            half_life: 200.0,
            failure_weight: 0.25,
        }
    }
}

/// Where a line is being typed, resolved by [`TokenModel::context`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Context {
    dir: Option<usize>,
    repo: Option<usize>,
    previous: Option<usize>,
}

/// The words of the simple command being typed, split from the text before the
/// current word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position<'a> {
    /// Words of the current simple command before the current word, leading
    /// variable assignments left out.
    pub words: Vec<&'a str>,
    /// The separator before the current simple command (`|`, `&&`…) and the
    /// command it follows, when the simple command is not the first of the line.
    pub follows: Option<(&'a str, &'a str)>,
}

impl<'a> Position<'a> {
    /// Splits `before`, the text before the word being typed.
    pub fn of(before: &'a str) -> Self {
        let tokens = lexer::tokenize(before);
        let mut commands = simple_commands(&tokens);
        let (_, words) = commands.pop().unwrap_or_default();
        let follows = commands.last().and_then(|(_, previous)| {
            let separator = tokens
                .iter()
                .rev()
                .find(|token| token.is_separator())
                .map(|token| token.text)?;
            Some((separator, *previous.first()?))
        });
        Self { words, follows }
    }

    /// Whether the word being typed is a command name.
    pub fn is_command(&self) -> bool {
        self.words.is_empty()
    }
}

/// Predicts words from the contexts they appeared in.
pub struct TokenModel {
    params: Params,
    repos: Box<dyn RepoLookup>,
    words: Interner,
    dirs: Interner,
    repo_roots: Interner,
    dir_repo: HashMap<usize, Option<usize>>,
    sessions: Interner,
    last_command: HashMap<usize, usize>,
    table: HashMap<Key, HashMap<usize, Stat>>,
    seq: u64,
}

impl TokenModel {
    /// An empty model that finds repositories with `repos`.
    pub fn new(params: Params, repos: impl RepoLookup + 'static) -> Self {
        Self {
            params,
            repos: Box::new(repos),
            words: Interner::default(),
            dirs: Interner::default(),
            repo_roots: Interner::default(),
            dir_repo: HashMap::new(),
            sessions: Interner::default(),
            last_command: HashMap::new(),
            table: HashMap::new(),
            seq: 0,
        }
    }

    /// Learns from one executed command. Entries must arrive oldest first.
    pub fn push(&mut self, entry: &Entry) {
        let seq = self.seq;
        self.seq += 1;
        if entry.command.contains('\n') {
            return;
        }
        let weight = match entry.exit {
            Some(status) if !succeeded(status) => self.params.failure_weight,
            _ => 1.0,
        };
        let dir = entry.cwd.as_deref().map(|cwd| self.dirs.intern(cwd));
        let repo = dir.and_then(|dir| self.repo_of(dir));
        let session = entry.session.as_deref().map(|s| self.sessions.intern(s));
        let line_start = self.words.intern(LINE_START);

        let tokens = lexer::tokenize(&entry.command);
        let mut follows = session
            .and_then(|s| self.last_command.get(&s).copied())
            .map(|previous| (line_start, previous));
        let mut last_command = None;
        for (separator, words) in simple_commands(&tokens) {
            if let Some(separator) = separator {
                follows = Some(self.words.intern(separator)).zip(last_command);
            }
            if words.is_empty() {
                continue;
            }
            let ids: Vec<usize> = words
                .iter()
                .take(MAX_LEARNED_WORDS)
                .map(|word| self.words.intern(word))
                .collect();
            let place = Place { dir, repo, follows };
            for i in 0..ids.len() {
                for key in keys(&ids[..i], &place) {
                    let stat = self
                        .table
                        .entry(key)
                        .or_default()
                        .entry(ids[i])
                        .or_default();
                    stat.count += weight;
                    stat.last = seq;
                }
            }
            last_command = Some(ids[0]);
        }
        if let Some((session, command)) = session.zip(last_command) {
            self.last_command.insert(session, command);
        }
    }

    /// Resolves where a line is being typed: in directory `cwd`, by `session`.
    pub fn context(&mut self, cwd: Option<&str>, session: Option<&str>) -> Context {
        let dir = cwd.map(|cwd| self.dirs.intern(cwd));
        let repo = dir.and_then(|dir| self.repo_of(dir));
        let previous = session
            .and_then(|s| self.sessions.get(s))
            .and_then(|s| self.last_command.get(&s).copied());
        Context {
            dir,
            repo,
            previous,
        }
    }

    /// Words that may come at `position` and start with `partial`, with their
    /// probabilities, most likely first.
    pub fn predict(
        &self,
        position: &Position<'_>,
        partial: &str,
        ctx: &Context,
    ) -> Vec<(&str, f64)> {
        let place = self.place(position, ctx);
        let ids = self.ids(&position.words);
        let mut scores: HashMap<usize, f64> = HashMap::new();
        let mut total_weight = 0.0;
        for key in keys(&ids, &place) {
            let Some(counts) = self.table.get(&key) else {
                continue;
            };
            let matching: Vec<(usize, f64)> = counts
                .iter()
                .filter(|&(&word, _)| self.words.name(word).starts_with(partial))
                .map(|(&word, stat)| (word, self.strength(stat)))
                .collect();
            let mass: f64 = matching.iter().map(|&(_, strength)| strength).sum();
            if mass <= 0.0 {
                continue;
            }
            let weight = self.weight(&key);
            for (word, strength) in matching {
                *scores.entry(word).or_default() += weight * strength / mass;
            }
            total_weight += weight;
        }
        let mut ranked: Vec<(&str, f64)> = scores
            .into_iter()
            .map(|(word, score)| (self.words.name(word), score / total_weight))
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        ranked
    }

    /// Every word seen in the most general context of `position`: all command
    /// names, or all arguments of the command.
    pub fn vocabulary(&self, position: &Position<'_>) -> Vec<&str> {
        let key = match position.words.first() {
            None => Key::Start,
            Some(command) => match self.words.get(command) {
                Some(command) => Key::Args(command),
                None => return Vec::new(),
            },
        };
        self.table
            .get(&key)
            .map(|counts| counts.keys().map(|&word| self.words.name(word)).collect())
            .unwrap_or_default()
    }

    fn place(&self, position: &Position<'_>, ctx: &Context) -> Place {
        let follows = match position.follows {
            Some((separator, previous)) => self.words.get(separator).zip(self.words.get(previous)),
            None => self.words.get(LINE_START).zip(ctx.previous),
        };
        Place {
            dir: ctx.dir,
            repo: ctx.repo,
            follows,
        }
    }

    /// Ids of `words`; a word never seen gets an id no context contains.
    fn ids(&self, words: &[&str]) -> Vec<usize> {
        words
            .iter()
            .map(|word| self.words.get(word).unwrap_or(usize::MAX))
            .collect()
    }

    fn strength(&self, stat: &Stat) -> f64 {
        let age = self.seq.saturating_sub(stat.last + 1) as f64;
        stat.count * (1.0 + (-age / self.params.half_life).exp2())
    }

    fn weight(&self, key: &Key) -> f64 {
        let w = &self.params.weights;
        match key {
            Key::Prefix(_) => w.prefix,
            Key::DirTail(..) => w.dir_tail,
            Key::RepoTail(..) => w.repo_tail,
            Key::Tail2(..) => w.tail2,
            Key::Tail(..) => w.tail,
            Key::After(_) => w.after,
            Key::Args(_) => w.args,
            Key::DirStart(_) => w.dir_start,
            Key::RepoStart(_) => w.repo_start,
            Key::Follows(..) => w.follows,
            Key::Start => w.start,
        }
    }

    fn repo_of(&mut self, dir: usize) -> Option<usize> {
        if let Some(&repo) = self.dir_repo.get(&dir) {
            return repo;
        }
        let root = self.repos.repo_root(Path::new(self.dirs.name(dir)));
        let repo = root.map(|root| self.repo_roots.intern(&root.to_string_lossy()));
        self.dir_repo.insert(dir, repo);
        repo
    }
}

/// Pseudo-word standing for the start of a line in `follows` contexts.
const LINE_START: &str = "\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    /// Hash of the words before; a collision only merges two rare contexts.
    Prefix(u64),
    DirTail(usize, usize, usize),
    RepoTail(usize, usize, usize),
    Tail2(usize, usize, usize),
    Tail(usize, usize),
    After(usize),
    Args(usize),
    DirStart(usize),
    RepoStart(usize),
    Follows(usize, usize),
    Start,
}

/// Context shared by all the words of a simple command.
struct Place {
    dir: Option<usize>,
    repo: Option<usize>,
    follows: Option<(usize, usize)>,
}

/// The contexts of the word that comes after `before` in a simple command.
fn keys(before: &[usize], place: &Place) -> Vec<Key> {
    let mut keys = Vec::with_capacity(7);
    let Some((&command, _)) = before.split_first() else {
        keys.extend(place.dir.map(Key::DirStart));
        keys.extend(place.repo.map(Key::RepoStart));
        keys.extend(
            place
                .follows
                .map(|(separator, previous)| Key::Follows(separator, previous)),
        );
        keys.push(Key::Start);
        return keys;
    };
    let last = before[before.len() - 1];
    let mut hasher = DefaultHasher::new();
    before.hash(&mut hasher);
    keys.push(Key::Prefix(hasher.finish()));
    keys.extend(place.dir.map(|dir| Key::DirTail(dir, command, last)));
    keys.extend(place.repo.map(|repo| Key::RepoTail(repo, command, last)));
    if before.len() >= 2 {
        keys.push(Key::Tail2(command, before[before.len() - 2], last));
    }
    keys.push(Key::Tail(command, last));
    keys.push(Key::After(last));
    keys.push(Key::Args(command));
    keys
}

/// Splits tokens into simple commands, each with the separator before it.
/// Leading variable assignments (`FOO=1 cmd`) are left out of the words.
fn simple_commands<'a>(tokens: &[Token<'a>]) -> Vec<(Option<&'a str>, Vec<&'a str>)> {
    let mut commands = vec![(None, Vec::new())];
    for token in tokens {
        if token.is_separator() {
            commands.push((Some(token.text), Vec::new()));
        } else if let Some((_, words)) = commands.last_mut()
            && !(words.is_empty() && is_assignment(token.text))
        {
            words.push(token.text);
        }
    }
    commands
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        let mut chars = name.chars();
        chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Exit statuses that do not suggest the command line was wrong: success, and
/// interruption by SIGINT (Ctrl-C), SIGPIPE or SIGTSTP (Ctrl-Z).
fn succeeded(status: i32) -> bool {
    matches!(status, 0 | 130 | 141 | 148)
}

#[derive(Debug, Clone, Copy, Default)]
struct Stat {
    count: f64,
    last: u64,
}

/// Maps strings to dense ids.
#[derive(Debug, Default)]
struct Interner {
    ids: HashMap<String, usize>,
    names: Vec<String>,
}

impl Interner {
    fn intern(&mut self, name: &str) -> usize {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = self.names.len();
        self.names.push(name.to_owned());
        self.ids.insert(name.to_owned(), id);
        id
    }

    fn get(&self, name: &str) -> Option<usize> {
        self.ids.get(name).copied()
    }

    fn name(&self, id: usize) -> &str {
        &self.names[id]
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::repo::NoRepos;

    /// Every directory under `/repo` belongs to the repository rooted there.
    struct OneRepo;

    impl RepoLookup for OneRepo {
        fn repo_root(&self, dir: &Path) -> Option<PathBuf> {
            dir.starts_with("/repo").then(|| PathBuf::from("/repo"))
        }
    }

    fn ran(command: &str, cwd: &str, session: &str) -> Entry {
        Entry {
            command: command.to_owned(),
            cwd: Some(cwd.to_owned()),
            session: Some(session.to_owned()),
            exit: Some(0),
            ..Entry::default()
        }
    }

    fn model_with(history: &[Entry], repos: impl RepoLookup + 'static) -> TokenModel {
        let mut model = TokenModel::new(Params::default(), repos);
        history.iter().for_each(|entry| model.push(entry));
        model
    }

    fn model(history: &[Entry]) -> TokenModel {
        model_with(history, NoRepos)
    }

    /// The most likely word to complete `line`, split at its last word.
    fn top(model: &mut TokenModel, line: &str, cwd: &str, session: &str) -> Option<String> {
        let start = line.rfind(' ').map_or(0, |i| i + 1);
        let (before, partial) = line.split_at(start);
        let ctx = model.context(Some(cwd), Some(session));
        let position = Position::of(before);
        let ranked = model.predict(&position, partial, &ctx);
        ranked.first().map(|(word, _)| (*word).to_owned())
    }

    #[test]
    fn predicts_an_argument_of_a_line_never_typed() {
        let mut model = model(&[
            ran("kubectl get pods -n web", "/p", "s"),
            ran("kubectl -n api get pods", "/p", "s"),
            ran("kubectl get deploy", "/p", "s"),
        ]);
        assert_eq!(
            top(&mut model, "kubectl -n db get po", "/p", "s").as_deref(),
            Some("pods")
        );
        assert_eq!(
            top(&mut model, "kubectl -n db get ", "/p", "s").as_deref(),
            Some("pods")
        );
    }

    #[test]
    fn predicts_the_command_after_a_pipe() {
        let mut model = model(&[
            ran("cat a | grep x", "/p", "s"),
            ran("ls | grep y", "/p", "s"),
            ran("git log", "/p", "s"),
        ]);
        assert_eq!(
            top(&mut model, "ps aux | ", "/p", "s").as_deref(),
            Some("grep")
        );
    }

    #[test]
    fn prefers_what_ran_in_the_current_directory() {
        let mut history = Vec::new();
        for _ in 0..2 {
            history.push(ran("make test", "/a", "s"));
            history.push(ran("make build", "/b", "s"));
        }
        let mut model = model(&history);
        assert_eq!(top(&mut model, "make ", "/a", "s").as_deref(), Some("test"));
        assert_eq!(
            top(&mut model, "make ", "/b", "s").as_deref(),
            Some("build")
        );
    }

    #[test]
    fn uses_the_repository_in_a_directory_it_has_not_seen() {
        let mut model = model_with(
            &[
                ran("cargo nextest run", "/repo/core", "s"),
                ran("cargo new scratch", "/tmp", "s"),
            ],
            OneRepo,
        );
        assert_eq!(
            top(&mut model, "cargo ", "/repo/cli", "s").as_deref(),
            Some("nextest")
        );
    }

    #[test]
    fn predicts_the_command_that_follows_the_previous_line() {
        // docker is the most frequent command overall, but df always came after du.
        let mut history = Vec::new();
        for _ in 0..3 {
            history.push(ran("docker build .", "/p", "s1"));
            history.push(ran("docker push img", "/p", "s1"));
            history.push(ran("du -sh", "/p", "s1"));
            history.push(ran("df -h", "/p", "s1"));
        }
        history.push(ran("du -sh", "/p", "s2"));
        let mut model = model(&history);
        let ctx = model.context(Some("/p"), Some("s2"));
        let ranked = model.predict(&Position::of(""), "d", &ctx);
        assert_eq!(ranked.first().map(|(word, _)| *word), Some("df"));
    }

    #[test]
    fn counts_failed_runs_less() {
        let mut typo = ran("git stauts", "/p", "s");
        typo.exit = Some(1);
        let mut model = model(&[typo.clone(), typo, ran("git status", "/p", "s")]);
        assert_eq!(
            top(&mut model, "git sta", "/p", "s").as_deref(),
            Some("status")
        );
    }

    #[test]
    fn ignores_leading_assignments() {
        let mut model = model(&[ran("RUST_LOG=debug cargo run", "/p", "s")]);
        assert_eq!(
            top(&mut model, "cargo r", "/p", "s").as_deref(),
            Some("run")
        );
        assert_eq!(Position::of("FOO=1 BAR=2 cargo ").words, ["cargo"]);
    }

    #[test]
    fn keeps_quoted_words_whole() {
        let mut model = model(&[ran(r#"git commit -m "fix the bug""#, "/p", "s")]);
        assert_eq!(
            top(&mut model, "git commit -m ", "/p", "s").as_deref(),
            Some(r#""fix the bug""#)
        );
    }

    #[test]
    fn splits_the_position_being_typed() {
        let position = Position::of("cat f | grep -v x && ");
        assert!(position.is_command());
        assert_eq!(position.follows, Some(("&&", "grep")));
        let position = Position::of("docker compose ");
        assert_eq!(position.words, ["docker", "compose"]);
        assert_eq!(position.follows, None);
    }

    #[test]
    fn probabilities_sum_to_one() {
        let mut model = model(&[
            ran("git push", "/p", "s"),
            ran("git pull", "/p", "s"),
            ran("git push", "/p", "s"),
        ]);
        let ctx = model.context(Some("/p"), Some("s"));
        let ranked = model.predict(&Position::of("git "), "pu", &ctx);
        assert_eq!(ranked[0].0, "push");
        let total: f64 = ranked.iter().map(|(_, p)| p).sum();
        assert!((total - 1.0).abs() < 1e-9, "{total}");
    }
}
