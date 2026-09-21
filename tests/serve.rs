//! `augur serve` driven over its stdin/stdout protocol, as ble.sh drives it.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use augur::protocol::VERSION;

const TIMEOUT: Duration = Duration::from_secs(5);

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    responses: Receiver<String>,
}

impl Server {
    /// An engine without a language model.
    fn start(state: &Path, args: &[&str]) -> Self {
        Self::spawn(state, &[&["--no-model"], args].concat())
    }

    fn spawn(state: &Path, args: &[&str]) -> Self {
        let protocol = VERSION.to_string();
        Self::spawn_bare(state, &[&["--protocol", &protocol], args].concat())
    }

    /// An engine given nothing but `args`, its diagnostics kept.
    fn spawn_bare(state: &Path, args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_augur"))
            .arg("serve")
            .args(args)
            .env("AUGUR_STATE_DIR", state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn augur serve");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, responses) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            responses,
        }
    }

    fn send(&mut self, requests: &str) {
        let stdin = self.stdin.as_mut().expect("stdin still open");
        stdin
            .write_all(requests.as_bytes())
            .expect("write requests");
        stdin.flush().expect("flush requests");
    }

    fn response(&self) -> String {
        self.responses
            .recv_timeout(TIMEOUT)
            .expect("a response in time")
    }

    /// Queries `line`, typed in `/work`, and returns the response's edit kind
    /// and text. Candidates carry their kind: `wstatus`, `fnotes.md`.
    fn query(
        &mut self,
        id: u64,
        line: &str,
        word_start: &str,
        candidates: &[&str],
    ) -> (String, String) {
        self.query_in("/work", id, line, word_start, candidates)
    }

    fn query_in(
        &mut self,
        cwd: &str,
        id: u64,
        line: &str,
        word_start: &str,
        candidates: &[&str],
    ) -> (String, String) {
        let matching = if candidates.is_empty() { "" } else { "prefix" };
        self.send(&format!(
            "Q\t{id}\t{cwd}\t{line}\t{word_start}\t{matching}\t{}\n",
            candidates.join("\x1f")
        ));
        let response = self.response();
        let fields: Vec<&str> = response.split('\t').collect();
        assert_eq!(fields.len(), 5, "{response:?}");
        assert_eq!((fields[0], fields[1]), ("R", id.to_string().as_str()));
        (fields[2].to_owned(), fields[3].to_owned())
    }

    /// The text appended for `line`, empty for no suggestion.
    fn append(&mut self, id: u64, line: &str) -> String {
        let (kind, text) = self.query(id, line, "", &[]);
        assert!(kind == "+" || kind.is_empty(), "unexpected edit {kind:?}");
        text
    }

