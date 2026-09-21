//! Names of the commands the shell can run.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

const RESCAN_AFTER: Duration = Duration::from_secs(60);

/// Executables on `$PATH`, plus the functions, aliases, builtins and keywords
/// the shell reports.
#[derive(Debug, Default)]
pub struct Commands {
    path: Option<OsString>,
    executables: BTreeSet<String>,
    shell: BTreeSet<String>,
    scanned: Option<Instant>,
}

impl Commands {
    /// The commands found through `path`, a `$PATH`-style list of directories.
    pub fn new(path: Option<OsString>) -> Self {
        Self {
            path,
            ..Self::default()
        }
    }

    /// Replaces the names the shell defines itself.
    pub fn set_shell_names(&mut self, names: impl IntoIterator<Item = String>) {
        self.shell = names.into_iter().collect();
    }

    /// Every known command name. `$PATH` is scanned again once a minute.
    pub fn names(&mut self) -> impl Iterator<Item = &str> {
        if self.scanned.is_none_or(|at| at.elapsed() > RESCAN_AFTER) {
            self.executables = scan(self.path.as_deref());
            self.scanned = Some(Instant::now());
        }
        self.executables
            .iter()
            .chain(&self.shell)
            .map(String::as_str)
    }
}

fn scan(path: Option<&OsStr>) -> BTreeSet<String> {
    let Some(path) = path else {
        return BTreeSet::new();
    };
    std::env::split_paths(path)
        .filter_map(|dir| fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .filter(|entry| is_executable(&entry.path()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect()
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use std::fs::Permissions;

    use super::*;

    #[test]
    fn lists_executables_on_the_path_and_shell_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = dir.path().join("mytool");
        fs::write(&tool, "#!/bin/sh\n").expect("write tool");
        fs::set_permissions(&tool, Permissions::from_mode(0o755)).expect("chmod");
        fs::write(dir.path().join("notes.txt"), "not a command").expect("write notes");
        fs::create_dir(dir.path().join("subdir")).expect("mkdir");

        let mut commands = Commands::new(Some(dir.path().as_os_str().to_owned()));
        commands.set_shell_names(["bleopt".to_owned()]);
        let names: Vec<&str> = commands.names().collect();
        assert_eq!(names, ["mytool", "bleopt"]);
    }
}
