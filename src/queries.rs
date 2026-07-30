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

use anyhow::Context;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::cache::write_atomic;

/// A saved-query name, valid by construction: `FromStr` is the ONLY public
/// constructor and carries the former `is_valid_name` rule.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QueryName(String);

/// Rejection carrying the exact message `queries::set` used to produce.
#[derive(Debug, thiserror::Error)]
#[error("invalid query name {0:?} (expected letters, digits, '_', or '-' only)")]
pub struct InvalidQueryName(String);

impl std::str::FromStr for QueryName {
    type Err = InvalidQueryName;
    fn from_str(name: &str) -> Result<Self, Self::Err> {
        let valid = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if valid {
            Ok(QueryName(name.to_string()))
        } else {
            Err(InvalidQueryName(name.to_string()))
        }
    }
}

impl std::fmt::Display for QueryName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl AsRef<str> for QueryName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
impl std::borrow::Borrow<str> for QueryName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// The persisted saved queries, as read from and written to `queries.toml`:
/// a flat map of name to SQL text.
///
/// `#[serde(transparent)]` makes the file itself just `name = "sql"` lines at
/// the top level, rather than nesting the map under a wrapper key.
/// `Deserialize` stays non-validating (`QueryName`'s derived, transparent
/// impl) deliberately — a hand-edited `queries.toml` with an odd name still
/// loads exactly as before; only the write boundaries ([`set`], via
/// `QueryName::from_str`) reject one.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Queries(BTreeMap<QueryName, String>);

impl Queries {
    /// Every saved name, in sorted order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(QueryName::as_ref)
    }

    /// Every saved `(name, sql)` pair, in name-sorted order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(name, sql)| (name.as_ref(), sql.as_str()))
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

/// Saves `sql` under `name` in `queries`. `name` is already valid by
/// construction (`QueryName::from_str` is its only constructor), so this is
/// infallible. Overwrites any existing SQL already saved under the same name
/// (last-write-wins), like [`crate::config::set`].
pub fn set(queries: &mut Queries, name: QueryName, sql: &str) {
    queries.0.insert(name, sql.to_string());
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn round_trips() {
        let td = tempdir().unwrap();
        let p = td.path().join("queries.toml");
        let mut q = Queries::default();
        set(
            &mut q,
            "stale".parse().unwrap(),
            "SELECT file.name WHERE status='draft'",
        );
        save_to(&p, &q).unwrap();
        assert_eq!(load_from(&p).unwrap(), q);

        // Wire-shape pin: queries.toml stays flat `name = "sql"` lines — the
        // QueryName newtype must be invisible in the serialized form.
        let toml_text = std::fs::read_to_string(&p).unwrap();
        assert!(
            toml_text.contains("stale = "),
            "expected flat top-level key, got:\n{toml_text}"
        );
    }

    /// The character-class rule formerly checked by `set` on every write now
    /// lives entirely in `QueryName::from_str`, its only constructor.
    #[test]
    fn query_name_rejects_bad_chars_and_accepts_good_ones() {
        assert!("has space".parse::<QueryName>().is_err());
        assert!("ok-name_1".parse::<QueryName>().is_ok());
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

    #[test]
    fn set_overwrites_an_existing_name_last_write_wins() {
        let mut q = Queries::default();
        set(&mut q, "stale".parse().unwrap(), "SELECT 1");
        set(&mut q, "stale".parse().unwrap(), "SELECT 2");
        assert_eq!(get(&q, "stale"), Some("SELECT 2"));
    }

    #[test]
    fn remove_reports_whether_the_name_was_present() {
        let mut q = Queries::default();
        set(&mut q, "stale".parse().unwrap(), "SELECT 1");
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
        set(&mut q, "zeta".parse().unwrap(), "SELECT 1");
        set(&mut q, "alpha".parse().unwrap(), "SELECT 2");
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
