//! The user-global configuration file: its schema, its location, and the
//! read/modify/write operations both the `querymatter config` subcommand and
//! the REPL's `.set`/`.unset` are built on.
//!
//! Every field is optional. Absent means "fall through to the next precedence
//! layer" (see [`crate::settings`]), which is what makes `unset` expressible
//! and keeps the file as sparse as the user left it.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::cache::write_atomic;
use crate::discover;
use crate::render::{Format, TableStyle};

/// The persisted settings, as read from and written to `config.toml`.
///
/// `deny_unknown_fields` makes a typo'd key a hard error rather than a
/// silently ignored line — the same "reject loudly" rule the
/// `QUERYMATTER_TABLE_STYLE` environment variable already follows.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<Format>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table_style: Option<TableStyle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub respect_gitignore: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lenient: Option<bool>,
}

/// One configurable setting, named identically on the command line and in the
/// TOML file.
///
/// The `#[value(name = …)]` attributes force snake_case; `ValueEnum`'s default
/// would render `TableStyle` as `table-style`, disagreeing with the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum ConfigKey {
    #[value(name = "format")]
    Format,
    #[value(name = "table_style")]
    TableStyle,
    #[value(name = "ext")]
    Ext,
    #[value(name = "respect_gitignore")]
    RespectGitignore,
    #[value(name = "hidden")]
    Hidden,
    #[value(name = "exclude")]
    Exclude,
    #[value(name = "lenient")]
    Lenient,
}

/// What a [`ConfigKey`] accepts, for `config get` and for error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Allowed {
    /// Exactly one of these strings.
    OneOf(&'static [&'static str]),
    /// A comma-separated list of free-form strings.
    List,
}

impl fmt::Display for Allowed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Allowed::OneOf(values) => write!(f, "{}", values.join(", ")),
            Allowed::List => write!(f, "a comma-separated list"),
        }
    }
}

impl ConfigKey {
    /// Every key, in listing order.
    pub const ALL: [ConfigKey; 7] = [
        ConfigKey::Format,
        ConfigKey::TableStyle,
        ConfigKey::Ext,
        ConfigKey::RespectGitignore,
        ConfigKey::Hidden,
        ConfigKey::Exclude,
        ConfigKey::Lenient,
    ];

    /// The key's name, identical on the command line and in the TOML file.
    pub fn as_str(self) -> &'static str {
        match self {
            ConfigKey::Format => "format",
            ConfigKey::TableStyle => "table_style",
            ConfigKey::Ext => "ext",
            ConfigKey::RespectGitignore => "respect_gitignore",
            ConfigKey::Hidden => "hidden",
            ConfigKey::Exclude => "exclude",
            ConfigKey::Lenient => "lenient",
        }
    }

    /// The values this key accepts.
    pub fn allowed(self) -> Allowed {
        match self {
            ConfigKey::Format => Allowed::OneOf(&["table", "json", "csv", "tsv", "md"]),
            ConfigKey::TableStyle => Allowed::OneOf(&["ascii", "unicode", "compact", "plain"]),
            ConfigKey::RespectGitignore | ConfigKey::Hidden | ConfigKey::Lenient => {
                Allowed::OneOf(&["true", "false"])
            }
            ConfigKey::Ext | ConfigKey::Exclude => Allowed::List,
        }
    }
}

/// The config file's path, `<config_dir>/querymatter/config.toml`.
///
/// `None` when no home directory can be determined — readable as "no config",
/// but an error to write to (see [`save`]).
pub fn config_path() -> Option<PathBuf> {
    let dirs = ProjectDirs::from("", "", "querymatter")?;
    Some(dirs.config_dir().join("config.toml"))
}

/// Loads the user's config file, or [`Config::default`] when there is none.
pub fn load() -> anyhow::Result<Config> {
    match config_path() {
        Some(path) => load_from(&path),
        None => Ok(Config::default()),
    }
}

