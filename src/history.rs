//! The command history: executed commands and the context they ran in.
//!
//! The shell integration appends one record per executed command to an
//! append-only log:
//!
//! ```text
//! 1 <TAB> started_at <TAB> duration_ms <TAB> exit <TAB> session <TAB> cwd <TAB> command
//! ```
//!
//! The leading `1` is the format version and an empty field means "unknown".
//! The text fields (`session`, `cwd` and `command`) are escaped with
//! [`crate::escape`].

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::escape::{escape, unescape};

const FORMAT_VERSION: &str = "1";
const FIELDS: usize = 7;

/// Session given to commands imported from a bash history file, which records
/// no session of its own.
pub const IMPORTED_SESSION: &str = "import";

/// One executed command and the context it ran in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    /// The command line as the shell accepted it.
    pub command: String,
    /// Working directory the command started in.
    pub cwd: Option<String>,
    /// Identifier of the shell session that ran the command.
    pub session: Option<String>,
    /// Exit status.
    pub exit: Option<i32>,
    /// Start time, in seconds since the Unix epoch.
    pub started_at: Option<i64>,
    /// Wall-clock duration, in milliseconds.
    pub duration_ms: Option<u64>,
}

/// Why a history record could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    /// The record was written in an unknown format version.
    #[error("unsupported record version {0:?}")]
    Version(String),
    /// The record does not have the expected number of fields.
    #[error("expected {FIELDS} fields, found {0}")]
    FieldCount(usize),
    /// A numeric field does not hold a number.
    #[error("invalid {field}: {value:?}")]
    Field {
        /// Name of the offending field.
        field: &'static str,
        /// Its raw content.
        value: String,
    },
    /// The command is empty or blank.
    #[error("empty command")]
    EmptyCommand,
}

/// Parses one record, without its trailing newline.
pub fn parse_record(line: &str) -> Result<Entry, RecordError> {
    let fields: Vec<&str> = line.splitn(FIELDS, '\t').collect();
    if fields[0] != FORMAT_VERSION {
        return Err(RecordError::Version(fields[0].to_owned()));
    }
    let &[_, started_at, duration_ms, exit, session, cwd, command] = fields.as_slice() else {
        return Err(RecordError::FieldCount(fields.len()));
    };
    let command = unescape(command);
    if command.trim().is_empty() {
        return Err(RecordError::EmptyCommand);
    }
    Ok(Entry {
        command,
        cwd: text_field(cwd),
        session: text_field(session),
        exit: number_field(exit, "exit")?,
        started_at: number_field(started_at, "started_at")?,
        duration_ms: number_field(duration_ms, "duration_ms")?,
    })
}

/// Formats `entry` as one record, without the trailing newline.
pub fn format_record(entry: &Entry) -> String {
    fn number(value: Option<impl ToString>) -> String {
        value.map(|v| v.to_string()).unwrap_or_default()
    }
    fn text(value: Option<&str>) -> String {
        value.map(escape).unwrap_or_default()
    }
    [
        FORMAT_VERSION.to_owned(),
        number(entry.started_at),
        number(entry.duration_ms),
        number(entry.exit),
        text(entry.session.as_deref()),
        text(entry.cwd.as_deref()),
        escape(&entry.command),
    ]
    .join("\t")
}

fn text_field(raw: &str) -> Option<String> {
    (!raw.is_empty()).then(|| unescape(raw))
}

fn number_field<T: FromStr>(raw: &str, field: &'static str) -> Result<Option<T>, RecordError> {
    if raw.is_empty() {
        return Ok(None);
    }
    raw.parse().map(Some).map_err(|_| RecordError::Field {
        field,
        value: raw.to_owned(),
    })
}

/// Records read by one [`LogTail::poll`].
#[derive(Debug, Default)]
pub struct Batch {
    /// Records appended since the previous poll.
    pub entries: Vec<Entry>,
    /// Lines that could not be parsed.
    pub skipped: usize,
    /// Why the first of them could not, `None` when it is not UTF-8.
    pub first_error: Option<RecordError>,
    /// The file was truncated, replaced or removed: `entries` starts over from
    /// its beginning and everything returned by earlier polls is obsolete.
    pub reset: bool,
}

