//! Finding the repository a directory belongs to.

use std::path::{Path, PathBuf};

/// Finds the repository a directory belongs to.
pub trait RepoLookup {
    /// Root of the repository containing `dir`, if any.
    fn repo_root(&self, dir: &Path) -> Option<PathBuf>;
}

/// Finds git repositories: the nearest ancestor holding a `.git` entry.
#[derive(Debug, Clone, Copy, Default)]
pub struct GitRepos;

impl RepoLookup for GitRepos {
    fn repo_root(&self, dir: &Path) -> Option<PathBuf> {
        dir.ancestors()
            .find(|ancestor| ancestor.join(".git").exists())
            .map(Path::to_path_buf)
    }
}

/// Knows no repositories; for tests and replays that should ignore them.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoRepos;

impl RepoLookup for NoRepos {
    fn repo_root(&self, _: &Path) -> Option<PathBuf> {
        None
    }
}
