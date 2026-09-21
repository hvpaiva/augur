mod serve;

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use augur::commands::Commands;
use augur::eval::{self, Metrics, Strategy};
use augur::history::{self, format_record};
use augur::llm::{self, Ollama, Oracle};
use augur::protocol;
use augur::repo::GitRepos;
use augur::state::StateDir;
use augur::suggest::{self, Candidate, Edit, Plan, Request};
use clap::{Args, Parser, Subcommand};

/// Inline command suggestions for bash, one word at a time, via ble.sh.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Answer suggestion queries from one shell over stdin/stdout (started by ble.sh)
    Serve {
        /// Protocol the shell integration speaks; the engine refuses another
        #[arg(long, value_name = "VERSION")]
        protocol: Option<u32>,
        /// Exit when this process, the shell being served, is gone
        #[arg(long, value_name = "PID")]
        owner_pid: Option<u32>,
        /// Session id of the shell being served
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        #[command(flatten)]
        model: ModelArgs,
    },
    /// Print the version of the protocol `serve` speaks
    Protocol,
    /// Snapshot a bash history file as the starting history
    Import {
        /// History file to import [default: $HISTFILE, then ~/.bash_history]
        #[arg(long, value_name = "FILE")]
        histfile: Option<PathBuf>,
        /// Replace an existing snapshot
        #[arg(long)]
        force: bool,
    },
    /// Replay the history and compare augur with ble.sh's history suggestions
    Eval {
        /// Measure only the last N commands; the ones before only train the models
        #[arg(long, value_name = "N")]
        last: Option<usize>,
        /// Also measure augur with the language model (slow: one request per decision)
        #[arg(long)]
        with_model: bool,
        #[command(flatten)]
        model: ModelArgs,
    },
    /// Show what augur suggests for a line
    Suggest {
        /// The partially typed command line
        line: String,
        /// Directory the line is typed in [default: the current directory]
        #[arg(long, value_name = "DIR")]
        cwd: Option<String>,
        /// Session typing the line
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        /// A word the shell's completion offers for the current word; repeat for several
        #[arg(long = "candidate", value_name = "WORD")]
        candidates: Vec<String>,
        /// A file name the shell's completion offers for the current word; repeat for several
        #[arg(long = "file", value_name = "NAME")]
        files: Vec<String>,
        #[command(flatten)]
        model: ModelArgs,
    },
}

#[derive(Args)]
struct ModelArgs {
    /// Ollama model asked when the history and the completion cannot decide
    #[arg(long, value_name = "NAME", default_value = llm::DEFAULT_MODEL)]
    model: String,
    /// Never ask a language model
    #[arg(long)]
    no_model: bool,
    /// Ollama's base URL
    #[arg(long, value_name = "URL", default_value = llm::DEFAULT_URL)]
    ollama: String,
    /// Milliseconds to wait for the model's answer
    #[arg(long, value_name = "MS", default_value_t = 400)]
    model_timeout: u64,
}

impl ModelArgs {
    fn model(&self) -> Option<serve::Model> {
        (!self.no_model).then(|| serve::Model {
            url: self.ollama.clone(),
            name: self.model.clone(),
            timeout: Duration::from_millis(self.model_timeout),
        })
    }

    /// A client for the model, loaded into memory so that the first request
    /// does not time out while it loads.
    fn loaded_oracle(&self) -> Result<Option<Ollama>> {
        let Some(model) = self.model() else {
            return Ok(None);
        };
        let ollama = Ollama::new(&model.url, &model.name, model.timeout);
        ollama
            .warm_up(serve::WARM_UP)
            .with_context(|| format!("loading {} from {}", model.name, model.url))?;
        Ok(Some(ollama))
    }
}

/// Reports an error on one line: the shell integration shows the last line the
/// engine wrote as the reason it exited.
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("augur: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    // Not located yet: `protocol` must answer even without a state directory,
    // or the shell would take an engine without HOME for an old one.
    let state = StateDir::locate;
    match cli.command {
        Command::Serve {
            protocol,
            owner_pid,
            session,
            model,
        } => {
            check_protocol(protocol)?;
            serve::run(serve::Options {
                state: state()?,
                session,
                owner_pid,
                owner_poll: Duration::from_secs(1),
                params: suggest::Params::default(),
                model: model.model(),
            })
        }
        Command::Protocol => {
            println!("{}", protocol::VERSION);
            Ok(())
        }
        Command::Import { histfile, force } => import(&state()?, histfile, force),
        Command::Eval {
            last,
            with_model,
            model,
        } => {
            let oracle = if with_model {
                model.loaded_oracle()?
            } else {
                None
            };
            evaluate(&state()?, last, oracle.as_ref())
        }
        Command::Suggest {
            line,
            cwd,
            session,
            candidates,
            files,
            model,
        } => {
            let candidates: Vec<Candidate> = candidates
                .into_iter()
                .map(Candidate::word)
                .chain(files.into_iter().map(Candidate::file))
                .collect();
            suggest(
                &state()?,
                &line,
                cwd,
                session.as_deref(),
                &candidates,
                &model,
            )
        }
    }
}

/// Refuses to serve a shell integration that speaks another protocol. The two
/// are installed separately, and answering queries the engine misreads would
/// leave the shell without suggestions and without a reason.
fn check_protocol(spoken: Option<u32>) -> Result<()> {
    const REMEDY: &str = "run `ble-augur restart`, or source shell/augur.bash again";
    match spoken {
        Some(protocol::VERSION) => Ok(()),
        Some(other) => bail!(
            "the shell integration speaks protocol {other} and this engine protocol {}: {REMEDY}",
            protocol::VERSION
        ),
        None => bail!(
            "the shell integration did not state its protocol; this engine speaks protocol {}: \
             {REMEDY}",
            protocol::VERSION
        ),
    }
}

