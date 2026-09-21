//! `augur serve`: answers the queries of one shell over stdin and stdout.
//!
//! The shell queries on every keystroke pause and abandons a query as soon as
//! the user types again, so queries can arrive faster than they are answered.
//! Only the newest pending query is answered: the others were abandoned
//! already. Questions for the language model run on their own thread; an
//! answer that arrives after a newer query is dropped, and a query whose
//! question fails gets its fallback.
//!
//! A query that cannot be understood is answered with the reason, and logged:
//! the shell integration may be older than this engine, and an engine that
//! answered nothing would look like one with nothing to suggest.

use std::io::{self, BufRead, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, Result};
use augur::commands::Commands;
use augur::files::Disk;
use augur::history::{Entry, LogTail, RecordError};
use augur::llm::{Ollama, Oracle};
use augur::protocol::{self, ParseError, Query, Request};
use augur::repo::GitRepos;
use augur::state::StateDir;
use augur::suggest::{self, Engine, Plan, Question, Suggestion};

/// How long the model may take to load into memory.
pub(crate) const WARM_UP: Duration = Duration::from_secs(60);

/// The language model to ask, when there is one.
#[derive(Debug, Clone)]
pub struct Model {
    /// Ollama's base URL.
    pub url: String,
    /// Model name.
    pub name: String,
    /// How long to wait for an answer.
    pub timeout: Duration,
}

/// Settings for [`run`].
#[derive(Debug, Clone)]
pub struct Options {
    /// Where the history lives.
    pub state: StateDir,
    /// Session id of the shell being served.
    pub session: Option<String>,
    /// Exit once this process, the shell being served, is gone.
    pub owner_pid: Option<u32>,
    /// How often to check that the owner is alive.
    pub owner_poll: Duration,
    /// Engine parameters.
    pub params: suggest::Params,
    /// The language model, if any.
    pub model: Option<Model>,
}

enum Event {
    Line(String),
    Answer {
        id: u64,
        /// The model's answer, or why there is none.
        answer: Result<Option<Suggestion>, String>,
    },
    Closed,
    OwnerGone,
}

/// A query waiting for the language model.
struct Pending {
    id: u64,
    fallback: Option<Suggestion>,
    line: String,
    cwd: String,
    is_guess: bool,
}

/// Serves queries until stdin closes or the owner process exits.
///
/// Nothing but responses may go to stdout; diagnostics go to stderr, which the
/// shell integration discards unless `bleopt augur_log` names a file.
pub fn run(options: Options) -> Result<()> {
    let mut server = Server::load(&options)?;
    let oracle = options.model.as_ref().map(|model| {
        let ollama = Arc::new(Ollama::new(&model.url, &model.name, model.timeout));
        let warming = Arc::clone(&ollama);
        thread::spawn(move || {
            if let Err(e) = warming.warm_up(WARM_UP) {
                eprintln!("augur: {e}");
            }
        });
        ollama as Arc<dyn Oracle>
    });

    let (tx, rx) = mpsc::channel();
    spawn_reader(tx.clone());
    if let Some(pid) = options.owner_pid {
        spawn_owner_watch(pid, options.owner_poll, tx.clone());
    }

    let mut out = io::stdout().lock();
    let mut pending: Option<Pending> = None;
    // The model's last failure. Without Ollama every question fails the same
    // way, and the log would grow by a line at every pause in typing.
    let mut model_error: Option<String> = None;
    while let Ok(first) = rx.recv() {
        let mut newest: Option<Result<Query, (u64, String)>> = None;
        let mut answers = Vec::new();
        for event in std::iter::once(first).chain(rx.try_iter()) {
            match event {
                Event::Line(line) => match protocol::parse_request(&line) {
                    Ok(Request::Query(query)) => newest = Some(Ok(query)),
                    Ok(Request::Names(names)) => server.set_shell_names(names),
                    Err(e) => {
                        eprintln!("augur: ignoring request: {e}");
                        if let ParseError::Malformed { id: Some(id), .. } = e {
                            newest = Some(Err((id, e.to_string())));
                        }
                    }
                },
                Event::Answer { id, answer } => answers.push((id, answer)),
                Event::Closed | Event::OwnerGone => return Ok(()),
            }
        }

        if let Some(query) = newest {
            pending = None;
            let query = match query {
                Ok(query) => query,
                Err((id, reason)) => {
                    writeln!(out, "{}", protocol::format_error(id, &reason))?;
                    out.flush()?;
                    continue;
                }
            };
            match server.plan(&query) {
                Plan::Ready(suggestion) => respond(&mut out, query.id, suggestion.as_ref())?,
                Plan::Ask { question, fallback } => match &oracle {
                    Some(oracle) => {
                        let id = query.id;
                        pending = Some(Pending {
                            id,
                            fallback,
                            line: query.line,
                            cwd: query.cwd,
                            is_guess: question.is_guess(),
                        });
                        spawn_question(Arc::clone(oracle), id, question, tx.clone());
                    }
                    None => respond(&mut out, query.id, fallback.as_ref())?,
                },
            }
        }
        for (id, answer) in answers {
            let answer = match answer {
                Ok(answer) => {
                    model_error = None;
                    answer
                }
                Err(e) => {
                    if model_error.as_ref() != Some(&e) {
                        eprintln!("augur: {e}");
                        model_error = Some(e);
                    }
                    None
                }
            };
            if let Some(waiting) = pending.take_if(|pending| pending.id == id) {
                let suggestion = server.engine.settle(
                    &waiting.line,
                    Some(&waiting.cwd),
                    waiting.is_guess,
                    answer,
                    waiting.fallback,
                );
                respond(&mut out, id, suggestion.as_ref())?;
            }
        }
    }
    Ok(())
}