/// Loads `path`, treating a missing file as [`Config::default`].
///
/// A malformed file, an unknown key, or an invalid value is a hard error whose
/// message names `path` — that path is the user's only route to fixing it,
/// since a broken config blocks every command.
pub fn load_from(path: &Path) -> anyhow::Result<Config> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(err) => {
            return Err(err).with_context(|| format!("cannot read config file {}", path.display()));
        }
    };
    toml::from_str(&text).with_context(|| format!("invalid config file {}", path.display()))
}

/// Writes `config` to the user's config file, creating parent directories.
pub fn save(config: &Config) -> anyhow::Result<PathBuf> {
    let path = config_path().context("cannot determine a config directory for this user")?;
    save_to(&path, config)?;
    Ok(path)
}

/// Writes `config` to `path`, creating any missing parent directories.
///
/// Writes atomically (temp file in the same directory, then `rename` over
/// `path`, via [`write_atomic`]) so a process killed mid-write or a full disk
/// can never leave a truncated `config.toml` behind — since [`load_from`]
/// treats a malformed file as a hard error, a truncated file would otherwise
/// block every subsequent invocation until hand-fixed.
pub fn save_to(path: &Path, config: &Config) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create config directory {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(config).context("failed to serialize the config")?;
    write_atomic(path, text.as_bytes())
        .with_context(|| format!("cannot write config file {}", path.display()))
}

/// Sets `key` to `value` in `config`, validating first: a rejected value
/// leaves `config` untouched.
pub fn set(config: &mut Config, key: ConfigKey, value: &str) -> anyhow::Result<()> {
    match key {
        ConfigKey::Format => config.format = Some(parse_enum(key, value)?),
        ConfigKey::TableStyle => config.table_style = Some(parse_enum(key, value)?),
        ConfigKey::RespectGitignore => config.respect_gitignore = Some(parse_bool(key, value)?),
        ConfigKey::Hidden => config.hidden = Some(parse_bool(key, value)?),
        ConfigKey::Ext => config.ext = Some(split_list(value)),
        ConfigKey::Exclude => config.exclude = Some(parse_exclude_list(value)?),
        ConfigKey::Lenient => config.lenient = Some(parse_bool(key, value)?),
    }
    Ok(())
}

/// Splits `value` into glob patterns and validates each one via
/// [`discover::validate_excludes`], so a `config set exclude` with a pattern
/// `globset` cannot compile is rejected here — at write time, naming the
/// offending pattern — rather than silently doing nothing at scan time
/// (`discover`'s own glob compiler has no error channel back to its caller
/// and drops what doesn't compile).
fn parse_exclude_list(value: &str) -> anyhow::Result<Vec<String>> {
    let patterns = split_list(value);
    discover::validate_excludes(&patterns)?;
    Ok(patterns)
}

/// Removes `key` from `config`, returning it to the next precedence layer.
pub fn unset(config: &mut Config, key: ConfigKey) {
    match key {
        ConfigKey::Format => config.format = None,
        ConfigKey::TableStyle => config.table_style = None,
        ConfigKey::RespectGitignore => config.respect_gitignore = None,
        ConfigKey::Hidden => config.hidden = None,
        ConfigKey::Ext => config.ext = None,
        ConfigKey::Exclude => config.exclude = None,
        ConfigKey::Lenient => config.lenient = None,
    }
}

/// `config`'s value for `key`, spelled the way [`set`] accepts it, or `None`
/// when the key is absent from the file.
pub fn get(config: &Config, key: ConfigKey) -> Option<String> {
    match key {
        ConfigKey::Format => config.format.map(|f| enum_name(&f)),
        ConfigKey::TableStyle => config.table_style.map(|s| enum_name(&s)),
        ConfigKey::RespectGitignore => config.respect_gitignore.map(|b| b.to_string()),
        ConfigKey::Hidden => config.hidden.map(|b| b.to_string()),
        ConfigKey::Ext => config.ext.as_ref().map(|list| list.join(",")),
        ConfigKey::Exclude => config.exclude.as_ref().map(|list| list.join(",")),
        ConfigKey::Lenient => config.lenient.map(|b| b.to_string()),
    }
}