fn import(state: &StateDir, histfile: Option<PathBuf>, force: bool) -> Result<()> {
    let histfile = match histfile.or_else(|| env::var_os("HISTFILE").map(PathBuf::from)) {
        Some(path) => path,
        None => {
            PathBuf::from(env::var_os("HOME").context("HOME is not set")?).join(".bash_history")
        }
    };
    let target = state.imported_log();
    if target.exists() && !force {
        bail!(
            "{} exists already, and importing again would count the commands recorded since twice; \
             pass --force to replace it",
            target.display()
        );
    }
    let bytes = fs::read(&histfile).with_context(|| format!("reading {}", histfile.display()))?;
    let entries = history::parse_bash_history(&String::from_utf8_lossy(&bytes));

    state
        .create()
        .with_context(|| format!("creating {}", state.root().display()))?;
    let partial = target.with_extension("tsv.partial");
    // A leftover, or a link planted in its place, must not decide where the
    // history goes or with which permissions.
    match fs::remove_file(&partial) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", partial.display()));
        }
        _ => {}
    }
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&partial)
        .with_context(|| format!("creating {}", partial.display()))?;
    let mut out = BufWriter::new(file);
    for entry in &entries {
        writeln!(out, "{}", format_record(entry))
            .with_context(|| format!("writing {}", partial.display()))?;
    }
    out.into_inner()
        .map_err(io::IntoInnerError::into_error)?
        .sync_all()
        .with_context(|| format!("writing {}", partial.display()))?;
    fs::rename(&partial, &target).with_context(|| format!("replacing {}", target.display()))?;

    println!(
        "imported {} commands from {} into {}",
        entries.len(),
        histfile.display(),
        target.display()
    );
    Ok(())
}

fn evaluate(state: &StateDir, last: Option<usize>, oracle: Option<&Ollama>) -> Result<()> {
    let history = state
        .load()
        .with_context(|| format!("reading the history in {}", state.root().display()))?;
    if history.entries.is_empty() {
        bail!(
            "no history in {}: run `augur import` first",
            state.root().display()
        );
    }
    let mut strategies = vec![Strategy::BleHistory, Strategy::Augur];
    if oracle.is_some() {
        strategies.push(Strategy::AugurModel);
    }
    let results = eval::replay(
        &history.entries,
        &suggest::Params::default(),
        &strategies,
        GitRepos,
        oracle.map(|oracle| oracle as &dyn Oracle),
        last,
        Some(Commands::new(env::var_os("PATH"))),
    );
    print_report(&strategies, &results, history.skipped);
    Ok(())
}

fn print_report(strategies: &[Strategy], results: &[Metrics], skipped: usize) {
    let Some(first) = results.first() else { return };
    println!(
        "measured {} commands, {} characters",
        first.commands, first.characters
    );
    if skipped > 0 {
        println!("skipped {skipped} malformed records");
    }
    println!();
    println!(
        "{:<15} {:>9} {:>9} {:>9} {:>9} {:>10}",
        "strategy", "coverage", "accuracy", "useful", "saved", "latency"
    );
    for (strategy, m) in strategies.iter().zip(results) {
        println!(
            "{:<15} {:>8.1}% {:>8.1}% {:>8.1}% {:>8.1}% {:>8}µs",
            strategy.label(),
            m.coverage() * 100.0,
            m.accuracy() * 100.0,
            m.useful() * 100.0,
            m.savings() * 100.0,
            m.mean_latency().as_micros(),
        );
    }
    println!();
    println!("coverage  positions with a suggestion, measured after every typed character");
    println!("accuracy  suggestions that were exactly the rest of the word, or the next word");
    println!("useful    positions with a right suggestion");
    println!("saved     keystrokes saved accepting right suggestions with →");
}

fn suggest(
    state: &StateDir,
    line: &str,
    cwd: Option<String>,
    session: Option<&str>,
    candidates: &[Candidate],
    model: &ModelArgs,
) -> Result<()> {
    let history = state
        .load()
        .with_context(|| format!("reading the history in {}", state.root().display()))?;
    let mut engine = serve::new_engine(&suggest::Params::default());
    for entry in &history.entries {
        engine.push(entry);
    }
    let cwd = match cwd {
        Some(cwd) => cwd,
        None => env::current_dir()
            .context("reading the current directory")?
            .to_string_lossy()
            .into_owned(),
    };
    let request = Request {
        line,
        cwd: Some(&cwd),
        session,
        word_start: None,
        candidates,
        fuzzy: false,
    };
    let suggestion = match engine.plan(&request) {
        Plan::Ready(suggestion) => suggestion,
        Plan::Ask { question, fallback } => {
            let answer = match model.loaded_oracle() {
                Ok(Some(oracle)) => question.ask(&oracle).unwrap_or_else(|e| {
                    eprintln!("augur: {e}");
                    None
                }),
                Ok(None) => None,
                Err(e) => {
                    eprintln!("augur: {e:#}");
                    None
                }
            };
            engine.settle(line, Some(&cwd), question.is_guess(), answer, fallback)
        }
    };
    match suggestion {
        None => println!("no suggestion"),
        Some(suggestion) => {
            let origin = format!(
                "(from {}, p={:.2})",
                suggestion.source.name(),
                suggestion.confidence
            );
            match suggestion.edit {
                Edit::Append(text) => println!("{line}\u{2502}{text}    {origin}"),
                Edit::Replace(word) => println!("replace the current word with {word}    {origin}"),
            }
        }
    }
    Ok(())
}
