//! augur's state directory and the files in it.

use std::ffi::OsString;
use std::fs::DirBuilder;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use crate::history::{Entry, LogTail};

/// Environment variable overriding the state directory. Must be absolute.
pub const STATE_DIR_ENV: &str = "AUGUR_STATE_DIR";

/// No state directory could be derived from the environment.
#[derive(Debug, thiserror::Error)]
#[error(
    "cannot locate the state directory: set {STATE_DIR_ENV}, XDG_STATE_HOME or HOME to an absolute path"
)]
pub struct NoStateDir;

/// The directory holding the history log written by the shell and the snapshot
/// written by `augur import`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDir {
    root: PathBuf,
}

/// History read from a [`StateDir`].
#[derive(Debug, Default)]
pub struct History {
    /// Imported entries followed by recorded ones, oldest first.
    pub entries: Vec<Entry>,
    /// Lines that could not be parsed.
    pub skipped: usize,
}

impl StateDir {
    /// Locates the directory from the environment: `$AUGUR_STATE_DIR`, then
    /// `$XDG_STATE_HOME/augur`, then `$HOME/.local/state/augur`. Relative paths
    /// are ignored, as the XDG Base Directory specification requires.
    pub fn locate() -> Result<Self, NoStateDir> {
        Self::locate_with(|name| std::env::var_os(name))
    }

    fn locate_with(var: impl Fn(&str) -> Option<OsString>) -> Result<Self, NoStateDir> {
        let absolute = |name: &str| var(name).map(PathBuf::from).filter(|p| p.is_absolute());
        let root = if let Some(dir) = absolute(STATE_DIR_ENV) {
            dir
        } else if let Some(state_home) = absolute("XDG_STATE_HOME") {
            state_home.join("augur")
        } else {
            absolute("HOME")
                .ok_or(NoStateDir)?
                .join(".local/state/augur")
        };
        Ok(Self { root })
    }

    /// The directory itself.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The log the shell integration appends to.
    pub fn history_log(&self) -> PathBuf {
        self.root.join("history.tsv")
    }

    /// The snapshot written by `augur import`.
    pub fn imported_log(&self) -> PathBuf {
        self.root.join("imported.tsv")
    }

    /// Creates the directory, accessible by its owner only.
    pub fn create(&self) -> io::Result<()> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.root)
    }

    /// Reads the imported snapshot followed by the recorded log. Missing files
    /// read as empty.
    pub fn load(&self) -> io::Result<History> {
        let mut history = History::default();
        for path in [self.imported_log(), self.history_log()] {
            let batch = LogTail::new(path).poll()?;
            history.entries.extend(batch.entries);
            history.skipped += batch.skipped;
        }
        Ok(history)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locate(vars: &[(&str, &str)]) -> Option<PathBuf> {
        StateDir::locate_with(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        })
        .ok()
        .map(|dir| dir.root)
    }

    #[test]
    fn follows_the_documented_precedence() {
        let all = [
            (STATE_DIR_ENV, "/custom"),
            ("XDG_STATE_HOME", "/xdg"),
            ("HOME", "/home/u"),
        ];
        assert_eq!(locate(&all), Some(PathBuf::from("/custom")));
        assert_eq!(locate(&all[1..]), Some(PathBuf::from("/xdg/augur")));
        assert_eq!(
            locate(&all[2..]),
            Some(PathBuf::from("/home/u/.local/state/augur"))
        );
        assert_eq!(locate(&[]), None);
    }

    #[test]
    fn ignores_relative_paths() {
        let vars = [
            (STATE_DIR_ENV, "relative"),
            ("XDG_STATE_HOME", "also/relative"),
            ("HOME", "/home/u"),
        ];
        assert_eq!(
            locate(&vars),
            Some(PathBuf::from("/home/u/.local/state/augur"))
        );
    }
}
