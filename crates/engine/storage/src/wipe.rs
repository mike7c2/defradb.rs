//! Wiping a closed node's store for a purge.
//!
//! The data directory is emptied (opened, every key dropped, closed), never
//! deleted; the other directories the node owns are removed; the kept paths
//! (the keyring) are never reached. `proofs/lean/StorePurge.lean` proves the
//! removal set against [`check`], and `proofs/tla/StorePurge.tla` the order:
//! the wipe runs only after the node closed, and the node opens only after it.

use std::path::{Component, Path, PathBuf};

use crate::corekv::{Error, Result};
use crate::{RegolithStore, RegolithStoreOptions};

/// What a purge wipes once the node is closed.
#[derive(Clone, Default)]
pub struct WipeTarget {
    /// The on-disk store and the options it was opened with, not placed on a
    /// pool (the node's pool is stopped by then); `None` for a memory store,
    /// which the close already dropped.
    pub store: Option<(PathBuf, RegolithStoreOptions)>,
    /// Directories the node owns besides the store, removed whole: the
    /// embedded Vera node's chain and journal.
    pub owned_dirs: Vec<PathBuf>,
    /// Paths no removal may equal or sit above: the keyring.
    pub kept: Vec<PathBuf>,
}

impl std::fmt::Debug for WipeTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WipeTarget")
            .field("store", &self.store.as_ref().map(|(path, _)| path))
            .field("owned_dirs", &self.owned_dirs)
            .field("kept", &self.kept)
            .finish()
    }
}

/// Refuses a target that would remove the data directory, a kept path or a
/// directory above either, before anything is touched. Paths are compared
/// with `.` and `..` resolved, so `root/../root/vera/keys` is seen inside
/// `root/vera`; nothing on disk is read, so an absent directory still passes.
pub fn check(target: &WipeTarget) -> Result<()> {
    let protected = target
        .kept
        .iter()
        .chain(target.store.as_ref().map(|(path, _)| path));
    for keep in protected {
        let resolved = lexical(keep);
        if let Some(dir) = target
            .owned_dirs
            .iter()
            .find(|dir| resolved.starts_with(lexical(dir)))
        {
            return Err(Error::Backend(format!(
                "refusing to wipe '{}': it holds '{}', which a purge keeps",
                dir.display(),
                keep.display()
            )));
        }
    }
    Ok(())
}

/// Wipes `target`: drops every key of the store on disk, then removes the
/// owned directories. Blocking; run it off the data path
/// (`Edge::spawn_blocking`) once the node has closed.
pub fn wipe(target: &WipeTarget) -> Result<()> {
    check(target)?;
    if let Some((path, options)) = &target.store {
        RegolithStore::open_with_options(path, options.clone())?.drop_all_and_close()?;
    }
    for dir in &target.owned_dirs {
        remove_dir(dir)?;
    }
    Ok(())
}

/// `path` with each `.` dropped and each `..` taking back the component
/// before it. A `..` with nothing to take back stays at the start of a
/// relative path and is dropped at the root, as the filesystem does.
fn lexical(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match resolved.components().next_back() {
                Some(Component::Normal(_)) => {
                    resolved.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => resolved.push(".."),
            },
            other => resolved.push(other),
        }
    }
    resolved
}

fn remove_dir(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(Error::Backend(format!(
            "failed to remove '{}': {error}",
            dir.display()
        ))),
        _ => Ok(()),
    }
}
