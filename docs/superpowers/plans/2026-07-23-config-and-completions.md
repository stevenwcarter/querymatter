# Config File, Config Commands, and Shell Completions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a user-global TOML config file with `flag > env > config > default` precedence, CLI and REPL commands to list/inspect/set/unset settings, and shell completion scripts.

**Architecture:** Two new modules — `src/config.rs` (the TOML schema, its file I/O, and a `ConfigKey` enum naming the six settings) and `src/settings.rs` (a `Settings` struct of `Resolved<T>` values, each carrying the `Source` layer it came from). The valued CLI flags become `Option<T>` so a config value can outrank a clap-supplied default, and `main` parses via `ArgMatches` so `Source::Flag` and `Source::Env` stay distinguishable. Both the `querymatter config` subcommand and the REPL's `.settings`/`.set`/`.unset` call the same two modules.

**Tech Stack:** Rust edition 2024, `clap` 4 derive (+ `env` feature, + `ValueEnum`), `clap_complete` 4.6, `toml` 1.1, `serde`, `directories`, `insta`, `assert_cmd` + `predicates`.

**Spec:** `docs/superpowers/specs/2026-07-23-config-and-completions-design.md`

## Global Constraints

- Edition 2024. Every file must stay `cargo fmt --check` clean and `cargo clippy --all-targets -- -D warnings` clean.
- **Default output must not change by a single byte.** The committed snapshots `src/snapshots/querymatter__render__tests__table_snapshot.snap` and `..._md_snapshot.snap` must stay byte-identical to `main`. Verify with `git diff main -- <those paths>` before every commit.
- Precedence, per key, independently: **`flag > env > config > default`**.
- `Settings::default()` is the **single source of truth** for the built-in defaults: `Format::Table`, `TableStyle::Ascii`, `ext = ["md", "markdown"]`, `respect_gitignore = false`, `hidden = false`, `exclude = []`.
- The six config keys, spelled **snake_case** in both the TOML file and the commands: `format`, `table_style`, `ext`, `respect_gitignore`, `hidden`, `exclude`.
- A missing config file is **not** an error. A malformed file, an unknown key, or an invalid value is a **hard error naming the file path**.
- The config file is read **exactly once**, at startup.
- stdout carries data only (query results, `config list` rows, `config get`/`config path` output, completion scripts). Confirmations, notes, warnings, and errors go to stderr.
- `Format`'s and `TableStyle`'s `FromStr` impls are **retained** — the REPL parses free-text words, not clap arguments.
- This is a binary-only crate (no `src/lib.rs`): use `cargo test <filter>`, never `cargo test --lib <filter>`. `cargo-insta` is **not installed**; use `INSTA_UPDATE=always cargo test` or `INSTA_FORCE_UPDATE=1 cargo test`.
- This repo has no pre-commit hook. Run `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings` yourself before every commit.

---

### Task 1: `ValueEnum` and serde on `Format` and `TableStyle`

**Files:**
- Modify: `Cargo.toml` (no new deps this task — `clap` and `serde` are already present)
- Modify: `src/render.rs` (derives on `Format` and `TableStyle`, tests)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `Format` and `TableStyle` additionally derive `clap::ValueEnum`, `serde::Serialize`, `serde::Deserialize`.
  - Their `ValueEnum` names and serde spellings are exactly: `Format` → `table`, `json`, `csv`, `tsv`, `md`; `TableStyle` → `ascii`, `unicode`, `compact`, `plain`.
  - Both keep their existing `FromStr` impls unchanged.

- [ ] **Step 1: Write the failing tests**

Add `use clap::ValueEnum;` to the `tests` module in `src/render.rs` (the
`to_possible_value()` method calls below need the trait in scope), then add:

```rust
    /// Every value clap will offer as a completion must also parse through
    /// `FromStr`, which is what the REPL's `.format`/`.style`/`.set` use. If
    /// these ever diverge, a value you can tab-complete becomes a value the
    /// REPL rejects.
    #[test]
    fn format_value_enum_agrees_with_from_str() {
        for variant in <Format as clap::ValueEnum>::value_variants() {
            let possible = variant.to_possible_value().expect("no variant is skipped");
            let name = possible.get_name();
            assert_eq!(
                name.parse::<Format>().expect("clap's name must parse"),
                *variant,
                "clap offers {name:?} but FromStr disagrees"
            );
        }
    }

    #[test]
    fn table_style_value_enum_agrees_with_from_str() {
        for variant in <TableStyle as clap::ValueEnum>::value_variants() {
            let possible = variant.to_possible_value().expect("no variant is skipped");
            let name = possible.get_name();
            assert_eq!(
                name.parse::<TableStyle>().expect("clap's name must parse"),
                *variant,
                "clap offers {name:?} but FromStr disagrees"
            );
        }
    }

    /// The TOML spelling must match the CLI spelling exactly, so a config file
    /// and a command line never disagree about what "md" means.
    #[test]
    fn format_serde_spelling_matches_cli() {
        assert_eq!(toml_value(&Format::Md), "md");
        assert_eq!(toml_value(&Format::Table), "table");
        assert_eq!(toml_value(&Format::Json), "json");
        assert_eq!(toml_value(&Format::Csv), "csv");
        assert_eq!(toml_value(&Format::Tsv), "tsv");
    }

    #[test]
    fn table_style_serde_spelling_matches_cli() {
        assert_eq!(toml_value(&TableStyle::Ascii), "ascii");
        assert_eq!(toml_value(&TableStyle::Unicode), "unicode");
        assert_eq!(toml_value(&TableStyle::Compact), "compact");
        assert_eq!(toml_value(&TableStyle::Plain), "plain");
    }

    /// Serializes `value` the way it will appear in `config.toml`.
    fn toml_value<T: serde::Serialize>(value: &T) -> String {
        serde_json::to_value(value)
            .expect("these enums serialize as plain strings")
            .as_str()
            .expect("as a JSON string")
            .to_string()
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test render::tests::format_value_enum`
Expected: FAIL — `the trait bound Format: clap::ValueEnum is not satisfied`.

- [ ] **Step 3: Add the derives**

