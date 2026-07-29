//! Role-distinct wrappers for the three path meanings the cache/discover
//! pipeline threads around, so a vault root, a containing directory, and a
//! concrete file can no longer be swapped at a call site.
//!
//! Each type is a thin, infallible wrapper — there is no parse-time
//! validation to fail, only role tagging — so every constructor is `new`,
//! never `TryFrom`. `Deref<Target = Path>`/`AsRef<Path>` mean a caller that
//! merely wants to read the underlying path (`.display()`, `.join()`,
//! `.starts_with()`, …) never has to unwrap explicitly; only a caller that
//! needs an *owned* `PathBuf` (a `BTreeMap` key, a `CachedFile`/`CachedDir`
//! field of that type) calls [`VaultRoot::as_path`]/[`DirPath::as_path`]/
//! [`FilePath::as_path`] (or `.to_path_buf()` via deref) explicitly.
//!
//! None of the three derives `Ord`/`Hash`: nothing here needs to key a
//! `BTreeMap`/`HashMap` by role — internal groupings key by the plain
//! `PathBuf` extracted via `as_path().to_path_buf()`, then re-wrap only at
//! the point a role-typed value is actually needed again.

use std::ops::Deref;
use std::path::{Path, PathBuf};

/// The vault / scan root every `Record`'s `file.*` attrs are resolved against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultRoot(PathBuf);

/// The immediate containing directory a `CachedFile::rel_path` is relative to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct DirPath(PathBuf);

/// One concrete markdown file on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePath(PathBuf);

impl VaultRoot {
    pub fn new(path: PathBuf) -> Self {
        VaultRoot(path)
    }
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}
impl AsRef<Path> for VaultRoot {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}
impl Deref for VaultRoot {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl DirPath {
    pub fn new(path: PathBuf) -> Self {
        DirPath(path)
    }
    pub fn as_path(&self) -> &Path {
        &self.0
    }
    /// The explicit conversion for the one call site (`store::scan_root`)
    /// that legitimately scans files directly under the vault root rather
    /// than resolving each file's immediate parent directory — see its call
    /// site for why that's intentional, not a bug.
    pub fn from_root(root: &VaultRoot) -> DirPath {
        DirPath(root.as_path().to_path_buf())
    }
}
impl AsRef<Path> for DirPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}
impl Deref for DirPath {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl FilePath {
    pub fn new(path: PathBuf) -> Self {
        FilePath(path)
    }
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}
impl AsRef<Path> for FilePath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}
impl Deref for FilePath {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}