    fn exits_within(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.child.try_wait().expect("poll child").is_some() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // The child may have exited already; there is nothing to clean up then.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn record(path: &Path, command: &str) {
    record_in(path, "/work", command);
}

fn record_in(path: &Path, cwd: &str, command: &str) {
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open history log");
    writeln!(log, "1\t1790000000\t5\t0\tother-shell\t{cwd}\t{command}").expect("append record");
}

#[test]
fn suggests_one_word_from_the_log_and_follows_new_records() {
    let state = tempfile::tempdir().expect("tempdir");
    let log = state.path().join("history.tsv");
    record(&log, "cargo build --release");

    let mut server = Server::start(state.path(), &["--session", "this-shell"]);
    assert_eq!(server.append(1, "cargo b"), "uild");
    assert_eq!(server.append(2, "cargo build "), "--release");
    assert_eq!(server.append(3, "docker c"), "");

    record(&log, "docker compose up -d");
    assert_eq!(server.append(4, "docker c"), "ompose");
}

#[test]
fn chooses_among_the_completion_candidates() {
    let state = tempfile::tempdir().expect("tempdir");
    record(&state.path().join("history.tsv"), "docker ps");
    let mut server = Server::start(state.path(), &[]);

    let answer = server.query(1, "dock", "0", &["wdocker", "wdocker-compose", "wdockerd"]);
    assert_eq!(answer, ("+".to_owned(), "er".to_owned()));
    // Without history or model, the shortest candidate that continues the word
    // stands.
    let answer = server.query(2, "systemctl sta", "10", &["wstart", "wstatus", "wstop"]);
    assert_eq!(answer, ("+".to_owned(), "rt".to_owned()));
}

#[test]
fn corrects_a_word_against_the_shell_names() {
    let state = tempfile::tempdir().expect("tempdir");
    let mut server = Server::start(state.path(), &[]);
    server.send("N\tbleopt\x1fble-bind\x1fble-import\n");
    let answer = server.query(1, "ble-o", "0", &[]);
    assert_eq!(answer, ("=".to_owned(), "bleopt".to_owned()));
}

#[test]
fn keeps_the_shell_names_when_the_log_is_replaced() {
    let state = tempfile::tempdir().expect("tempdir");
    let log = state.path().join("history.tsv");
    record(&log, "ls -la");
    let mut server = Server::start(state.path(), &[]);
    server.send("N\tbleopt\n");
    assert_eq!(server.append(1, "ls "), "-la");

    let replacement = state.path().join("history.tsv.new");
    record(&replacement, "ls -lh");
    std::fs::rename(&replacement, &log).expect("replace the log");
    assert_eq!(server.append(2, "ls "), "-lh");
    let answer = server.query(3, "ble-o", "0", &[]);
    assert_eq!(answer, ("=".to_owned(), "bleopt".to_owned()));
}

#[test]
fn suggests_a_file_from_the_history_only_where_it_is() {
    let state = tempfile::tempdir().expect("tempdir");
    let here = tempfile::tempdir().expect("tempdir");
    let elsewhere = tempfile::tempdir().expect("tempdir");
    std::fs::write(here.path().join("status.txt"), "").expect("write the file");
    let (here, elsewhere) = (
        here.path().to_str().expect("utf-8 path"),
        elsewhere.path().to_str().expect("utf-8 path"),
    );
    let log = state.path().join("history.tsv");
    for cwd in [here, here, elsewhere] {
        record_in(&log, cwd, "cat status.txt");
    }
    let mut server = Server::start(state.path(), &[]);

    let text = |answer: (String, String)| answer.1;
    assert_eq!(
        text(server.query_in(here, 1, "cat s", "", &[])),
        "tatus.txt"
    );
    assert_eq!(text(server.query_in(elsewhere, 2, "cat s", "", &[])), "");
    // The completion lists the directory for a command it knows nothing about.
    let listing = ["f.git", "fsrc"];
    assert_eq!(
        text(server.query_in(elsewhere, 3, "kubectl get ", "12", &listing)),
        ""
    );
}

#[test]
fn answers_queued_queries_in_order_ending_with_the_newest() {
    let state = tempfile::tempdir().expect("tempdir");
    record(&state.path().join("history.tsv"), "cargo build");
    let mut server = Server::start(state.path(), &[]);

    server.send("Q\t1\t/work\tc\t\t\t\nQ\t2\t/work\tca\t\t\t\nQ\t3\t/work\tcar\t\t\t\n");
    let mut ids = Vec::new();
    while ids.last() != Some(&3) {
        let response = server.response();
        let id: u64 = response
            .split('\t')
            .nth(1)
            .and_then(|id| id.parse().ok())
            .expect("an id");
        ids.push(id);
    }
    assert!(
        ids.windows(2).all(|pair| pair[0] < pair[1]),
        "out of order: {ids:?}"
    );
}

#[test]
fn skips_unknown_requests_and_answers_malformed_queries() {
    let state = tempfile::tempdir().expect("tempdir");
    record(&state.path().join("history.tsv"), "ls -la");
    let mut server = Server::start(state.path(), &[]);

    server.send("HELLO\tthere\n");
    // A query of the first protocol, and a candidate without its kind.
    server.send("Q\t5\t/work\tls\n");
    assert_eq!(
        server.response(),
        "R\t5\t!\tmalformed request: a query has 5 fields after its id, found 2\t"
    );
    server.send("Q\t6\t/work\tls \t3\tprefix\t-la\n");
    assert_eq!(
        server.response(),
        "R\t6\t!\tmalformed request: a candidate without its kind\t"
    );
    assert_eq!(server.append(7, "ls "), "-la");
}

#[test]
fn refuses_a_shell_integration_that_speaks_another_protocol() {
    let state = tempfile::tempdir().expect("tempdir");
    for args in [&["--no-model"][..], &["--no-model", "--protocol", "1"][..]] {
        let mut server = Server::spawn_bare(state.path(), args);
        assert!(server.exits_within(TIMEOUT), "served {args:?}");
        let status = server.child.wait().expect("reap the engine");
        assert!(!status.success(), "accepted {args:?}");
        // The shell shows the last line as the reason: it must say what to do.
        let mut diagnostics = String::new();
        let mut stderr = server.child.stderr.take().expect("piped stderr");
        stderr
            .read_to_string(&mut diagnostics)
            .expect("read stderr");
        let reason = diagnostics.lines().last().unwrap_or_default();
        assert!(reason.contains("ble-augur restart"), "{diagnostics:?}");
    }
}

#[test]
fn states_its_protocol_without_a_state_directory() {
    let output = Command::new(env!("CARGO_BIN_EXE_augur"))
        .arg("protocol")
        .env_remove("HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("AUGUR_STATE_DIR")
        .output()
        .expect("run augur protocol");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        VERSION.to_string()
    );
}

/// An Ollama that continues every prompt with `text`, certain of it.
fn model_continuing(text: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("address"));
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().expect("clone the stream"));
            let mut length = 0;
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|read| read > 0) && line != "\r\n" {
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap_or(0);
                }
                line.clear();
            }
            let mut body = vec![0; length];
            if reader.read_exact(&mut body).is_err() {
                continue;
            }
            let answer = format!(
                r#"{{"response":"{text}","logprobs":[{{"token":"{text}","logprob":-0.01}}]}}"#
            );
            // The engine may have given up on the request already.
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                answer.len()
            );
        }
    });
    url
}

