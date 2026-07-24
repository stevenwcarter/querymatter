# Config file, config commands, and shell completions — design

Date: 2026-07-23
Status: approved

## 1. Problem

`querymatter` has no persistent user preferences. The REPL's `.style` and
`.format` are session-only, and the only durable setting is the
`QUERYMATTER_TABLE_STYLE` environment variable — which covers one option out of
several and requires editing a shell profile. Scan preferences (`--ext`,
`--hidden`, `--respect-gitignore`, `--exclude`) must be retyped on every
invocation.

Separately, the CLI ships no shell completions, and clap cannot currently offer
value completion for `--format`/`--table-style` because both parse via
`FromStr`, so their allowed values exist only in prose.

## 2. Goals

1. A user-global TOML config file holding six settings, with a documented,
   debuggable precedence relative to flags and environment.
2. Commands to **list** the settings (with where each value came from),
   **inspect** one (with its allowed values), and **set**/**unset** it —
   available from both the CLI and the REPL, sharing one implementation.
3. Shell completions for bash/zsh/fish/elvish/powershell, including the allowed
   values of enum-valued flags.
4. README instructions for all of the above.

## 3. Non-goals

Explicitly out of scope; no code should anticipate them:

- Project-local config (`./.querymatter.toml` or similar) and config-file
  discovery by walking up. `.querymatterignore` remains the project-local
  mechanism, for exclusions specifically.
- A `--config <PATH>` flag.
- Dynamic, per-key value completion (`config set table_style <TAB>`). Static
  completion cannot know which key precedes the value, and the clap_complete
  engine that can is unstable.
- Configuring `--fast`, `--no-cache`, `--force-cache`, or `init`'s `--ttl`.
  Cache freshness stays per-invocation, and TTL keeps its single home in the
  vault manifest.
- Any `config edit` / `$EDITOR` integration.

## 4. The config file

### 4.1 Location

`<config_dir>/querymatter/config.toml`, resolved with
`directories::ProjectDirs::from("", "", "querymatter")` — the same crate and
qualifier the REPL already uses for its history file (`repl::history_path`).
On Linux this is `~/.config/querymatter/config.toml`.

`ProjectDirs::from` returning `None` (no home directory) is not an error for
reading — it means "no config"; it *is* an error for writing, reported as
"cannot determine a config directory".

### 4.2 Schema

Six optional keys, in a new `src/config.rs`:

```toml
format            = "table"
table_style       = "unicode"
ext               = ["md", "markdown"]
respect_gitignore = true
hidden            = false
exclude           = ["**/templates/**"]
```

```rust
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
```

Every field is `Option`: absent means "fall through to the built-in default",
which is what makes `unset` expressible and keeps the file minimal. The
`skip_serializing_if` attributes mean writing the file back never materializes
keys the user did not set, so `set` on one key leaves the rest of the file as
sparse as it was.

`Format` and `TableStyle` gain `Serialize`/`Deserialize` derives that go
through their string forms, so the TOML spelling matches the CLI spelling
exactly (`"md"`, `"unicode"`).

### 4.3 Error handling

- **Missing file** — not an error. Yields `Config::default()` (all `None`).
- **Malformed TOML** — hard error naming the file path and the parse error.
- **Unknown key** — hard error (`deny_unknown_fields`), naming the file path
  and the offending key. A typo'd key must not silently do nothing, consistent
  with the precedent set for `QUERYMATTER_TABLE_STYLE`.
- **Wrong-typed or invalid value** (`table_style = "fancy"`, `hidden = 3`) —
  hard error naming the file path, the key, and the allowed values.

Because a malformed config blocks every command, the error message must name
the file path, so the user can edit or delete it. This is the escape hatch;
there is no separate repair mode.

Reading happens exactly **once**, at startup, before the store is built.

## 5. Precedence and the `Settings` type

### 5.1 The rule

```
flag  >  environment  >  config file  >  built-in default
```

Applied per key, independently: a config `hidden = true` with a `--format json`
flag yields hidden-scanning *and* JSON.

### 5.2 Why the CLI struct must change

Today `--format` carries `default_value = "table"` and `--table-style` carries
`default_value = "ascii"`. clap therefore always populates them, so
`cli.format` cannot distinguish "the user typed `--format table`" from "clap
supplied the default" — and a config value could never win. The valued flags
become `Option<T>` with **no** `default_value`:

```rust
    /// Output format for results. [default: table]
    #[arg(long)]
    pub format: Option<Format>,

    /// Border style for `--format table`. [default: ascii]
    #[arg(long, env = "QUERYMATTER_TABLE_STYLE")]
    pub table_style: Option<TableStyle>,
```

`table_style` keeps clap's `env`, so clap continues to deliver the
flag-over-environment half of the precedence natively; the resolver only adds
the config and default layers beneath it.

`WalkFlags::ext` likewise becomes `Option<Vec<String>>` (losing
`default_value = "md,markdown"`), and `exclude` stays `Vec<String>` but is
treated as "unset" when empty.

### 5.3 The resolver

A new `src/settings.rs`:

```rust
/// Every setting resolved to a concrete value, with the source it came from.
pub struct Settings {
    pub format: Format,
    pub table_style: TableStyle,
    pub ext: Vec<String>,
    pub respect_gitignore: bool,
    pub hidden: bool,
    pub exclude: Vec<String>,
}

/// Which precedence layer supplied a resolved value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source { Flag, Env, Config, Default }

impl Settings {
    pub fn resolve(cli: &Cli, config: &Config) -> Self;
    /// Per-key sources, for `config list`.
    pub fn sources(cli: &Cli, config: &Config) -> BTreeMap<ConfigKey, Source>;
}
```

`Settings::default()` is the **single home** for the built-in defaults
(`Format::Table`, `TableStyle::Ascii`, `["md", "markdown"]`, `false`, `false`,
`[]`).

Distinguishing `Flag` from `Env` for `table_style` requires clap's
`ArgMatches::value_source`, which the derive API hides. `main` therefore parses
via `Cli::command().get_matches()` + `Cli::from_arg_matches`, keeping the
`ArgMatches` alongside the `Cli` so `sources()` can consult it. This is
confined to `main` and the `settings` module.

### 5.4 Booleans need negation flags

`--hidden` and `--respect-gitignore` are `bool` flags: today "absent" and
"false" are indistinguishable, so a config `hidden = true` could never be
overridden back to false on the command line. Two new flags:

```rust
    /// Do not descend into hidden files and directories (overrides config).
    #[arg(long, conflicts_with = "hidden")]
    pub no_hidden: bool,

    /// Ignore `.gitignore`/`.ignore` rules while scanning (overrides config).
    #[arg(long, conflicts_with = "respect_gitignore")]
    pub no_respect_gitignore: bool,
```

Each pair resolves to `Some(true)` / `Some(false)` / `None`, and the
conflicting pair is rejected by clap.

### 5.5 List semantics

`ext` and `exclude` **replace**, never append. A flag-supplied list wholly
overrides the configured list — appending would leave no way to *shrink* a
configured list for one invocation. `--exclude` remains repeatable within a
single invocation.

## 6. Commands

### 6.1 CLI: `querymatter config <SUBCOMMAND>`

| Command | Behavior |
|---|---|
| `config list` | All six keys — always all six, including those falling through to defaults — each with its resolved value and its source (`flag`/`env`/`config`/`default`), aligned, to stdout. |
| `config get <KEY>` | The resolved value on the first stdout line, then its allowed values. |
| `config set <KEY> <VALUE>` | Validates, writes the file (creating parent directories), reports the file path on stderr. |
| `config unset <KEY>` | Removes the key; a key that was already absent is reported, not an error. |
| `config path` | The config file path on stdout, whether or not it exists. |

`config` is a peer of `init` in the existing `Command` enum.

**Output discipline:** the *data* (`list` rows, `get`'s value, `path`) goes to
stdout so it can be piped; `set`/`unset` confirmations go to stderr, matching
`init`'s existing "no stdout" convention.

Setting an invalid value is a hard error naming the key and the allowed values,
exiting non-zero, leaving the file untouched.

### 6.2 REPL: `.settings`, `.set`, `.unset`

| Command | Behavior |
|---|---|
| `.settings` | Same rows as `config list`, to stdout. |
| `.set <key> <value>` | Persists to the config file **and** applies to the running session. |
| `.unset <key>` | Removes from the config file **and** reverts the session to the resolved value without it. |

`DotCommand` gains `Settings`, `Set(String, String)`, and `Unset(String)`.
Errors (unknown key, invalid value, unwritable file) go to stderr, matching
`BadFormat`/`BadStyle`.

`.style` and `.format` are unchanged and remain **session-only**, preserving
the "try it, then keep it" flow: `.style unicode` to experiment, `.set
table_style unicode` to make it stick.

Only the two rendering keys (`format`, `table_style`) change a running
session's behavior. Setting a scan key (`ext`, `hidden`, `respect_gitignore`,
`exclude`) persists but prints a note on stderr saying it takes effect on the
next run, since the store is already loaded.

### 6.3 The shared core

Both surfaces call `src/config.rs` for load/store/set/unset and
`src/settings.rs` for resolution and source attribution. Neither surface
contains its own copy of validation, key naming, or precedence logic.

### 6.4 `ConfigKey`

A `ConfigKey` enum names the six keys, deriving `clap::ValueEnum` (for
completion and for `config get/set/unset` argument validation) and exposing:

```rust
impl ConfigKey {
    pub fn as_str(self) -> &'static str;         // "table_style"
    pub fn allowed_values(self) -> Vec<String>;  // for `get` and error messages
}
```

The TOML key names are exactly `ConfigKey::as_str` — snake_case, matching the
struct field names, so the file and the commands never disagree.

## 7. Shell completions

`querymatter completions <SHELL>` prints a completion script to stdout for
`bash`, `zsh`, `fish`, `elvish`, or `powershell`, via `clap_complete::generate`
against `Cli::command()`. Adds one dependency: `clap_complete`.

To make values completable, `Format`, `TableStyle`, and `ConfigKey` derive
`clap::ValueEnum`. This also improves `--help`, which currently lists valid
values only in prose.

`Format`'s and `TableStyle`'s `FromStr` impls are **retained** — the REPL's
`.format`/`.style`/`.set` parse free-text words, not clap arguments. `ValueEnum`
and `FromStr` must accept the same spellings; a test asserts this for every
variant, including `Format`'s `markdown` alias (which `FromStr` accepts and
`ValueEnum` will not offer as a completion — acceptable, and pinned by the test
so the asymmetry is deliberate rather than accidental).

## 8. Documentation

`README.md` gains:

- A **Configuration** section: file location, the six keys with an example
  file, the precedence rule, and the `config` subcommands.
- A **Shell completions** section with a per-shell install line (bash, zsh,
  fish).
- New rows in the Flags table for `--no-hidden` and `--no-respect-gitignore`.
- New rows in the REPL dot-commands table for `.settings`, `.set`, `.unset`.
- A note on the `--format`/`--table-style`/`--ext` help text no longer carrying
  clap's `[default: …]` marker, since the defaults now live in prose.

## 9. Invariants this feature depends on

- **`.querymatterignore` remains the project-local exclusion mechanism.** The
  config's `exclude` key is user-global and does not replace it; both apply.
- **The config file is read exactly once, at startup.** A `config set` run in
  another shell cannot change a running session mid-query. The REPL's `.set`
  mutates the in-memory `Settings` directly, not by re-reading the file.
- **`Settings::default()` is the single source of truth for built-in
  defaults.** Removing clap's `default_value` means `--help` text and the
  resolver could drift; a test asserts the defaults named in the help output
  match `Settings::default()`.
- **`ValueEnum` and `FromStr` accept the same spellings** for `Format` and
  `TableStyle`, so a value that completes is a value that parses in the REPL.

## 10. Testing

`src/config.rs`:
- Round-trip: write a config, read it back, compare.
- Missing file yields `Config::default()` without error.
- Malformed TOML errors, and the message names the path.
- Unknown key errors, and the message names the path and the key.
- Invalid enum value errors, and the message names the allowed values.
- `set` then `unset` returns the file to its prior content.
- `set` creates missing parent directories.

`src/settings.rs`:
- A precedence matrix: for each of the six keys, assert flag beats env beats
  config beats default, using only the layers that key supports.
- `--no-hidden` overrides a config `hidden = true`; `--hidden` and
  `--no-hidden` together are rejected by clap. Same for the gitignore pair.
- `ext`/`exclude` from a flag replace rather than append to the configured list.
- `sources()` reports the correct `Source` for each layer.
- Help text defaults match `Settings::default()`.

`src/cli.rs` / `src/repl.rs`:
- `config` subcommand parsing for all five subcommands.
- `parse_dot` for `.settings`, `.set k v`, `.unset k`, plus the bad-key and
  missing-argument forms.
- `ValueEnum`/`FromStr` agreement for every `Format` and `TableStyle` variant.

`tests/cli.rs` (all with `XDG_CONFIG_HOME` pointed at a `TempDir`, so the
developer's real config is never read or written):
- `config set table_style unicode` then a query emits box-drawing borders.
- A flag overrides the config value.
- `QUERYMATTER_TABLE_STYLE` overrides the config value, and a flag overrides
  both.
- `config list` names the source for a config-supplied and a default value.
- `config get table_style` prints the value and the allowed values.
- `config set table_style fancy` exits non-zero, names the allowed values, and
  leaves the file unchanged.
- A malformed config file makes a query exit non-zero with the path in stderr.
- `completions bash|zsh|fish` each exit zero and emit a non-empty script
  mentioning `querymatter`.

**Regression guard, carried forward:** the committed snapshots
`querymatter__render__tests__table_snapshot.snap` and `..._md_snapshot.snap`
must stay byte-unchanged — this feature must not alter default output.

## 11. Files touched

| file | change |
|---|---|
| `Cargo.toml` | add `toml`, `clap_complete` |
| `src/config.rs` | **new** — schema, load/save, set/unset, `ConfigKey` |
| `src/settings.rs` | **new** — `Settings`, `Source`, resolution |
| `src/cli.rs` | `Option<T>` flags, negation flags, `config`/`completions` subcommands, `ValueEnum` |
| `src/render.rs` | `ValueEnum` + serde on `Format` and `TableStyle` |
| `src/main.rs` | `get_matches` + `from_arg_matches`, config load, `Settings` wiring, subcommand dispatch |
| `src/repl.rs` | `.settings`, `.set`, `.unset` |
| `src/session.rs` | construct from `Settings` |
| `README.md` | Configuration and Shell completions sections; flag and dot-command rows |
| `tests/cli.rs` | integration tests under a temp `XDG_CONFIG_HOME` |