fn respond(out: &mut impl Write, id: u64, suggestion: Option<&Suggestion>) -> Result<()> {
    writeln!(out, "{}", protocol::format_response(id, suggestion))?;
    out.flush()?;
    Ok(())
}

struct Server {
    engine: Engine,
    params: suggest::Params,
    session: Option<String>,
    shell_names: Vec<String>,
    imported: Vec<Entry>,
    recorded: LogTail,
}

impl Server {
    fn load(options: &Options) -> Result<Self> {
        let imported_path = options.state.imported_log();
        let imported = LogTail::new(&imported_path)
            .poll()
            .with_context(|| format!("reading {}", imported_path.display()))?;
        report_skipped(
            &imported_path,
            imported.skipped,
            imported.first_error.as_ref(),
        );
        let mut server = Self {
            engine: new_engine(&options.params),
            params: options.params.clone(),
            session: options.session.clone(),
            shell_names: Vec::new(),
            imported: imported.entries,
            recorded: LogTail::new(options.state.history_log()),
        };
        server.rebuild();
        server.refresh()?;
        Ok(server)
    }

    fn rebuild(&mut self) {
        self.engine = new_engine(&self.params);
        self.engine
            .set_shell_names(self.shell_names.iter().cloned());
        for entry in &self.imported {
            self.engine.push(entry);
        }
    }

    fn set_shell_names(&mut self, names: Vec<String>) {
        self.engine.set_shell_names(names.iter().cloned());
        self.shell_names = names;
    }

    /// Learns the commands recorded since the previous refresh, including those
    /// of other shells.
    fn refresh(&mut self) -> Result<()> {
        let batch = self
            .recorded
            .poll()
            .with_context(|| format!("reading {}", self.recorded.path().display()))?;
        if batch.reset {
            self.rebuild();
        }
        report_skipped(
            self.recorded.path(),
            batch.skipped,
            batch.first_error.as_ref(),
        );
        for entry in &batch.entries {
            self.engine.push(entry);
        }
        Ok(())
    }

    fn plan(&mut self, query: &Query) -> Plan {
        if let Err(e) = self.refresh() {
            eprintln!("augur: {e:#}");
        }
        self.engine.plan(&suggest::Request {
            line: &query.line,
            cwd: Some(&query.cwd),
            session: self.session.as_deref(),
            word_start: query.word_start,
            candidates: &query.candidates,
            fuzzy: query.fuzzy,
        })
    }
}

/// An engine that knows this machine: its repositories, commands and files.
pub(crate) fn new_engine(params: &suggest::Params) -> Engine {
    Engine::new(
        params.clone(),
        GitRepos,
        Commands::new(std::env::var_os("PATH")),
    )
    .with_files(Disk, std::env::var_os("HOME").map(PathBuf::from))
}

fn report_skipped(path: &Path, skipped: usize, first: Option<&RecordError>) {
    if skipped > 0 {
        let reason = first.map_or_else(|| "not UTF-8".to_owned(), ToString::to_string);
        eprintln!(
            "augur: skipped {skipped} malformed records in {}, the first: {reason}",
            path.display()
        );
    }
}

fn spawn_question(oracle: Arc<dyn Oracle>, id: u64, question: Question, tx: Sender<Event>) {
    thread::spawn(move || {
        // A panic over a response nobody foresaw must not leave the query
        // without an answer: the shell would wait for its whole timeout.
        let answer = panic::catch_unwind(AssertUnwindSafe(|| question.ask(oracle.as_ref())))
            .unwrap_or_else(|_| Ok(None))
            .map_err(|e| e.to_string());
        // The server may be gone already; nobody is left to answer then.
        let _ = tx.send(Event::Answer { id, answer });
    });
}

fn spawn_reader(tx: Sender<Event>) {
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match stdin.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf);
                    let line = line.strip_suffix('\n').unwrap_or(&line).to_owned();
                    if tx.send(Event::Line(line)).is_err() {
                        return;
                    }
                }
            }
        }
        // The receiver may be gone already; nothing is left to notify then.
        let _ = tx.send(Event::Closed);
    });
}

/// Watches `/proc/<pid>`. The shell keeps both ends of our stdin FIFO open, so
/// stdin never reaches EOF when the shell dies without unloading ble.sh, e.g.
/// on SIGKILL.
fn spawn_owner_watch(pid: u32, poll: Duration, tx: Sender<Event>) {
    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    thread::spawn(move || {
        while proc_dir.exists() {
            thread::sleep(poll);
        }
        // The server may have exited on its own meanwhile.
        let _ = tx.send(Event::OwnerGone);
    });
}