In `src/render.rs`, change `Format`'s derive line and add the serde rename:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
```

and `TableStyle`'s:

```rust
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum TableStyle {
```

`clap::ValueEnum`'s derive renames variants to kebab-case by default, which for
these single-word variants is exactly the lowercase spelling `FromStr` already
accepts — no per-variant `#[value(name = …)]` is needed. Do not remove the
`#[default]` attribute on `TableStyle::Ascii`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test render::`
Expected: PASS, every test in the module.

- [ ] **Step 5: Confirm the default output is untouched**

Run: `git diff main -- src/snapshots/`
Expected: **empty**.

- [ ] **Step 6: Lint and format**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add src/render.rs
git commit -m "feat(render): derive ValueEnum and serde on Format and TableStyle"
```

---

### Task 2: The config module

**Files:**
- Modify: `Cargo.toml` (add `toml`)
- Create: `src/config.rs`
- Modify: `src/main.rs` (add `mod config;`)

**Interfaces:**
- Consumes: `Format` and `TableStyle`'s serde derives from Task 1.
- Produces:
  - `pub struct Config { format: Option<Format>, table_style: Option<TableStyle>, ext: Option<Vec<String>>, respect_gitignore: Option<bool>, hidden: Option<bool>, exclude: Option<Vec<String>> }` — all fields `pub`, derives `Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize`.
  - `pub enum ConfigKey { Format, TableStyle, Ext, RespectGitignore, Hidden, Exclude }` — derives `Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum`.
  - `pub enum Allowed { OneOf(&'static [&'static str]), List }`, implementing `Display`.
  - `impl ConfigKey { pub fn as_str(self) -> &'static str; pub fn allowed(self) -> Allowed; }`
  - `pub fn config_path() -> Option<PathBuf>`
  - `pub fn load() -> anyhow::Result<Config>`
  - `pub fn load_from(path: &Path) -> anyhow::Result<Config>`
  - `pub fn save_to(path: &Path, config: &Config) -> anyhow::Result<()>`
  - `pub fn set(config: &mut Config, key: ConfigKey, value: &str) -> anyhow::Result<()>`
  - `pub fn unset(config: &mut Config, key: ConfigKey)`
  - `pub fn get(config: &Config, key: ConfigKey) -> Option<String>`

- [ ] **Step 1: Add the `toml` dependency**

In `Cargo.toml`, add to `[dependencies]` in alphabetical position (after `thiserror`):

```toml
toml = "1.1.3"
```

- [ ] **Step 2: Write the failing tests**

Create `src/config.rs` containing **only** this test module for now (the
implementation lands in Step 4):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    // `to_possible_value()` is a `ValueEnum` method; the trait must be in scope.
    use clap::ValueEnum;
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
        assert!(!text.contains("format"), "unset keys must be absent, got:\n{text}");
        assert!(!text.contains("hidden"), "unset keys must be absent, got:\n{text}");
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let td = tempdir().unwrap();
        let path = td.path().join("a").join("b").join("config.toml");
        save_to(&path, &Config::default()).unwrap();
        assert!(path.is_file());
    }

    #[test]
    fn malformed_file_errors_naming_the_path() {
        let td = tempdir().unwrap();
        let path = td.path().join("config.toml");
        std::fs::write(&path, "this is not = = toml").unwrap();
        let err = format!("{:#}", load_from(&path).unwrap_err());
        assert!(err.contains("config.toml"), "must name the file, got: {err}");
    }

    /// A typo'd key must not silently do nothing.
    #[test]
    fn unknown_key_errors_naming_the_path_and_key() {
        let td = tempdir().unwrap();
        let path = td.path().join("config.toml");
        std::fs::write(&path, "tabel_style = \"unicode\"\n").unwrap();
        let err = format!("{:#}", load_from(&path).unwrap_err());
        assert!(err.contains("config.toml"), "must name the file, got: {err}");
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
        assert!(err.contains("unicode"), "must name the allowed values, got: {err}");
        assert_eq!(config, Config::default(), "a rejected set must not mutate");
    }

    #[test]
    fn set_rejects_a_non_boolean() {
        let mut config = Config::default();
        assert!(set(&mut config, ConfigKey::Hidden, "yes").is_err());
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
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test config::`
Expected: FAIL to compile — `cannot find type Config in this scope` (the
module has tests but no implementation yet). Add `mod config;` to `src/main.rs`
alongside the existing `mod` declarations if the module is not found at all.

- [ ] **Step 4: Write the implementation**

Put this **above** the test module in `src/config.rs`:

```rust
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
    pub const ALL: [ConfigKey; 6] = [
        ConfigKey::Format,
        ConfigKey::TableStyle,
        ConfigKey::Ext,
        ConfigKey::RespectGitignore,
        ConfigKey::Hidden,
        ConfigKey::Exclude,
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
        }
    }

    /// The values this key accepts.
    pub fn allowed(self) -> Allowed {
        match self {
            ConfigKey::Format => Allowed::OneOf(&["table", "json", "csv", "tsv", "md"]),
            ConfigKey::TableStyle => Allowed::OneOf(&["ascii", "unicode", "compact", "plain"]),
            ConfigKey::RespectGitignore | ConfigKey::Hidden => Allowed::OneOf(&["true", "false"]),
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
pub fn save_to(path: &Path, config: &Config) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create config directory {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(config).context("failed to serialize the config")?;
    fs::write(path, text).with_context(|| format!("cannot write config file {}", path.display()))
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
        ConfigKey::Exclude => config.exclude = Some(split_list(value)),
    }
    Ok(())
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
```

In `src/main.rs`, add `mod config;` next to the existing module declarations.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test config::`
Expected: PASS, all tests in the module.

- [ ] **Step 6: Lint, format, full test run**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: clean, all tests pass.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/config.rs src/main.rs
git commit -m "feat(config): add the config file schema, IO, and ConfigKey"
```

---

### Task 3: Precedence — `Option<T>` flags, negation flags, and the resolver

This is the widest task: it changes how every valued flag is declared, so the
existing behavior must be preserved exactly for anyone who passes no config
file. The regression guard is the existing test suite plus a new precedence
matrix.

**Files:**
- Modify: `src/cli.rs` (`Option<T>` flags, negation flags, `walk_opts` removal, tests)
- Create: `src/settings.rs`
- Modify: `src/main.rs` (`get_matches` + `from_arg_matches`, config load, `Settings` wiring, `mod settings;`)
- Modify: `src/session.rs` (construct from `Settings`)
- Modify: `src/repl.rs` (call the new `Session` accessors)
- Test: `tests/cli.rs`

**Interfaces:**
- Consumes: `Config`, `ConfigKey` from Task 2.
- Produces:
  - `pub struct Resolved<T> { pub value: T, pub source: Source }` — `Debug, Clone, PartialEq, Eq`.
  - `pub enum Source { Session, Flag, Env, Config, Default }` — `Debug, Clone, Copy, PartialEq, Eq`, implementing `Display` as `session`/`flag`/`env`/`config`/`default`.
  - `pub struct Settings { pub format: Resolved<Format>, pub table_style: Resolved<TableStyle>, pub ext: Resolved<Vec<String>>, pub respect_gitignore: Resolved<bool>, pub hidden: Resolved<bool>, pub exclude: Resolved<Vec<String>> }` — derives `Debug, Clone, PartialEq, Eq`, with `Default` written by hand (it carries the built-in default *values*, which a derive cannot supply).
  - `impl Settings { pub fn resolve(cli: &Cli, config: &Config, matches: &ArgMatches) -> Self; pub fn resolve_walk(walk: &WalkFlags, config: &Config, matches: &ArgMatches) -> Self; pub fn walk_opts(&self) -> WalkOpts; pub fn rows(&self) -> String; }`
  - `Session::new(store: Box<dyn RecordStore>, settings: Settings, fallback: Settings, vault: Option<PathBuf>) -> Session`, with `Session::format()`, `Session::style()`, `Session::settings()`, `Session::set_format`, `Session::set_style`.
  - `Cli::format: Option<Format>`, `Cli::table_style: Option<TableStyle>`, `WalkFlags::ext: Option<Vec<String>>`, `WalkFlags::no_hidden: bool`, `WalkFlags::no_respect_gitignore: bool`.

- [ ] **Step 1: Write the failing tests**

Create `src/settings.rs` with only this test module (implementation in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use crate::config::Config;
    use crate::render::{Format, TableStyle};
    use clap::{CommandFactory, FromArgMatches};

    /// Parses `args` and resolves them against `config`, the way `main` does.
    fn resolve(args: &[&str], config: &Config) -> Settings {
        // `table_style`'s clap `env` reads the real process environment, so
        // neutralize it: a developer with QUERYMATTER_TABLE_STYLE exported
        // must not change these results.
        let mut command = Cli::command().mut_arg("table_style", |a| a.env(None::<&str>));
        let matches = command.try_get_matches_from_mut(args).unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap();
        Settings::resolve(&cli, config, &matches)
    }

    fn config_with(f: impl FnOnce(&mut Config)) -> Config {
        let mut config = Config::default();
        f(&mut config);
        config
    }

    #[test]
    fn defaults_when_nothing_is_set() {
        let s = resolve(&["querymatter"], &Config::default());
        assert_eq!(s.format.value, Format::Table);
        assert_eq!(s.format.source, Source::Default);
        assert_eq!(s.table_style.value, TableStyle::Ascii);
        assert_eq!(s.ext.value, vec!["md".to_string(), "markdown".to_string()]);
        assert!(!s.hidden.value);
        assert!(!s.respect_gitignore.value);
        assert!(s.exclude.value.is_empty());
    }

    /// The defaults the resolver produces with nothing set must be exactly
    /// `Settings::default()` — one source of truth, not two.
    #[test]
    fn resolver_defaults_match_settings_default() {
        let s = resolve(&["querymatter"], &Config::default());
        let d = Settings::default();
        assert_eq!(s.format.value, d.format.value);
        assert_eq!(s.table_style.value, d.table_style.value);
        assert_eq!(s.ext.value, d.ext.value);
        assert_eq!(s.respect_gitignore.value, d.respect_gitignore.value);
        assert_eq!(s.hidden.value, d.hidden.value);
        assert_eq!(s.exclude.value, d.exclude.value);
    }

    #[test]
    fn config_beats_default() {
        let config = config_with(|c| c.table_style = Some(TableStyle::Unicode));
        let s = resolve(&["querymatter"], &config);
        assert_eq!(s.table_style.value, TableStyle::Unicode);
        assert_eq!(s.table_style.source, Source::Config);
    }

    #[test]
    fn flag_beats_config() {
        let config = config_with(|c| c.table_style = Some(TableStyle::Unicode));
        let s = resolve(&["querymatter", "--table-style", "compact"], &config);
        assert_eq!(s.table_style.value, TableStyle::Compact);
        assert_eq!(s.table_style.source, Source::Flag);
    }

    #[test]
    fn format_flag_beats_config() {
        let config = config_with(|c| c.format = Some(Format::Json));
        assert_eq!(
            resolve(&["querymatter"], &config).format.value,
            Format::Json
        );
        let s = resolve(&["querymatter", "--format", "csv"], &config);
        assert_eq!(s.format.value, Format::Csv);
        assert_eq!(s.format.source, Source::Flag);
    }

    #[test]
    fn ext_flag_replaces_the_configured_list() {
        let config = config_with(|c| c.ext = Some(vec!["md".into(), "markdown".into()]));
        let s = resolve(&["querymatter", "--ext", "mdx"], &config);
        assert_eq!(s.ext.value, vec!["mdx".to_string()], "replace, never append");
        assert_eq!(s.ext.source, Source::Flag);
    }

    #[test]
    fn exclude_flag_replaces_the_configured_list() {
        let config = config_with(|c| c.exclude = Some(vec!["**/a/**".into()]));
        let s = resolve(&["querymatter", "--exclude", "**/b/**"], &config);
        assert_eq!(s.exclude.value, vec!["**/b/**".to_string()]);
    }

    /// Without a negation flag there would be no way to turn a configured
    /// `true` back off for one invocation.
    #[test]
    fn no_hidden_overrides_a_configured_true() {
        let config = config_with(|c| c.hidden = Some(true));
        assert!(resolve(&["querymatter"], &config).hidden.value);
        let s = resolve(&["querymatter", "--no-hidden"], &config);
        assert!(!s.hidden.value);
        assert_eq!(s.hidden.source, Source::Flag);
    }

    #[test]
    fn no_respect_gitignore_overrides_a_configured_true() {
        let config = config_with(|c| c.respect_gitignore = Some(true));
        assert!(resolve(&["querymatter"], &config).respect_gitignore.value);
        let s = resolve(&["querymatter", "--no-respect-gitignore"], &config);
        assert!(!s.respect_gitignore.value);
    }

    #[test]
    fn hidden_flag_beats_a_configured_false() {
        let config = config_with(|c| c.hidden = Some(false));
        let s = resolve(&["querymatter", "--hidden"], &config);
        assert!(s.hidden.value);
        assert_eq!(s.hidden.source, Source::Flag);
    }

    #[test]
    fn a_flag_and_its_negation_together_are_rejected() {
        let mut command = Cli::command().mut_arg("table_style", |a| a.env(None::<&str>));
        assert!(
            command
                .try_get_matches_from_mut(["querymatter", "--hidden", "--no-hidden"])
                .is_err()
        );
        let mut command = Cli::command().mut_arg("table_style", |a| a.env(None::<&str>));
        assert!(
            command
                .try_get_matches_from_mut([
                    "querymatter",
                    "--respect-gitignore",
                    "--no-respect-gitignore"
                ])
                .is_err()
        );
    }

    #[test]
    fn walk_opts_carries_the_resolved_scan_settings() {
        let config = config_with(|c| {
            c.ext = Some(vec!["mdx".into()]);
            c.hidden = Some(true);
            c.respect_gitignore = Some(true);
            c.exclude = Some(vec!["**/x/**".into()]);
        });
        let opts = resolve(&["querymatter"], &config).walk_opts();
        assert_eq!(opts.exts, vec!["mdx".to_string()]);
        assert!(opts.hidden);
        assert!(opts.respect_gitignore);
        assert_eq!(opts.excludes, vec!["**/x/**".to_string()]);
        assert!(opts.ignore_files.is_empty(), "filled by the caller");
    }

    #[test]
    fn rows_name_every_key_and_its_source() {
        let config = config_with(|c| c.table_style = Some(TableStyle::Unicode));
        let rows = resolve(&["querymatter"], &config).rows();
        for key in crate::config::ConfigKey::ALL {
            assert!(rows.contains(key.as_str()), "{} missing from:\n{rows}", key.as_str());
        }
        assert!(rows.contains("unicode"), "got:\n{rows}");
        assert!(rows.contains("config"), "got:\n{rows}");
        assert!(rows.contains("default"), "got:\n{rows}");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test settings::`
Expected: FAIL to compile — `cannot find type Settings in this scope`.

- [ ] **Step 3: Restructure the CLI flags**

In `src/cli.rs`, change `WalkFlags`'s `ext`, `respect_gitignore`, and `hidden`
fields and add the two negation flags:

```rust
    /// File extensions (without the leading dot) to include. [default: md,markdown]
    #[arg(long, value_delimiter = ',')]
    pub ext: Option<Vec<String>>,

    /// Honor `.gitignore`/`.ignore` rules while scanning.
    #[arg(long)]
    pub respect_gitignore: bool,

    /// Ignore `.gitignore`/`.ignore` rules, overriding a config `true`.
    #[arg(long, conflicts_with = "respect_gitignore")]
    pub no_respect_gitignore: bool,

    /// Descend into hidden files and directories.
    #[arg(long)]
    pub hidden: bool,

    /// Do not descend into hidden files and directories, overriding a config `true`.
    #[arg(long, conflicts_with = "hidden")]
    pub no_hidden: bool,
```

**Delete `WalkFlags::walk_opts` entirely** — its job moves to
`Settings::walk_opts`, which is the only place that knows the resolved values.
Keep `validate_excludes` and `ignore_files`/`resolve_ignore_files` as they are.

`WalkFlags`'s own doc comment (around `src/cli.rs:28`) opens with an intra-doc
link to the method you just deleted — ``[`walk_opts`](WalkFlags::walk_opts)`` —
which would become a broken link. Rewrite that sentence to name only the
methods that remain, e.g.:

```rust
/// Grouping them here keeps [`validate_excludes`](WalkFlags::validate_excludes)
/// and [`ignore_files`](WalkFlags::ignore_files) — which only ever read these
/// fields — off [`Cli`], so `init` reuses the exact same discovery semantics.
/// Turning the raw flags into a [`WalkOpts`](crate::discover::WalkOpts) is
/// [`Settings::walk_opts`](crate::settings::Settings::walk_opts)'s job, since
/// only the resolver knows which layer won for each field.
```

The two callers at `src/main.rs:55` and `src/main.rs:130` are updated in Step 7.

In `Cli`, drop both `default_value`s:

```rust
    /// Output format for results. [default: table]
    #[arg(long, value_enum)]
    pub format: Option<Format>,

    /// Border style for `--format table`. [default: ascii]
    #[arg(long, value_enum, env = "QUERYMATTER_TABLE_STYLE")]
    pub table_style: Option<TableStyle>,
```

The `[default: …]` markers move into the doc comments because clap no longer
supplies them; `Settings::default()` remains the value's real home, and the
`resolver_defaults_match_settings_default` test keeps the two honest.

- [ ] **Step 4: Write the resolver**

Put this **above** the test module in `src/settings.rs`:

```rust
//! Resolving each setting from the layers that can supply it, and recording
//! which layer won.
//!
//! The precedence is `flag > env > config > default`, applied per key
//! independently. [`Settings::default`] is the single home of the built-in
//! defaults: clap no longer carries `default_value` for the valued flags,
//! because a clap-supplied default is indistinguishable from a user-typed one
//! and would outrank the config file.

use std::collections::BTreeMap;
use std::fmt;

use clap::ArgMatches;
use clap::parser::ValueSource;

use crate::cli::{Cli, WalkFlags};
use crate::config::{Config, ConfigKey};
use crate::discover::WalkOpts;
use crate::render::{Format, TableStyle};

/// Which precedence layer supplied a resolved value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Set during this REPL session by `.style`/`.format`.
    Session,
    /// A command-line flag.
    Flag,
    /// An environment variable.
    Env,
    /// The config file.
    Config,
    /// The built-in default.
    Default,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Source::Session => "session",
            Source::Flag => "flag",
            Source::Env => "env",
            Source::Config => "config",
            Source::Default => "default",
        };
        f.write_str(name)
    }
}

/// A resolved value together with the layer it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved<T> {
    pub value: T,
    pub source: Source,
}

impl<T> Resolved<T> {
    fn new(value: T, source: Source) -> Self {
        Resolved { value, source }
    }
}

/// Every setting resolved to a concrete value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub format: Resolved<Format>,
    pub table_style: Resolved<TableStyle>,
    pub ext: Resolved<Vec<String>>,
    pub respect_gitignore: Resolved<bool>,
    pub hidden: Resolved<bool>,
    pub exclude: Resolved<Vec<String>>,
}

impl Default for Settings {
    /// The built-in defaults — the single source of truth for them.
    fn default() -> Self {
        Settings {
            format: Resolved::new(Format::Table, Source::Default),
            table_style: Resolved::new(TableStyle::Ascii, Source::Default),
            ext: Resolved::new(vec!["md".to_string(), "markdown".to_string()], Source::Default),
            respect_gitignore: Resolved::new(false, Source::Default),
            hidden: Resolved::new(false, Source::Default),
            exclude: Resolved::new(Vec::new(), Source::Default),
        }
    }
}

impl Settings {
    /// Resolves every setting for query mode.
    pub fn resolve(cli: &Cli, config: &Config, matches: &ArgMatches) -> Self {
        let defaults = Settings::default();
        Settings {
            format: resolve_value(
                cli.format,
                config.format,
                defaults.format.value,
                source_of(matches, "format"),
            ),
            table_style: resolve_value(
                cli.table_style,
                config.table_style,
                defaults.table_style.value,
                source_of(matches, "table_style"),
            ),
            ..Settings::resolve_walk(&cli.walk, config, matches)
        }
    }

    /// Resolves the scan settings only — everything `querymatter init` needs.
    /// The rendering settings are left at their defaults, since `init`
    /// renders nothing.
    pub fn resolve_walk(walk: &WalkFlags, config: &Config, matches: &ArgMatches) -> Self {
        let defaults = Settings::default();
        Settings {
            ext: resolve_value(
                walk.ext.clone(),
                config.ext.clone(),
                // `.clone()`, not a move: `..defaults` below needs `defaults`
                // whole, and a partial move out of a non-`Copy` field would
                // make that struct-update illegal.
                defaults.ext.value.clone(),
                source_of(matches, "ext"),
            ),
            respect_gitignore: resolve_bool(
                matches,
                "respect_gitignore",
                "no_respect_gitignore",
                config.respect_gitignore,
                defaults.respect_gitignore.value,
            ),
            hidden: resolve_bool(
                matches,
                "hidden",
                "no_hidden",
                config.hidden,
                defaults.hidden.value,
            ),
            exclude: resolve_value(
                non_empty(walk.exclude.clone()),
                config.exclude.clone(),
                defaults.exclude.value.clone(),
                source_of(matches, "exclude"),
            ),
            ..defaults
        }
    }

    /// The [`WalkOpts`] these settings describe, with an empty `ignore_files`
    /// for the caller to fill from [`WalkFlags::ignore_files`].
    pub fn walk_opts(&self) -> WalkOpts {
        WalkOpts {
            exts: self.ext.value.clone(),
            respect_gitignore: self.respect_gitignore.value,
            hidden: self.hidden.value,
            excludes: self.exclude.value.clone(),
            ignore_files: Vec::new(),
        }
    }

    /// One aligned `key  value  (source)` line per setting, shared by
    /// `querymatter config list` and the REPL's `.settings`.
    pub fn rows(&self) -> String {
        let cells = self.cells();
        let key_width = ConfigKey::ALL
            .iter()
            .map(|key| key.as_str().len())
            .max()
            .unwrap_or(0);
        let value_width = cells
            .values()
            .map(|(value, _)| value.len())
            .max()
            .unwrap_or(0);
        ConfigKey::ALL
            .iter()
            .map(|key| {
                let (value, source) = &cells[key];
                let name = key.as_str();
                format!("{name:key_width$}  {value:value_width$}  ({source})")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Each key's displayed value and source.
    fn cells(&self) -> BTreeMap<ConfigKey, (String, Source)> {
        BTreeMap::from([
            (
                ConfigKey::Format,
                (enum_name(&self.format.value), self.format.source),
            ),
            (
                ConfigKey::TableStyle,
                (enum_name(&self.table_style.value), self.table_style.source),
            ),
            (ConfigKey::Ext, (list(&self.ext.value), self.ext.source)),
            (
                ConfigKey::RespectGitignore,
                (
                    self.respect_gitignore.value.to_string(),
                    self.respect_gitignore.source,
                ),
            ),
            (
                ConfigKey::Hidden,
                (self.hidden.value.to_string(), self.hidden.source),
            ),
            (
                ConfigKey::Exclude,
                (list(&self.exclude.value), self.exclude.source),
            ),
        ])
    }
}

/// Picks the winning layer for one setting.
///
/// `cli` already carries the flag-over-environment decision (clap fills it
/// from either), so `cli_source` says which of the two it actually was.
fn resolve_value<T>(
    cli: Option<T>,
    config: Option<T>,
    default: T,
    cli_source: Option<Source>,
) -> Resolved<T> {
    match (cli, config) {
        (Some(value), _) => Resolved::new(value, cli_source.unwrap_or(Source::Flag)),
        (None, Some(value)) => Resolved::new(value, Source::Config),
        (None, None) => Resolved::new(default, Source::Default),
    }
}

/// Picks the winning layer for a boolean expressed as a flag/negation pair.
fn resolve_bool(
    matches: &ArgMatches,
    on: &str,
    off: &str,
    config: Option<bool>,
    default: bool,
) -> Resolved<bool> {
    if source_of(matches, on) == Some(Source::Flag) {
        Resolved::new(true, Source::Flag)
    } else if source_of(matches, off) == Some(Source::Flag) {
        Resolved::new(false, Source::Flag)
    } else if let Some(value) = config {
        Resolved::new(value, Source::Config)
    } else {
        Resolved::new(default, Source::Default)
    }
}

/// Whether `id` came from the command line or the environment, if it was
/// supplied at all. A clap-supplied default reads as `None`.
fn source_of(matches: &ArgMatches, id: &str) -> Option<Source> {
    match matches.value_source(id) {
        Some(ValueSource::CommandLine) => Some(Source::Flag),
        Some(ValueSource::EnvVariable) => Some(Source::Env),
        _ => None,
    }
}

/// `None` for an empty list, so an unsupplied repeatable flag falls through
/// to the config layer instead of masking it with an empty override.
fn non_empty(list: Vec<String>) -> Option<Vec<String>> {
    (!list.is_empty()).then_some(list)
}

/// A list rendered the way `config set` accepts it, or `(none)` when empty.
fn list(values: &[String]) -> String {
    if values.is_empty() {
        "(none)".to_string()
    } else {
        values.join(",")
    }
}

/// The name a `ValueEnum` uses for `value`.
fn enum_name<T: clap::ValueEnum>(value: &T) -> String {
    value
        .to_possible_value()
        .expect("no Format or TableStyle variant is skipped")
        .get_name()
        .to_string()
}
```

Add `mod settings;` to `src/main.rs`.

- [ ] **Step 5: Run the resolver tests**

Run: `cargo test settings::`
Expected: PASS, all tests in the module.

- [ ] **Step 6: Rebuild `Session` around `Settings`**

In `src/session.rs`, replace the `format`/`style` fields and their accessors:

```rust
pub struct Session {
    store: Box<dyn RecordStore>,
    /// Every setting as resolved for this session; `.style`/`.format` mutate
    /// the rendering ones in place.
    settings: Settings,
    /// The same resolution with the config layer removed, so `.unset` can
    /// revert a setting to whatever would apply without the config file.
    fallback: Settings,
    vault: Option<PathBuf>,
}

impl Session {
    /// Builds a session over `store` with `settings`, keeping `fallback` —
    /// the config-free resolution — for `.unset`.
    pub fn new(
        store: Box<dyn RecordStore>,
        settings: Settings,
        fallback: Settings,
        vault: Option<PathBuf>,
    ) -> Self {
        Session { store, settings, fallback, vault }
    }

    /// The format rendered results are produced in.
    pub fn format(&self) -> Format {
        self.settings.format.value
    }

    /// The border style used when rendering [`Format::Table`].
    pub fn style(&self) -> TableStyle {
        self.settings.table_style.value
    }

    /// Every setting, for `.settings`.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Switches the output format for the rest of this session only.
    pub fn set_format(&mut self, format: Format) {
        self.settings.format = Resolved { value: format, source: Source::Session };
    }

    /// Switches the table border style for the rest of this session only.
    pub fn set_style(&mut self, style: TableStyle) {
        self.settings.table_style = Resolved { value: style, source: Source::Session };
    }
```

Update `render_statement` to use the accessors:

```rust
        let output = statement.terminator.output(self.format());
        Ok(render::render(&table, output, self.style()))
```

Update every `Session::new` call in `src/session.rs`'s own tests to pass
`Settings::default()` twice (settings and fallback). In `src/repl.rs`, change
`session.format` / `session.style` reads to `session.format()` / `session.style()`.

- [ ] **Step 7: Wire `main`**

In `src/main.rs`, add the imports and switch to matches-based parsing:

```rust
use clap::{CommandFactory, FromArgMatches};
```

```rust
fn main() -> anyhow::Result<()> {
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches)?;
    let config = config::load()?;
    match &cli.command {
        Some(Command::Init(args)) => run_init(args, &config, &matches),
        None => run_query(&cli, &config, &matches),
    }
}
```

In `run_init`, replace `args.walk.walk_opts()` with the resolved settings:

```rust
    let settings = Settings::resolve_walk(&args.walk, config, matches);
    let mut opts = settings.walk_opts();
    opts.ignore_files = args.walk.ignore_files()?;
```

In `run_query`, replace `cli.walk.walk_opts()` the same way, and build the
session from the settings:

```rust
    let settings = Settings::resolve(cli, config, matches);
    let mut opts = settings.walk_opts();
    opts.ignore_files = cli.walk.ignore_files()?;
```

```rust
    let fallback = Settings::resolve(cli, &Config::default(), matches);
    let session = Session::new(Box::new(store), settings, fallback, session_vault);
```

- [ ] **Step 8: Run the whole suite**

Run: `cargo test`
Expected: PASS. The pre-existing tests are the regression guard here — if a
test that passed before now fails, the flag restructure changed behavior for a
user with no config file, which is not allowed.

- [ ] **Step 9: Write the failing integration test**

Add to `tests/cli.rs`:

```rust
/// Points HOME and XDG_CONFIG_HOME at `dir` so a test never reads or writes
/// the developer's real config. HOME covers macOS, where `directories` uses
/// ~/Library/Application Support and ignores XDG_CONFIG_HOME.
fn with_config_home(cmd: &mut Command, dir: &std::path::Path) -> &mut Command {
    cmd.env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir)
        .env_remove("QUERYMATTER_TABLE_STYLE")
}

/// Writes a config file into the fake config home `dir`.
fn write_config(dir: &std::path::Path, body: &str) {
    let path = dir.join("querymatter");
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("config.toml"), body).unwrap();
}

#[test]
fn config_file_supplies_the_table_style() {
    let td = tree();
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = \"unicode\"\n");
    let mut cmd = Command::cargo_bin("querymatter").unwrap();
    with_config_home(&mut cmd, home.path())
        .args(["-e", "SELECT status WHERE prd = '010'"])
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("╭"));
}

#[test]
fn flag_overrides_the_config_file() {
    let td = tree();
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = \"unicode\"\n");
    let mut cmd = Command::cargo_bin("querymatter").unwrap();
    with_config_home(&mut cmd, home.path())
        .args([
            "-e",
            "SELECT status WHERE prd = '010'",
            "--table-style",
            "ascii",
        ])
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("+--"))
        .stdout(predicates::str::contains("╭").not());
}

#[test]
fn env_overrides_the_config_file() {
    let td = tree();
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = \"ascii\"\n");
    let mut cmd = Command::cargo_bin("querymatter").unwrap();
    with_config_home(&mut cmd, home.path())
        .env("QUERYMATTER_TABLE_STYLE", "unicode")
        .args(["-e", "SELECT status WHERE prd = '010'"])
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("╭"));
}

#[test]
fn config_file_supplies_the_format() {
    let td = tree();
    let home = TempDir::new().unwrap();
    write_config(home.path(), "format = \"json\"\n");
    let mut cmd = Command::cargo_bin("querymatter").unwrap();
    let out = with_config_home(&mut cmd, home.path())
        .args(["-e", "SELECT status WHERE prd = '010'"])
        .arg(td.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 2);
}

/// A broken config blocks every command, so its message must name the file.
#[test]
fn malformed_config_exits_non_zero_naming_the_path() {
    let td = tree();
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = = broken\n");
    let mut cmd = Command::cargo_bin("querymatter").unwrap();
    with_config_home(&mut cmd, home.path())
        .args(["-e", "SELECT status"])
        .arg(td.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("config.toml"));
}

#[test]
fn unknown_config_key_exits_non_zero_naming_the_key() {
    let td = tree();
    let home = TempDir::new().unwrap();
    write_config(home.path(), "tabel_style = \"unicode\"\n");
    let mut cmd = Command::cargo_bin("querymatter").unwrap();
    with_config_home(&mut cmd, home.path())
        .args(["-e", "SELECT status"])
        .arg(td.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("tabel_style"));
}
```

Add `use predicates::prelude::*;` to the top of `tests/cli.rs` if it is not
already there — `.not()` needs `PredicateBooleanExt` in scope.

- [ ] **Step 10: Run the integration tests**

Run: `cargo test --test cli`
Expected: PASS, all tests.

- [ ] **Step 11: Confirm the default output is untouched**

Run: `git diff main -- src/snapshots/`
Expected: **empty**.

- [ ] **Step 12: Lint, format, full test run**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: clean, all tests pass.

- [ ] **Step 13: Commit**

```bash
git add src/cli.rs src/settings.rs src/main.rs src/session.rs src/repl.rs tests/cli.rs
git commit -m "feat(settings): resolve settings across flag, env, config, and default"
```

---

### Task 4: The `querymatter config` subcommand

**Files:**
- Modify: `src/cli.rs` (`Command::Config`, `ConfigArgs`, `ConfigAction`, tests)
- Modify: `src/main.rs` (dispatch and `run_config`)
- Test: `tests/cli.rs`
- Modify: `README.md` (Configuration section, negation-flag rows)

**Interfaces:**
- Consumes: `config::{Config, ConfigKey, load, save, set, unset, get, config_path}` from Task 2; `Settings::resolve` and `Settings::rows` from Task 3.
- Produces: `Command::Config(ConfigArgs)`, `pub struct ConfigArgs { pub action: ConfigAction }`, `pub enum ConfigAction { List, Get { key: ConfigKey }, Set { key: ConfigKey, value: String }, Unset { key: ConfigKey }, Path }`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/cli.rs`:

```rust
    #[test]
    fn config_list_parses() {
        match parse(&["querymatter", "config", "list"]).command {
            Some(Command::Config(args)) => assert!(matches!(args.action, ConfigAction::List)),
            other => panic!("expected a Config subcommand, got {other:?}"),
        }
    }

    #[test]
    fn config_set_parses_key_and_value() {
        match parse(&["querymatter", "config", "set", "table_style", "unicode"]).command {
            Some(Command::Config(args)) => match args.action {
                ConfigAction::Set { key, value } => {
                    assert_eq!(key, ConfigKey::TableStyle);
                    assert_eq!(value, "unicode");
                }
                other => panic!("expected Set, got {other:?}"),
            },
            other => panic!("expected a Config subcommand, got {other:?}"),
        }
    }

    #[test]
    fn config_get_and_unset_and_path_parse() {
        assert!(matches!(
            parse(&["querymatter", "config", "get", "format"]).command,
            Some(Command::Config(_))
        ));
        assert!(matches!(
            parse(&["querymatter", "config", "unset", "hidden"]).command,
            Some(Command::Config(_))
        ));
        assert!(matches!(
            parse(&["querymatter", "config", "path"]).command,
            Some(Command::Config(_))
        ));
    }

    /// Keys are spelled snake_case, matching the TOML file exactly.
    #[test]
    fn config_key_is_rejected_when_misspelled() {
        assert!(try_parse(&["querymatter", "config", "get", "table-style"]).is_err());
        assert!(try_parse(&["querymatter", "config", "get", "bogus"]).is_err());
    }
```

Add `use crate::config::{ConfigKey};` and `ConfigAction` to that test module's
imports as needed (it imports names individually rather than by glob).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test cli::tests::config_`
Expected: FAIL — `no variant named Config found for enum Command`.

- [ ] **Step 3: Add the subcommand**

In `src/cli.rs`, add to the `Command` enum:

```rust
    /// Show or change the persistent configuration.
    Config(ConfigArgs),
```

and below it:

```rust
/// Arguments for `querymatter config <ACTION>`.
#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// What to do with the configuration.
    #[command(subcommand)]
    pub action: ConfigAction,
}

/// The `querymatter config` actions.
#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// List every setting, its value, and where that value came from.
    List,
    /// Show one setting's value and the values it accepts.
    Get {
        /// The setting to show.
        #[arg(value_enum)]
        key: ConfigKey,
    },
    /// Set one setting in the config file.
    Set {
        /// The setting to change.
        #[arg(value_enum)]
        key: ConfigKey,
        /// The new value; a comma-separated list for `ext` and `exclude`.
        value: String,
    },
    /// Remove one setting from the config file.
    Unset {
        /// The setting to remove.
        #[arg(value_enum)]
        key: ConfigKey,
    },
    /// Print the config file's path, whether or not it exists.
    Path,
}
```

with `use crate::config::ConfigKey;` at the top of `src/cli.rs`.

- [ ] **Step 4: Implement the dispatch**

In `src/main.rs`, add the arm:

```rust
        Some(Command::Config(args)) => run_config(&args.action, &cli, &config, &matches),
```

and the function:

```rust
/// Runs a `querymatter config` action.
///
/// Output discipline: the data (`list` rows, `get`'s value, `path`) goes to
/// stdout so it can be piped; `set`/`unset` confirmations go to stderr,
/// matching `init`'s no-stdout convention.
fn run_config(
    action: &ConfigAction,
    cli: &Cli,
    config: &Config,
    matches: &ArgMatches,
) -> anyhow::Result<()> {
    match action {
        ConfigAction::List => {
            println!("{}", Settings::resolve(cli, config, matches).rows());
        }
        ConfigAction::Get { key } => {
            let settings = Settings::resolve(cli, config, matches);
            println!("{}", settings.value_of(*key));
            println!("values: {}", key.allowed());
        }
        ConfigAction::Set { key, value } => {
            let mut updated = config.clone();
            config::set(&mut updated, *key, value)?;
            let path = config::save(&updated)?;
            eprintln!(
                "querymatter: set {} = {value} in {}",
                key.as_str(),
                path.display()
            );
        }
        ConfigAction::Unset { key } => {
            let mut updated = config.clone();
            let was_present = config::get(config, *key).is_some();
            config::unset(&mut updated, *key);
            let path = config::save(&updated)?;
            if was_present {
                eprintln!("querymatter: removed {} from {}", key.as_str(), path.display());
            } else {
                eprintln!(
                    "querymatter: {} was not set in {}",
                    key.as_str(),
                    path.display()
                );
            }
        }
        ConfigAction::Path => {
            let path = config::config_path()
                .context("cannot determine a config directory for this user")?;
            println!("{}", path.display());
        }
    }
    Ok(())
}
```

This needs one small addition to `src/settings.rs` — the single-key accessor
`config get` prints:

```rust
    /// One setting's value, spelled the way `config set` accepts it.
    pub fn value_of(&self, key: ConfigKey) -> String {
        self.cells()[&key].0.clone()
    }
```

Add `use clap::ArgMatches;`, `use crate::cli::ConfigAction;`, and
`use crate::settings::Settings;` to `src/main.rs` as needed.

- [ ] **Step 5: Run the unit tests**

Run: `cargo test`
Expected: PASS, all tests.

- [ ] **Step 6: Write the failing integration tests**

Add to `tests/cli.rs`:

```rust
#[test]
fn config_set_then_query_honors_it() {
    let td = tree();
    let home = TempDir::new().unwrap();
    let mut cmd = Command::cargo_bin("querymatter").unwrap();
    with_config_home(&mut cmd, home.path())
        .args(["config", "set", "table_style", "unicode"])
        .assert()
        .success();
    let mut cmd = Command::cargo_bin("querymatter").unwrap();
    with_config_home(&mut cmd, home.path())
        .args(["-e", "SELECT status WHERE prd = '010'"])
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("╭"));
}

#[test]
fn config_list_names_the_source_of_each_value() {
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = \"unicode\"\n");
    let mut cmd = Command::cargo_bin("querymatter").unwrap();
    with_config_home(&mut cmd, home.path())
        .args(["config", "list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("table_style"))
        .stdout(predicates::str::contains("unicode"))
        .stdout(predicates::str::contains("(config)"))
        .stdout(predicates::str::contains("(default)"));
}

#[test]
fn config_get_prints_the_value_and_allowed_values() {
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = \"unicode\"\n");
    let mut cmd = Command::cargo_bin("querymatter").unwrap();
    with_config_home(&mut cmd, home.path())
        .args(["config", "get", "table_style"])
        .assert()
        .success()
        .stdout(predicates::str::contains("unicode"))
        .stdout(predicates::str::contains("ascii"))
        .stdout(predicates::str::contains("compact"));
}

/// A rejected value must not touch the file.
#[test]
fn config_set_rejects_a_bad_value_and_leaves_the_file_alone() {
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = \"unicode\"\n");
    let before = fs::read_to_string(home.path().join("querymatter/config.toml")).unwrap();
    let mut cmd = Command::cargo_bin("querymatter").unwrap();
    with_config_home(&mut cmd, home.path())
        .args(["config", "set", "table_style", "fancy"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("fancy"))
        .stderr(predicates::str::contains("unicode"));
    let after = fs::read_to_string(home.path().join("querymatter/config.toml")).unwrap();
    assert_eq!(before, after, "a rejected set must not rewrite the file");
}

#[test]
fn config_unset_removes_the_key() {
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = \"unicode\"\n");
    let mut cmd = Command::cargo_bin("querymatter").unwrap();
    with_config_home(&mut cmd, home.path())
        .args(["config", "unset", "table_style"])
        .assert()
        .success();
    let text = fs::read_to_string(home.path().join("querymatter/config.toml")).unwrap();
    assert!(!text.contains("table_style"), "got:\n{text}");
}

#[test]
fn config_path_prints_a_path_on_stdout() {
    let home = TempDir::new().unwrap();
    let mut cmd = Command::cargo_bin("querymatter").unwrap();
    with_config_home(&mut cmd, home.path())
        .args(["config", "path"])
        .assert()
        .success()
        .stdout(predicates::str::contains("config.toml"));
}
```

- [ ] **Step 7: Run the integration tests**

Run: `cargo test --test cli`
Expected: PASS, all tests.

- [ ] **Step 8: Document it**

In `README.md`, add two rows to the **Flags** table, immediately after
`--respect-gitignore` and `--hidden` respectively:

```markdown
| `--no-respect-gitignore` | Ignore `.gitignore`/`.ignore` rules, overriding a config `respect_gitignore = true`. |
```

```markdown
| `--no-hidden` | Do not descend into hidden files/directories, overriding a config `hidden = true`. |
```

Then add a new top-level section immediately **before** the `## REPL
dot-commands` section:

```markdown
## Configuration

Persistent settings live in a single user-global TOML file. `querymatter config
path` prints its location — on Linux that is
`~/.config/querymatter/config.toml`.

```toml
format            = "table"     # table, json, csv, tsv, md
table_style       = "unicode"   # ascii, unicode, compact, plain
ext               = ["md", "markdown"]
respect_gitignore = true
hidden            = false
exclude           = ["**/templates/**"]
```

Every key is optional; an absent key falls through to the next layer. Values
resolve per key, independently:

```
flag  >  environment  >  config file  >  built-in default
```

So a configured `hidden = true` still scans hidden files when you pass no flag,
and `--no-hidden` turns it back off for one run. `--table-style` additionally
reads `QUERYMATTER_TABLE_STYLE`, which outranks the file but loses to the flag.

| Command | Meaning |
| --- | --- |
| `config list` | Every setting, its resolved value, and which layer supplied it. |
| `config get <KEY>` | One setting's value, then the values it accepts. |
| `config set <KEY> <VALUE>` | Write the setting to the config file. `ext` and `exclude` take a comma-separated list. |
| `config unset <KEY>` | Remove the setting, returning it to the next layer. |
| `config path` | Print the config file's path, whether or not it exists. |

```console
$ querymatter config set table_style unicode
querymatter: set table_style = unicode in ~/.config/querymatter/config.toml

$ querymatter config list
format             table        (default)
table_style        unicode      (config)
ext                md,markdown  (default)
respect_gitignore  false        (default)
hidden             false        (default)
exclude            (none)       (default)
```

A malformed config file, an unknown key, or an invalid value is a hard error
naming the file — a typo must not silently do nothing. The file is read once,
at startup.
```

- [ ] **Step 9: Lint, format, full test run**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: clean, all tests pass.

- [ ] **Step 10: Commit**

```bash
git add src/cli.rs src/main.rs src/settings.rs tests/cli.rs README.md
git commit -m "feat(cli): add the querymatter config subcommand"
```

---

### Task 5: REPL `.settings`, `.set`, and `.unset`

**Files:**
- Modify: `src/repl.rs` (dot-commands, dispatch, help, tests)
- Modify: `src/session.rs` (config-writing helpers)
- Modify: `README.md` (dot-command rows)

**Interfaces:**
- Consumes: `config::{ConfigKey, set, unset, save, load}` from Task 2; `Session::settings()`, `Session::set_format`, `Session::set_style`, and the `fallback` field from Task 3.
- Produces: `DotCommand::Settings`, `DotCommand::Set(ConfigKey, String)`, `DotCommand::Unset(ConfigKey)`, `DotCommand::BadKey(String)`, `DotCommand::MissingArg(&'static str)`; `Session::persist_set`, `Session::persist_unset`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/repl.rs`:

```rust
    #[test]
    fn settings_command_parses() {
        assert_eq!(parse_dot(".settings"), DotCommand::Settings);
    }

    #[test]
    fn set_command_parses_key_and_value() {
        assert_eq!(
            parse_dot(".set table_style unicode"),
            DotCommand::Set(ConfigKey::TableStyle, "unicode".to_string())
        );
    }

    /// A list value may contain commas but no spaces; the rest of the line is
    /// taken verbatim so `exclude` globs survive.
    #[test]
    fn set_takes_the_rest_of_the_line_as_the_value() {
        assert_eq!(
            parse_dot(".set exclude **/a/**,**/b/**"),
            DotCommand::Set(ConfigKey::Exclude, "**/a/**,**/b/**".to_string())
        );
    }

    #[test]
    fn unset_command_parses() {
        assert_eq!(parse_dot(".unset hidden"), DotCommand::Unset(ConfigKey::Hidden));
    }

    #[test]
    fn bad_key_is_bad_key_not_unknown_command() {
        match parse_dot(".set bogus x") {
            DotCommand::BadKey(name) => assert_eq!(name, "bogus"),
            other => panic!("expected BadKey, got {other:?}"),
        }
    }

    #[test]
    fn missing_arguments_are_reported_as_missing() {
        assert_eq!(parse_dot(".set"), DotCommand::MissingArg("set"));
        assert_eq!(
            parse_dot(".set table_style"),
            DotCommand::MissingArg("set")
        );
        assert_eq!(parse_dot(".unset"), DotCommand::MissingArg("unset"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test repl::tests::set_command`
Expected: FAIL — `no variant named Settings found for enum DotCommand`.

- [ ] **Step 3: Add the dot-commands**

In `src/repl.rs`, add to `DotCommand`:

```rust
    /// `.settings` — list every setting, its value, and its source.
    Settings,
    /// `.set <key> <value>` — persist a setting to the config file.
    Set(ConfigKey, String),
    /// `.unset <key>` — remove a setting from the config file.
    Unset(ConfigKey),
    /// `.set`/`.unset` naming a key that isn't configurable, carrying the
    /// offending name so the error can name the key rather than the command.
    BadKey(String),
    /// `.set`/`.unset` with a missing argument, carrying the command name.
    MissingArg(&'static str),
```

Add the `parse_dot` arms after the `"style"` arm:

```rust
        "settings" => DotCommand::Settings,
        "set" => match (words.next(), rest_after_key(rest, 2)) {
            (Some(key), Some(value)) => match parse_key(key) {
                Some(key) => DotCommand::Set(key, value),
                None => DotCommand::BadKey(key.to_string()),
            },
            _ => DotCommand::MissingArg("set"),
        },
        "unset" => match words.next() {
            Some(key) => match parse_key(key) {
                Some(key) => DotCommand::Unset(key),
                None => DotCommand::BadKey(key.to_string()),
            },
            None => DotCommand::MissingArg("unset"),
        },
```

with two helpers next to `parse_dot`:

```rust
/// Parses a config key name, accepting exactly the spellings `ConfigKey`
/// declares — the same ones the TOML file and `querymatter config` use.
fn parse_key(name: &str) -> Option<ConfigKey> {
    ConfigKey::ALL.into_iter().find(|key| key.as_str() == name)
}

/// Everything after the first `skip` whitespace-separated words of `rest`,
/// trimmed — the value of a `.set`, taken verbatim so globs and commas
/// survive. `None` when there are fewer than `skip + 1` words.
fn rest_after_key(rest: &str, skip: usize) -> Option<String> {
    let mut remainder = rest.trim_start();
    for _ in 0..skip {
        let end = remainder.find(char::is_whitespace)?;
        remainder = remainder[end..].trim_start();
    }
    (!remainder.is_empty()).then(|| remainder.trim_end().to_string())
}
```

Note `parse_dot` currently binds `rest` as the line without its leading `.`;
pass that same binding to `rest_after_key`.

- [ ] **Step 4: Add the session-side persistence**

In `src/session.rs`:

```rust
    /// Persists `key = value` to the config file and applies it to this
    /// session when it affects rendering.
    ///
    /// Returns the config file's path, for the caller's confirmation message,
    /// and whether the change takes effect immediately — scan settings do
    /// not, because the store is already loaded.
    pub fn persist_set(&mut self, key: ConfigKey, value: &str) -> anyhow::Result<(PathBuf, bool)> {
        let mut config = config::load()?;
        config::set(&mut config, key, value)?;
        let path = config::save(&config)?;
        let immediate = self.apply(key, &config);
        Ok((path, immediate))
    }

    /// Removes `key` from the config file, reverting this session's value to
    /// whatever applies without it.
    pub fn persist_unset(&mut self, key: ConfigKey) -> anyhow::Result<(PathBuf, bool)> {
        let mut config = config::load()?;
        config::unset(&mut config, key);
        let path = config::save(&config)?;
        let immediate = match key {
            ConfigKey::Format => {
                self.settings.format = self.fallback.format.clone();
                true
            }
            ConfigKey::TableStyle => {
                self.settings.table_style = self.fallback.table_style.clone();
                true
            }
            _ => false,
        };
        Ok((path, immediate))
    }

    /// Applies a just-persisted rendering setting to this session. Returns
    /// whether anything changed now; scan settings take effect next run.
    fn apply(&mut self, key: ConfigKey, config: &Config) -> bool {
        match key {
            ConfigKey::Format => {
                if let Some(format) = config.format {
                    self.settings.format = Resolved { value: format, source: Source::Config };
                }
                true
            }
            ConfigKey::TableStyle => {
                if let Some(style) = config.table_style {
                    self.settings.table_style = Resolved { value: style, source: Source::Config };
                }
                true
            }
            _ => false,
        }
    }
```

- [ ] **Step 5: Dispatch and document the commands**

In `src/repl.rs`'s `dispatch_dot`, add:

```rust
        DotCommand::Settings => println!("{}", session.settings().rows()),
        DotCommand::Set(key, value) => report_persist(session.persist_set(key, &value), key, false),
        DotCommand::Unset(key) => report_persist(session.persist_unset(key), key, true),
        DotCommand::BadKey(name) => {
            eprintln!(
                "querymatter: unknown setting '{name}' (try: {})",
                ConfigKey::ALL
                    .iter()
                    .map(|key| key.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        DotCommand::MissingArg(cmd) => match cmd {
            "set" => eprintln!("querymatter: usage: .set <key> <value>"),
            _ => eprintln!("querymatter: usage: .unset <key>"),
        },
```

and the reporter next to the other helpers:

```rust
/// Reports the outcome of a `.set`/`.unset` on stderr: the file written, and
/// a note when the change only takes effect on the next run.
fn report_persist(outcome: anyhow::Result<(PathBuf, bool)>, key: ConfigKey, removed: bool) {
    match outcome {
        Ok((path, immediate)) => {
            let verb = if removed { "removed" } else { "saved" };
            eprintln!("querymatter: {verb} {} in {}", key.as_str(), path.display());
            if !immediate {
                eprintln!("querymatter: takes effect on the next run (the store is already loaded)");
            }
        }
        Err(err) => eprintln!("querymatter: {err:#}"),
    }
}
```

Add to `print_help`, after the `.style` line:

```rust
    println!("  .settings          list every setting, its value, and where it came from");
    println!("  .set <key> <val>   save a setting to the config file");
    println!("  .unset <key>       remove a setting from the config file");
```

- [ ] **Step 6: Run the tests**

Run: `cargo test`
Expected: PASS, all tests.

- [ ] **Step 7: Document it**

In `README.md`'s **REPL dot-commands** table, add three rows after `.style`:

```markdown
| `.settings` | List every setting, its resolved value, and which layer supplied it. |
| `.set <key> <value>` | Save a setting to the config file. Rendering settings (`format`, `table_style`) also apply immediately; scan settings take effect on the next run. |
| `.unset <key>` | Remove a setting from the config file. |
```

and add this sentence to the paragraph below that table:

```markdown
`.format` and `.style` change the current session only; `.set format` and
`.set table_style` persist to the config file — so you can try a setting, then
keep it.
```

- [ ] **Step 8: Lint, format, full test run**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: clean, all tests pass.

- [ ] **Step 9: Commit**

```bash
git add src/repl.rs src/session.rs README.md
git commit -m "feat(repl): add .settings, .set, and .unset"
```

---

### Task 6: Shell completions

**Files:**
- Modify: `Cargo.toml` (add `clap_complete`)
- Modify: `src/cli.rs` (`Command::Completions`, `CompletionsArgs`, test)
- Modify: `src/main.rs` (dispatch and `run_completions`)
- Test: `tests/cli.rs`
- Modify: `README.md` (Shell completions section)

**Interfaces:**
- Consumes: `Cli::command()` (clap's `CommandFactory`, already used in `main` from Task 3).
- Produces: `Command::Completions(CompletionsArgs)`, `pub struct CompletionsArgs { pub shell: clap_complete::Shell }`.

- [ ] **Step 1: Add the dependency**

In `Cargo.toml`, add to `[dependencies]` after `clap`:

```toml
clap_complete = "4.6.7"
```

- [ ] **Step 2: Write the failing tests**

Add to the `tests` module in `src/cli.rs`:

```rust
    #[test]
    fn completions_parses_each_shell() {
        for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
            match parse(&["querymatter", "completions", shell]).command {
                Some(Command::Completions(_)) => {}
                other => panic!("expected Completions for {shell}, got {other:?}"),
            }
        }
    }

    #[test]
    fn completions_rejects_an_unknown_shell() {
        assert!(try_parse(&["querymatter", "completions", "tcsh"]).is_err());
    }
```

and to `tests/cli.rs`:

```rust
#[test]
fn completions_emit_a_script_per_shell() {
    for shell in ["bash", "zsh", "fish"] {
        let out = Command::cargo_bin("querymatter")
            .unwrap()
            .args(["completions", shell])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let script = String::from_utf8(out).unwrap();
        assert!(
            script.contains("querymatter"),
            "{shell} script must name the binary, got:\n{script}"
        );
        assert!(script.len() > 100, "{shell} script looks empty:\n{script}");
    }
}

/// The completion script must offer the enum values, which is the whole
/// reason Format/TableStyle/ConfigKey became ValueEnums.
#[test]
fn bash_completions_include_enum_values() {
    let out = Command::cargo_bin("querymatter")
        .unwrap()
        .args(["completions", "bash"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let script = String::from_utf8(out).unwrap();
    assert!(script.contains("unicode"), "table styles missing:\n{script}");
    assert!(script.contains("tsv"), "formats missing:\n{script}");
    assert!(script.contains("table_style"), "config keys missing:\n{script}");
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test completions`
Expected: FAIL — `no variant named Completions found for enum Command`.

- [ ] **Step 4: Add the subcommand**

In `src/cli.rs`, add to `Command`:

```rust
    /// Print a shell completion script to stdout.
    Completions(CompletionsArgs),
```

and:

```rust
/// Arguments for `querymatter completions <SHELL>`.
#[derive(Debug, Args)]
pub struct CompletionsArgs {
    /// Shell to generate a completion script for.
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}
```

- [ ] **Step 5: Implement the dispatch**

In `src/main.rs`, add the arm:

```rust
        Some(Command::Completions(args)) => {
            run_completions(args);
            Ok(())
        }
```

and:

```rust
/// Writes a shell completion script for `args.shell` to stdout.
///
/// The script is data, so it goes to stdout for redirection into the shell's
/// completion directory (see the README).
fn run_completions(args: &CompletionsArgs) {
    let mut command = Cli::command();
    let name = command.get_name().to_string();
    clap_complete::generate(args.shell, &mut command, name, &mut io::stdout());
}
```

Note this arm must be matched **before** the config file is loaded, or a
malformed config would block a user from installing completions. Move the
`config::load()` call in `main` so that `Completions` is dispatched first:

```rust
fn main() -> anyhow::Result<()> {
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches)?;
    // Completions must work even with a broken config file — it is how a user
    // installs the completion that helps them type `config set` correctly.
    if let Some(Command::Completions(args)) = &cli.command {
        run_completions(args);
        return Ok(());
    }
    let config = config::load()?;
    match &cli.command {
        Some(Command::Init(args)) => run_init(args, &config, &matches),
        Some(Command::Config(args)) => run_config(&args.action, &cli, &config, &matches),
        Some(Command::Completions(_)) => unreachable!("handled above"),
        None => run_query(&cli, &config, &matches),
    }
}
```

- [ ] **Step 6: Run the tests**

Run: `cargo test`
Expected: PASS, all tests.

- [ ] **Step 7: Verify a real completion script by hand**

Run:

```bash
cargo run -q -- completions bash | head -20
cargo run -q -- completions zsh | grep -c 'table_style\|unicode'
```

Expected: a bash script beginning with `_querymatter()`, and a non-zero count
from the zsh grep.

- [ ] **Step 8: Document it**

In `README.md`, add a section immediately after the **Configuration** section:

```markdown
## Shell completions

`querymatter completions <SHELL>` prints a completion script to stdout for
`bash`, `zsh`, `fish`, `elvish`, or `powershell`. It completes subcommands,
flags, directories, and the allowed values of `--format`, `--table-style`, and
the `config` keys.

```sh
# bash
querymatter completions bash > ~/.local/share/bash-completion/completions/querymatter

# zsh — anywhere on your $fpath
querymatter completions zsh > "${fpath[1]}/_querymatter"

# fish
querymatter completions fish > ~/.config/fish/completions/querymatter.fish
```

Completions work even when the config file is malformed, so you can always
tab-complete your way to `querymatter config path`.
```

- [ ] **Step 9: Lint, format, full test run**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: clean, all tests pass.

- [ ] **Step 10: Confirm the default output is still untouched**

Run: `git diff main -- src/snapshots/`
Expected: **empty**.

- [ ] **Step 11: Commit**

```bash
git add Cargo.toml Cargo.lock src/cli.rs src/main.rs tests/cli.rs README.md
git commit -m "feat(cli): add shell completion generation"
```

---

### Task 7: Final review and branch completion

- [ ] **Step 1: Full verification**

Run: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: clean, all tests pass.

- [ ] **Step 2: Confirm default output is unchanged**

Run: `git diff main -- src/snapshots/querymatter__render__tests__table_snapshot.snap src/snapshots/querymatter__render__tests__md_snapshot.snap`
Expected: **empty**.

- [ ] **Step 3: Confirm the environment cannot poison the suite**

Run:

```bash
QUERYMATTER_TABLE_STYLE=unicode cargo test
QUERYMATTER_TABLE_STYLE=fancy cargo test
```

Expected: both fully green. This is the regression that the previous branch's
final review caught; the new `Option<T>` flags must not reintroduce it.

- [ ] **Step 4: Dispatch the final code reviewer**, then finish the branch per `superpowers:finishing-a-development-branch`.