/// Parses a closed-set value, reporting the allowed ones on failure.
fn parse_enum<T: std::str::FromStr>(key: ConfigKey, value: &str) -> anyhow::Result<T> {
    value.parse().map_err(|_| {
        anyhow::anyhow!(
            "invalid {} value {value:?} (expected {})",
            key.as_str(),
            key.allowed()
        )
    })
}

/// Parses `true`/`false`, case-insensitively.
fn parse_bool(key: ConfigKey, value: &str) -> anyhow::Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => bail!(
            "invalid {} value {value:?} (expected {})",
            key.as_str(),
            key.allowed()
        ),
    }
}

/// Splits a comma-separated list, trimming each entry and dropping blanks.
fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect()
}

/// The name a `ValueEnum` uses for `value`, which is also its TOML spelling.
fn enum_name<T: clap::ValueEnum>(value: &T) -> String {
    value
        .to_possible_value()
        .expect("no Format or TableStyle variant is skipped")
        .get_name()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    // `to_possible_value()` is a `ValueEnum` method; the trait must be in scope.
    use clap::ValueEnum;
    use std::collections::BTreeSet;
    use tempfile::tempdir;

    #[test]
    fn missing_file_is_default_not_error() {
        let td = tempdir().unwrap();
        let path = td.path().join("nope.toml");
        assert_eq!(load_from(&path).unwrap(), Config::default());
    }

    #[test]
    fn round_trips_through_the_file() {
        let td = tempdir().unwrap();
        let path = td.path().join("config.toml");
        let mut config = Config::default();
        set(&mut config, ConfigKey::TableStyle, "unicode").unwrap();
        set(&mut config, ConfigKey::Ext, "md,mdx").unwrap();
        set(&mut config, ConfigKey::Hidden, "true").unwrap();
        save_to(&path, &config).unwrap();
        assert_eq!(load_from(&path).unwrap(), config);
    }

    /// Writing back must not materialize keys the user never set, so the file
    /// stays as sparse as they left it.
    #[test]
    fn save_omits_unset_keys() {
        let td = tempdir().unwrap();
        let path = td.path().join("config.toml");
        let mut config = Config::default();
        set(&mut config, ConfigKey::TableStyle, "unicode").unwrap();
        save_to(&path, &config).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("table_style"), "got:\n{text}");
        assert!(
            !text.contains("format"),
            "unset keys must be absent, got:\n{text}"
        );
        assert!(
            !text.contains("hidden"),
            "unset keys must be absent, got:\n{text}"
        );
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let td = tempdir().unwrap();
        let path = td.path().join("a").join("b").join("config.toml");
        save_to(&path, &Config::default()).unwrap();
        assert!(path.is_file());
    }

    /// `save_to` writes atomically (temp file + rename, [`write_atomic`]):
    /// an already-existing, valid config file must still be intact and
    /// parseable after a subsequent `save_to` call, with no stray temp file
    /// left behind. Guards against the truncated-file failure mode a plain
    /// `fs::write` risks on a kill or a full disk mid-write.
    #[test]
    fn save_to_leaves_an_intact_parseable_file_with_no_stray_temp_file() {
        let td = tempdir().unwrap();
        let path = td.path().join("config.toml");
        let mut original = Config::default();
        set(&mut original, ConfigKey::TableStyle, "unicode").unwrap();
        save_to(&path, &original).unwrap();

        let mut updated = Config::default();
        set(&mut updated, ConfigKey::Format, "json").unwrap();
        save_to(&path, &updated).unwrap();

        assert_eq!(load_from(&path).unwrap(), updated);

        let stray_files: Vec<_> = std::fs::read_dir(td.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name() != path.file_name().unwrap())
            .collect();
        assert!(
            stray_files.is_empty(),
            "leftover temp file: {stray_files:?}"
        );
    }

    #[test]
    fn malformed_file_errors_naming_the_path() {
        let td = tempdir().unwrap();
        let path = td.path().join("config.toml");
        std::fs::write(&path, "this is not = = toml").unwrap();
        let err = format!("{:#}", load_from(&path).unwrap_err());
        assert!(
            err.contains("config.toml"),
            "must name the file, got: {err}"
        );
    }

    /// A typo'd key must not silently do nothing.
    #[test]
    fn unknown_key_errors_naming_the_path_and_key() {
        let td = tempdir().unwrap();
        let path = td.path().join("config.toml");
        std::fs::write(&path, "tabel_style = \"unicode\"\n").unwrap();
        let err = format!("{:#}", load_from(&path).unwrap_err());
        assert!(
            err.contains("config.toml"),
            "must name the file, got: {err}"
        );
        assert!(err.contains("tabel_style"), "must name the key, got: {err}");
    }

    #[test]
    fn invalid_value_in_file_errors() {
        let td = tempdir().unwrap();
        let path = td.path().join("config.toml");
        std::fs::write(&path, "table_style = \"fancy\"\n").unwrap();
        assert!(load_from(&path).is_err());
    }

    #[test]
    fn set_rejects_an_invalid_value_naming_the_allowed_ones() {
        let mut config = Config::default();
        let err = format!(
            "{:#}",
            set(&mut config, ConfigKey::TableStyle, "fancy").unwrap_err()
        );
        assert!(err.contains("fancy"), "must name the bad value, got: {err}");
        assert!(
            err.contains("unicode"),
            "must name the allowed values, got: {err}"
        );
        assert_eq!(config, Config::default(), "a rejected set must not mutate");
    }

    /// The enum case (`table_style = "fancy"`, above) is guarded by
    /// `parse_enum` returning before any assignment. The boolean keys go
    /// through a separate function, `parse_bool`, and are safe today only by
    /// the same `?`-short-circuit ordering — an invariant a later refactor of
    /// `set`'s match arms could break silently. Pinned explicitly per this
    /// project's spec-discipline rule against skipping a test because a
    /// current invariant "makes it safe".
    #[test]
    fn set_rejects_a_non_boolean_leaving_config_untouched() {
        let mut config = Config::default();
        let before = config.clone();
        assert!(set(&mut config, ConfigKey::Hidden, "yes").is_err());
        assert_eq!(config, before);
    }

    #[test]
    fn set_rejects_a_non_boolean_respect_gitignore_leaving_config_untouched() {
        let mut config = Config::default();
        let before = config.clone();
        assert!(set(&mut config, ConfigKey::RespectGitignore, "yes").is_err());
        assert_eq!(config, before);
    }

    /// A `config set exclude` with a glob `globset` cannot compile must be
    /// rejected up front, naming the offending pattern, and must not mutate
    /// `config` — otherwise a config-supplied `exclude` silently does
    /// nothing at scan time instead of failing loudly (IMPORTANT 1).
    #[test]
    fn set_rejects_an_invalid_exclude_glob_leaving_config_untouched() {
        let mut config = Config::default();
        let before = config.clone();
        let err = format!(
            "{:#}",
            set(&mut config, ConfigKey::Exclude, "[plans/**").unwrap_err()
        );
        assert!(
            err.contains("[plans/**"),
            "must name the bad pattern, got: {err}"
        );
        assert_eq!(config, before, "a rejected set must not mutate");
    }

    #[test]
    fn set_accepts_a_mix_of_valid_exclude_globs() {
        let mut config = Config::default();
        set(
            &mut config,
            ConfigKey::Exclude,
            "**/templates/**,*.draft.md",
        )
        .unwrap();
        assert_eq!(
            config.exclude,
            Some(vec![
                "**/templates/**".to_string(),
                "*.draft.md".to_string()
            ])
        );
    }

    #[test]
    fn set_parses_booleans_case_insensitively() {
        let mut config = Config::default();
        set(&mut config, ConfigKey::Hidden, "TRUE").unwrap();
        assert_eq!(config.hidden, Some(true));
        set(&mut config, ConfigKey::Hidden, "False").unwrap();
        assert_eq!(config.hidden, Some(false));
    }

    #[test]
    fn set_splits_lists_on_commas_dropping_blanks() {
        let mut config = Config::default();
        set(&mut config, ConfigKey::Ext, " md , markdown ,, ").unwrap();
        assert_eq!(
            config.ext,
            Some(vec!["md".to_string(), "markdown".to_string()])
        );
    }

    #[test]
    fn unset_returns_the_key_to_absent() {
        let mut config = Config::default();
        set(&mut config, ConfigKey::TableStyle, "unicode").unwrap();
        unset(&mut config, ConfigKey::TableStyle);
        assert_eq!(config, Config::default());
    }

    /// `get` renders the value in the same spelling `set` accepts, so
    /// `config get` output can be fed straight back to `config set`.
    #[test]
    fn get_round_trips_through_set() {
        let mut config = Config::default();
        for (key, value) in [
            (ConfigKey::Format, "json"),
            (ConfigKey::TableStyle, "unicode"),
            (ConfigKey::Ext, "md,mdx"),
            (ConfigKey::RespectGitignore, "true"),
            (ConfigKey::Hidden, "false"),
            (ConfigKey::Exclude, "**/x/**,**/y/**"),
            (ConfigKey::Lenient, "true"),
        ] {
            set(&mut config, key, value).unwrap();
            assert_eq!(get(&config, key).as_deref(), Some(value), "for {key:?}");
        }
    }

    #[test]
    fn get_is_none_for_an_unset_key() {
        assert_eq!(get(&Config::default(), ConfigKey::Format), None);
    }

    /// The command-line spelling of every key must equal its TOML spelling.
    #[test]
    fn config_key_cli_names_match_toml_names() {
        for key in <ConfigKey as clap::ValueEnum>::value_variants() {
            let possible = key.to_possible_value().expect("no variant is skipped");
            assert_eq!(possible.get_name(), key.as_str(), "for {key:?}");
        }
    }

    /// `ConfigKey::ALL` is hand-written while `value_variants()` is
    /// macro-derived from the enum itself; a new variant added without
    /// updating `ALL` would compile clean (the exhaustive matches in
    /// `as_str`/`allowed`/`set`/`unset`/`get` force *those* to be updated,
    /// but nothing forces `ALL`) and silently drop the key from any listing
    /// built from `ALL`. Pinning the two in agreement catches that drift.
    #[test]
    fn all_agrees_with_value_variants() {
        let variants = <ConfigKey as clap::ValueEnum>::value_variants();
        assert_eq!(
            ConfigKey::ALL.len(),
            variants.len(),
            "ConfigKey::ALL and value_variants() must have the same length"
        );
        let all_set: BTreeSet<ConfigKey> = ConfigKey::ALL.into_iter().collect();
        let variants_set: BTreeSet<ConfigKey> = variants.iter().copied().collect();
        assert_eq!(
            all_set, variants_set,
            "ConfigKey::ALL and value_variants() must name the same keys"
        );
    }

    #[test]
    fn allowed_lists_the_enum_values() {
        assert_eq!(
            ConfigKey::TableStyle.allowed().to_string(),
            "ascii, unicode, compact, plain"
        );
        assert_eq!(ConfigKey::Hidden.allowed().to_string(), "true, false");
        assert_eq!(
            ConfigKey::Ext.allowed().to_string(),
            "a comma-separated list"
        );
    }
}
