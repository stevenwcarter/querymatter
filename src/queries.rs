//! Saved named queries: the `queries.toml` file, its schema, and the
//! read/modify/write operations both the `querymatter query` subcommand and
//! the REPL's `.query run`/`.query list` are built on.
//!
//! Mirrors [`crate::config`]'s load/save/malformed-error discipline, but for
//! a SEPARATE file — `queries.toml`, sitting alongside `config.toml` under
//! the same `querymatter` config directory, never merged into it.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::cache::write_atomic;

/// The persisted saved queries, as read from and written to `queries.toml`:
/// a flat map of name to SQL text.
///
/// `#[serde(transparent)]` makes the file itself just `name = "sql"` lines at
/// the top level, rather than nesting the map under a wrapper key.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Queries(BTreeMap<String, String>);

impl Queries {
    /// Every saved name, in sorted order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// Every saved `(name, sql)` pair, in name-sorted order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(name, sql)| (name.as_str(), sql.as_str()))
    }
}

/// The saved-queries file's path, `<config_dir>/querymatter/queries.toml` —
/// the same `querymatter` config directory `config.toml` lives in, via the
/// same [`ProjectDirs::from`] lookup, but a distinct file.
///
/// `None` when no home directory can be determined — readable as "no saved
/// queries", but an error to write to (see [`save`]).
pub fn queries_path() -> Option<PathBuf> {
    let dirs = ProjectDirs::from("", "", "querymatter")?;
    Some(dirs.config_dir().join("queries.toml"))
}

/// Loads the user's saved queries, or [`Queries::default`] when there are none.
pub fn load() -> anyhow::Result<Queries> {
    match queries_path() {
        Some(path) => load_from(&path),
        None => Ok(Queries::default()),
    }
}

/// Loads `path`, treating a missing file as [`Queries::default`].
///
/// A malformed file is a hard error whose message names `path` — that path is
/// the user's only route to fixing it by hand.
pub fn load_from(path: &Path) -> anyhow::Result<Queries> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Queries::default()),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("cannot read queries file {}", path.display()));
        }
    };
    toml::from_str(&text).with_context(|| format!("invalid queries file {}", path.display()))
}

/// Writes `queries` to the user's saved-queries file, creating parent
/// directories.
pub fn save(queries: &Queries) -> anyhow::Result<PathBuf> {
    let path = queries_path().context("cannot determine a config directory for this user")?;
    save_to(&path, queries)?;
    Ok(path)
}

/// Writes `queries` to `path`, creating any missing parent directories.
///
/// Writes atomically (temp file in the same directory, then `rename` over
/// `path`, via [`write_atomic`]) for the same reason [`crate::config::save_to`]
/// does: a process killed mid-write must never leave a truncated, unparseable
/// `queries.toml` behind.
pub fn save_to(path: &Path, queries: &Queries) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create config directory {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(queries).context("failed to serialize the saved queries")?;
    write_atomic(path, text.as_bytes())
        .with_context(|| format!("cannot write queries file {}", path.display()))
}

/// Saves `sql` under `name` in `queries`, validating `name` first: a rejected
/// name leaves `queries` untouched. Overwrites any existing SQL already saved
/// under the same name (last-write-wins), like [`crate::config::set`].
pub fn set(queries: &mut Queries, name: &str, sql: &str) -> anyhow::Result<()> {
    ensure!(
        is_valid_name(name),
        "invalid query name {name:?} (expected letters, digits, '_', or '-' only)"
    );
    queries.0.insert(name.to_string(), sql.to_string());
    Ok(())
}

/// Removes `name` from `queries`, returning whether it had actually been
/// present — so a caller (`query delete`, `config unset`'s sibling) can
/// report "removed" only when something really changed.
pub fn remove(queries: &mut Queries, name: &str) -> bool {
    queries.0.remove(name).is_some()
}

/// `name`'s saved SQL, or `None` when no query is saved under that name.
pub fn get<'a>(queries: &'a Queries, name: &str) -> Option<&'a str> {
    queries.0.get(name).map(String::as_str)
}

/// Whether `name` matches the allowed saved-query name pattern
/// (`^[A-Za-z0-9_-]+$`) — checked by hand rather than pulling in a compiled
/// regex for so small and fixed a character class.
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn round_trips() {
        let td = tempdir().unwrap();
        let p = td.path().join("queries.toml");
        let mut q = Queries::default();
        set(&mut q, "stale", "SELECT file.name WHERE status='draft'").unwrap();
        save_to(&p, &q).unwrap();
        assert_eq!(load_from(&p).unwrap(), q);
    }

    #[test]
    fn rejects_bad_name() {
        let mut q = Queries::default();
        assert!(set(&mut q, "has space", "SELECT 1").is_err());
        assert!(set(&mut q, "ok-name_1", "SELECT 1").is_ok());
    }

    #[test]
    fn missing_file_is_empty_malformed_errors() {
        let td = tempdir().unwrap();
        assert_eq!(
            load_from(&td.path().join("nope.toml")).unwrap(),
            Queries::default()
        );
        let p = td.path().join("q.toml");
        std::fs::write(&p, "= = broken").unwrap();
        assert!(load_from(&p).unwrap_err().to_string().contains("q.toml"));
    }

    /// A rejected `set` (a bad name) must not mutate `queries` — mirrors
    /// `config::set`'s same guarantee.
    #[test]
    fn rejected_set_leaves_queries_untouched() {
        let mut q = Queries::default();
        set(&mut q, "kept", "SELECT 1").unwrap();
        let before = q.clone();
        assert!(set(&mut q, "bad name", "SELECT 2").is_err());
        assert_eq!(q, before);
    }

    #[test]
    fn set_overwrites_an_existing_name_last_write_wins() {
        let mut q = Queries::default();
        set(&mut q, "stale", "SELECT 1").unwrap();
        set(&mut q, "stale", "SELECT 2").unwrap();
        assert_eq!(get(&q, "stale"), Some("SELECT 2"));
    }

    #[test]
    fn remove_reports_whether_the_name_was_present() {
        let mut q = Queries::default();
        set(&mut q, "stale", "SELECT 1").unwrap();
        assert!(remove(&mut q, "stale"));
        assert!(
            !remove(&mut q, "stale"),
            "already removed, must report false"
        );
        assert!(!remove(&mut q, "never-set"));
    }

    #[test]
    fn get_is_none_for_an_unknown_name() {
        assert_eq!(get(&Queries::default(), "nope"), None);
    }

    #[test]
    fn names_and_iter_are_name_sorted() {
        let mut q = Queries::default();
        set(&mut q, "zeta", "SELECT 1").unwrap();
        set(&mut q, "alpha", "SELECT 2").unwrap();
        assert_eq!(q.names().collect::<Vec<_>>(), vec!["alpha", "zeta"]);
        assert_eq!(
            q.iter().collect::<Vec<_>>(),
            vec![("alpha", "SELECT 2"), ("zeta", "SELECT 1")]
        );
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let td = tempdir().unwrap();
        let path = td.path().join("a").join("b").join("queries.toml");
        save_to(&path, &Queries::default()).unwrap();
        assert!(path.is_file());
    }
}