/// Incremental reader for an append-only history log.
///
/// Every [`LogTail::poll`] returns the records appended since the previous one.
/// A trailing line without its newline is held back until it is complete, so a
/// record being written by another shell is never parsed half-way.
#[derive(Debug)]
pub struct LogTail {
    path: PathBuf,
    offset: u64,
    inode: Option<u64>,
    partial: Vec<u8>,
}

impl LogTail {
    /// Reads `path` from its beginning. The file does not need to exist yet.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            offset: 0,
            inode: None,
            partial: Vec::new(),
        }
    }

    /// The file being read.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the records appended since the previous call.
    pub fn poll(&mut self) -> io::Result<Batch> {
        let mut batch = Batch::default();
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                batch.reset = self.inode.is_some();
                self.restart();
                return Ok(batch);
            }
            Err(e) => return Err(e),
        };
        let meta = file.metadata()?;
        if self.inode.is_some_and(|inode| inode != meta.ino()) || meta.len() < self.offset {
            self.restart();
            batch.reset = true;
        }
        self.inode = Some(meta.ino());
        if meta.len() == self.offset {
            return Ok(batch);
        }

        file.seek(SeekFrom::Start(self.offset))?;
        let mut data = std::mem::take(&mut self.partial);
        let read = file.read_to_end(&mut data)?;
        self.offset += read as u64;
        let complete = data.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        self.partial = data.split_off(complete);

        for line in data.split(|&b| b == b'\n').filter(|line| !line.is_empty()) {
            let error = match std::str::from_utf8(line).map(parse_record) {
                Ok(Ok(entry)) => {
                    batch.entries.push(entry);
                    continue;
                }
                Ok(Err(e)) => Some(e),
                Err(_) => None,
            };
            if batch.skipped == 0 {
                batch.first_error = error;
            }
            batch.skipped += 1;
        }
        Ok(batch)
    }

    fn restart(&mut self) {
        self.offset = 0;
        self.inode = None;
        self.partial.clear();
    }
}

/// Parses a bash history file, in the format of `$HISTFILE`.
///
/// With `HISTTIMEFORMAT` set, bash writes a `#<epoch>` line before every entry
/// and an entry may span several lines; otherwise every line is an entry. All
/// entries belong to [`IMPORTED_SESSION`].
pub fn parse_bash_history(text: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut started_at = None;
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        if let Some(timestamp) = parse_timestamp(line) {
            push_imported(&mut entries, &lines.join("\n"), started_at);
            lines.clear();
            started_at = Some(timestamp);
        } else if started_at.is_some() {
            lines.push(line);
        } else {
            push_imported(&mut entries, line, None);
        }
    }
    push_imported(&mut entries, &lines.join("\n"), started_at);
    entries
}