#[test]
fn checks_the_files_the_model_comes_up_with() {
    let state = tempfile::tempdir().expect("tempdir");
    let work = tempfile::tempdir().expect("tempdir");
    std::fs::write(work.path().join("notes.md"), "").expect("write the file");
    let cwd = work.path().to_str().expect("utf-8 path");

    for (continuation, expected) in [(" notes.md", "notes.md"), (" ghost.txt", "")] {
        let url = model_continuing(continuation);
        let mut server =
            Server::spawn(state.path(), &["--ollama", &url, "--model-timeout", "3000"]);
        let (_, text) = server.query_in(cwd, 1, "cat ", "", &[]);
        assert_eq!(text, expected, "the model continued with {continuation:?}");
    }
}

#[test]
fn exits_once_the_owner_shell_is_gone() {
    let state = tempfile::tempdir().expect("tempdir");
    let mut owner = Command::new("true")
        .spawn()
        .expect("spawn a short-lived owner");
    let owner_pid = owner.id().to_string();
    owner.wait().expect("reap the owner");

    let mut server = Server::start(state.path(), &["--owner-pid", &owner_pid]);
    assert!(server.exits_within(TIMEOUT));
}

#[test]
fn exits_when_stdin_closes() {
    let state = tempfile::tempdir().expect("tempdir");
    let mut server = Server::start(state.path(), &[]);
    drop(server.stdin.take());
    assert!(server.exits_within(TIMEOUT));
}
