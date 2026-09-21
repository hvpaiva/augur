//! Telling whether a word names a file, and whether that file is there.
//!
//! The history remembers file names as it remembers any other word, but a file
//! that was there when a command ran need not be here now. A word that names a
//! path is only worth suggesting when the path exists.

use std::path::{Path, PathBuf};

/// Longest file extension, in characters, that makes a bare word a file name.
const MAX_EXTENSION: usize = 5;

/// Looks paths up.
pub trait FileLookup {
    /// Whether something is at `path`; a dangling symbolic link counts.
    fn exists(&self, path: &Path) -> bool;
}

/// The file system.
#[derive(Debug, Clone, Copy, Default)]
pub struct Disk;

impl FileLookup for Disk {
    fn exists(&self, path: &Path) -> bool {
        path.symlink_metadata().is_ok()
    }
}

/// The path `word` names when typed in `cwd`, when it clearly names one: it
/// holds a `/`, starts with `~` or `.`, or ends in a file extension.
///
/// `None` when the word is something else (an option, a subcommand, a branch)
/// or cannot be resolved without the shell (a variable, a glob, another user's
/// home, `host:path`, `key=value`); such words are not for this module to judge.
pub fn path_of(word: &str, cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let name = literal(word)?;
    if name.starts_with('-') || !looks_like_path(&name) {
        return None;
    }
    if name == "~" {
        return home.map(Path::to_path_buf);
    }
    if let Some(rest) = name.strip_prefix("~/") {
        return home.map(|home| home.join(rest));
    }
    if name.starts_with('~') {
        return None;
    }
    Some(cwd.join(name))
}

/// `word` as the shell would read it, when that takes no expansion: quotes
/// around it and backslashes are dropped, and what is left must be made of
/// characters that mean nothing to the shell.
fn literal(word: &str) -> Option<String> {
    let quoted = ['\'', '"'].into_iter().find_map(|quote| {
        word.strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
    });
    let name = match quoted {
        Some(inner) => inner.to_owned(),
        None => word.replace('\\', ""),
    };
    let plain =
        |c: char| c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '~' | '+' | ',' | ' ');
    (!name.is_empty() && name.chars().all(plain)).then_some(name)
}

fn looks_like_path(name: &str) -> bool {
    if name.contains('/') || name.starts_with('~') || name.starts_with('.') {
        return true;
    }
    // A version number, as in `v1.2.3`, ends in digits only.
    name.rsplit_once('.').is_some_and(|(stem, extension)| {
        !stem.is_empty()
            && (1..=MAX_EXTENSION).contains(&extension.chars().count())
            && extension.chars().all(char::is_alphanumeric)
            && extension.chars().any(char::is_alphabetic)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(word: &str) -> Option<PathBuf> {
        path_of(word, Path::new("/work"), Some(Path::new("/home/u")))
    }

    #[test]
    fn resolves_words_that_name_paths() {
        assert_eq!(path("status.txt"), Some(PathBuf::from("/work/status.txt")));
        assert_eq!(
            path("src/main.rs"),
            Some(PathBuf::from("/work/src/main.rs"))
        );
        assert_eq!(path("./run"), Some(PathBuf::from("/work/./run")));
        assert_eq!(path(".env"), Some(PathBuf::from("/work/.env")));
        assert_eq!(path("/etc/hosts"), Some(PathBuf::from("/etc/hosts")));
        assert_eq!(path("~/ux.txt"), Some(PathBuf::from("/home/u/ux.txt")));
        assert_eq!(path("~"), Some(PathBuf::from("/home/u")));
    }

    #[test]
    fn reads_quotes_and_escapes_as_the_shell_does() {
        assert_eq!(
            path("my\\ notes.md"),
            Some(PathBuf::from("/work/my notes.md"))
        );
        assert_eq!(
            path("'my notes.md'"),
            Some(PathBuf::from("/work/my notes.md"))
        );
    }

    #[test]
    fn leaves_other_words_alone() {
        for word in [
            "pods",
            "main",
            "-la",
            "--config=app.toml",
            "v1.2.3",
            "docker.service",
            "$HOME/notes.txt",
            "*.rs",
            "src/{a,b}.rs",
            "host:notes.txt",
            "https://example.com/a.txt",
            "user@example.com",
            "~other/notes.txt",
            "",
        ] {
            assert_eq!(path(word), None, "{word:?}");
        }
    }

    #[test]
    fn needs_a_home_to_resolve_a_tilde() {
        assert_eq!(path_of("~/a.txt", Path::new("/work"), None), None);
    }
}