fn parse_timestamp(line: &str) -> Option<i64> {
    let digits = line.strip_prefix('#')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn push_imported(entries: &mut Vec<Entry>, command: &str, started_at: Option<i64>) {
    if command.trim().is_empty() || command.contains(char::REPLACEMENT_CHARACTER) {
        return;
    }
    entries.push(Entry {
        command: command.to_owned(),
        session: Some(IMPORTED_SESSION.to_owned()),
        started_at,
        ..Entry::default()
    });
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write;

    use super::*;

    fn full_entry() -> Entry {
        Entry {
            command: "printf 'a\\tb\\n' | grep -c a".to_owned(),
            cwd: Some("/home/u/dir with\ttab".to_owned()),
            session: Some("1790000000.123456/4242".to_owned()),
            exit: Some(1),
            started_at: Some(1_790_000_000),
            duration_ms: Some(12),
        }
    }

    #[test]
    fn records_round_trip() {
        for entry in [
            full_entry(),
            Entry {
                command: "ls".to_owned(),
                ..Entry::default()
            },
        ] {
            let record = format_record(&entry);
            assert!(!record.contains('\n'));
            assert_eq!(parse_record(&record), Ok(entry));
        }
    }

    #[test]
    fn parses_a_record_written_by_the_shell() {
        let record = "1\t1790000000\t35\t0\tsid\t/tmp\tgit commit -m \"a\\tb\"";
        let entry = parse_record(record).expect("valid record");
        assert_eq!(entry.command, "git commit -m \"a\tb\"");
        assert_eq!(entry.duration_ms, Some(35));
        assert_eq!(entry.exit, Some(0));
    }

    #[test]
    fn rejects_malformed_records() {
        assert_eq!(
            parse_record("2\t\t\t\t\t\tls"),
            Err(RecordError::Version("2".to_owned()))
        );
        assert_eq!(parse_record("1\t\t\tls"), Err(RecordError::FieldCount(4)));
        assert_eq!(
            parse_record("1\t\t\t\t\t\t  "),
            Err(RecordError::EmptyCommand)
        );
        assert!(matches!(
            parse_record("1\tyesterday\t\t\t\t\tls"),
            Err(RecordError::Field {
                field: "started_at",
                ..
            })
        ));
    }

    fn append(path: &Path, data: &str) {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open log");
        file.write_all(data.as_bytes()).expect("append");
    }

    #[test]
    fn tail_returns_only_complete_new_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.tsv");
        let mut tail = LogTail::new(&path);
        assert!(
            tail.poll().expect("poll").entries.is_empty(),
            "missing file reads as empty"
        );

        append(&path, "1\t\t\t\t\t\tfirst\n1\t\t\t\t\t\tsec");
        let batch = tail.poll().expect("poll");
        assert_eq!(
            batch.entries,
            vec![Entry {
                command: "first".to_owned(),
                ..Entry::default()
            }]
        );

        append(&path, "ond\nnot a record\n");
        let batch = tail.poll().expect("poll");
        assert_eq!(
            batch.entries,
            vec![Entry {
                command: "second".to_owned(),
                ..Entry::default()
            }]
        );
        assert_eq!(batch.skipped, 1);
        assert!(!batch.reset);

        let batch = tail.poll().expect("poll");
        assert!(batch.entries.is_empty() && batch.skipped == 0);
    }

    #[test]
    fn tail_starts_over_when_the_file_is_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.tsv");
        let mut tail = LogTail::new(&path);
        append(&path, "1\t\t\t\t\t\told one\n1\t\t\t\t\t\told two\n");
        assert_eq!(tail.poll().expect("poll").entries.len(), 2);

        let replacement = dir.path().join("new.tsv");
        append(
            &replacement,
            "1\t\t\t\t\t\tnew one\n1\t\t\t\t\t\tnew two\n1\t\t\t\t\t\tnew three\n",
        );
        fs::rename(&replacement, &path).expect("replace");
        let batch = tail.poll().expect("poll");
        assert!(batch.reset);
        assert_eq!(batch.entries.len(), 3);

        fs::write(&path, "1\t\t\t\t\t\ttruncated\n").expect("truncate");
        let batch = tail.poll().expect("poll");
        assert!(batch.reset);
        assert_eq!(batch.entries.len(), 1);

        fs::remove_file(&path).expect("remove");
        assert!(tail.poll().expect("poll").reset);
    }

    #[test]
    fn imports_plain_bash_history() {
        let entries = parse_bash_history("ls -la\n\ncd /tmp\n");
        let commands: Vec<_> = entries.iter().map(|e| e.command.as_str()).collect();
        assert_eq!(commands, ["ls -la", "cd /tmp"]);
        assert!(
            entries
                .iter()
                .all(|e| e.session.as_deref() == Some(IMPORTED_SESSION))
        );
    }

    #[test]
    fn imports_timestamped_multi_line_entries() {
        let text =
            "legacy\n#1700000000\nfor f in *; do\necho \"$f\"\ndone\n#1700000005\ngit status\n";
        let entries = parse_bash_history(text);
        let summary: Vec<_> = entries
            .iter()
            .map(|e| (e.command.as_str(), e.started_at))
            .collect();
        assert_eq!(
            summary,
            [
                ("legacy", None),
                ("for f in *; do\necho \"$f\"\ndone", Some(1_700_000_000)),
                ("git status", Some(1_700_000_005)),
            ]
        );
    }
}
