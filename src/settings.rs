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
    pub lenient: Resolved<bool>,
}

impl Default for Settings {
    /// The built-in defaults — the single source of truth for them.
    fn default() -> Self {
        Settings {
            format: Resolved::new(Format::Table, Source::Default),
            table_style: Resolved::new(TableStyle::Ascii, Source::Default),
            ext: Resolved::new(
                vec!["md".to_string(), "markdown".to_string()],
                Source::Default,
            ),
            respect_gitignore: Resolved::new(false, Source::Default),
            hidden: Resolved::new(false, Source::Default),
            exclude: Resolved::new(Vec::new(), Source::Default),
            lenient: Resolved::new(false, Source::Default),
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
            lenient: resolve_bool(
                matches,
                "lenient",
                "no_lenient",
                config.lenient,
                defaults.lenient.value,
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

    /// One setting's displayed value: spelled the way `config set` accepts
    /// it, EXCEPT an empty `ext`/`exclude` list, which renders as the
    /// display sentinel `(none)` rather than an empty string. `(none)` is not
    /// itself a value `config set` accepts — round-tripping an empty list
    /// means using `config unset`, not passing `(none)` back in.
    pub fn value_of(&self, key: ConfigKey) -> String {
        self.cells()[&key].0.clone()
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
            (
                ConfigKey::Lenient,
                (self.lenient.value.to_string(), self.lenient.source),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
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
        assert!(!s.lenient.value);
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
        assert_eq!(s.lenient.value, d.lenient.value);
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
        assert_eq!(
            s.ext.value,
            vec!["mdx".to_string()],
            "replace, never append"
        );
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
    fn lenient_flag_beats_a_configured_false() {
        let config = config_with(|c| c.lenient = Some(false));
        let s = resolve(&["querymatter", "--lenient"], &config);
        assert!(s.lenient.value);
        assert_eq!(s.lenient.source, Source::Flag);
    }

    #[test]
    fn lenient_config_beats_default() {
        let config = config_with(|c| c.lenient = Some(true));
        let s = resolve(&["querymatter"], &config);
        assert!(s.lenient.value);
        assert_eq!(s.lenient.source, Source::Config);
    }

    /// Without a negation flag there would be no way to turn a configured
    /// `true` back to strict for one invocation.
    #[test]
    fn no_lenient_overrides_a_configured_true() {
        let config = config_with(|c| c.lenient = Some(true));
        assert!(resolve(&["querymatter"], &config).lenient.value);
        let s = resolve(&["querymatter", "--no-lenient"], &config);
        assert!(!s.lenient.value);
        assert_eq!(s.lenient.source, Source::Flag);
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
        let mut command = Cli::command().mut_arg("table_style", |a| a.env(None::<&str>));
        assert!(
            command
                .try_get_matches_from_mut(["querymatter", "--lenient", "--no-lenient"])
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

    /// `Settings::resolve_walk` — what `init` actually calls, per
    /// `main::run_init` — must pick up a configured `hidden = true` on its
    /// own. Every other precedence test above goes through `Settings::resolve`
    /// (query mode), which just delegates the scan fields to `resolve_walk`
    /// via `..Settings::resolve_walk(...)`; nothing pinned `resolve_walk`
    /// directly against `init`'s own nested `ArgMatches` until now (RECOMMENDED 5).
    #[test]
    fn resolve_walk_picks_up_a_configured_hidden_true() {
        let mut command = Cli::command().mut_arg("table_style", |a| a.env(None::<&str>));
        let matches = command
            .try_get_matches_from_mut(["querymatter", "init"])
            .unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap();
        let Some(Command::Init(init_args)) = &cli.command else {
            panic!("expected an Init subcommand");
        };
        let sub_matches = matches
            .subcommand_matches("init")
            .expect("Command::Init parsed implies the init subcommand matched");
        let config = config_with(|c| c.hidden = Some(true));

        let settings = Settings::resolve_walk(&init_args.walk, &config, sub_matches);
        assert!(
            settings.hidden.value,
            "config hidden = true must reach init's walk"
        );
        assert_eq!(settings.hidden.source, Source::Config);
    }

    #[test]
    fn rows_name_every_key_and_its_source() {
        let config = config_with(|c| c.table_style = Some(TableStyle::Unicode));
        let rows = resolve(&["querymatter"], &config).rows();
        for key in crate::config::ConfigKey::ALL {
            assert!(
                rows.contains(key.as_str()),
                "{} missing from:\n{rows}",
                key.as_str()
            );
        }
        assert!(rows.contains("unicode"), "got:\n{rows}");
        assert!(rows.contains("config"), "got:\n{rows}");
        assert!(rows.contains("default"), "got:\n{rows}");
    }
}
