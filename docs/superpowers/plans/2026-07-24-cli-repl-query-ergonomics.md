# CLI / REPL / query ergonomics bundle — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the 11-item whats-next bundle (W32–W46): config-backed
`timer`/`header`/`quiet` toggles, terminal-width tables, a `.output` pipe target,
`.query save`, `CASE WHEN`, single-pass aggregates, bounded top-k, `cache
status`, and multi-statement failure attribution.

**Architecture:** Extend querymatter's existing seams in place — the
`Config`/`ConfigKey`/`Settings` precedence system (flag>env>config>default), the
`render` layer, the REPL dot-command dispatch, and the `query::{ast,parse,exec}`
pipeline. No new abstractions beyond each item's needs; no cache schema bump.

**Tech Stack:** Rust 2024, clap (derive), comfy-table, csv, sqlparser, rustyline,
serde/toml, bincode; tests via `cargo test`, insta snapshots, assert_cmd +
predicates + tempfile for CLI integration.

## Global Constraints

- Edition 2024; keep `cargo fmt` and `cargo clippy` clean (this repo has no
  pre-commit hook — run them yourself before each commit).
- Binary-only crate: run the full suite with `cargo test` (NOT `cargo test
  --lib`).
- **stdout carries data; stderr carries diagnostics.** Never move result output
  to stderr or diagnostics to stdout.
- Non-TTY / piped output must stay byte-identical unless an item explicitly
  changes it (the insta snapshots and `tests/cli.rs` pin this).
- Config keys are spelled snake_case identically on the CLI and in `config.toml`.
- Every "declined because an invariant makes it safe" test is instead written at
  the seam it crosses (project spec-discipline rule).

---

### Task 1: Config keys + settings + CLI flags for `timer`/`header`/`quiet`

Adds the three boolean settings to the config/settings machinery so they resolve
through flag>env>config>default and are settable via `config set`/`.set`. No
behavior consumes them yet — later tasks do.

**Files:**
- Modify: `src/config.rs` (Config struct, ConfigKey enum + ALL + as_str + allowed + set/unset/get)
- Modify: `src/settings.rs` (Settings struct, Default, resolve, cells)
- Modify: `src/cli.rs` (Cli: `header`/`no_header`/`quiet`/`no_quiet` flags)
- Test: inline `#[cfg(test)]` in config.rs and settings.rs

**Interfaces:**
- Produces: `Config { …, timer: Option<bool>, header: Option<bool>, quiet: Option<bool> }`;
  `ConfigKey::{Timer, Header, Quiet}`;
  `Settings { …, timer: Resolved<bool>, header: Resolved<bool>, quiet: Resolved<bool> }`
  with defaults `timer=false`, `header=true`, `quiet=false`;
  `Cli { …, header: bool, no_header: bool, quiet: bool, no_quiet: bool }`.

- [ ] **Step 1: Write failing config tests**

Add to `src/config.rs` tests:

```rust
#[test]
fn timer_header_quiet_round_trip() {
    let mut config = Config::default();
    set(&mut config, ConfigKey::Timer, "true").unwrap();
    set(&mut config, ConfigKey::Header, "false").unwrap();
    set(&mut config, ConfigKey::Quiet, "true").unwrap();
    assert_eq!(get(&config, ConfigKey::Timer).as_deref(), Some("true"));
    assert_eq!(get(&config, ConfigKey::Header).as_deref(), Some("false"));
    assert_eq!(get(&config, ConfigKey::Quiet).as_deref(), Some("true"));
    assert_eq!(ConfigKey::Timer.allowed().to_string(), "true, false");
}
```

- [ ] **Step 2: Run and confirm failure**

Run: `cargo test --quiet config::tests::timer_header_quiet_round_trip`
Expected: FAIL — no `ConfigKey::Timer` variant.

- [ ] **Step 3: Implement config additions**

In `src/config.rs`, mirror the `lenient` key exactly:
- Add to `Config`: `#[serde(skip_serializing_if = "Option::is_none")] pub timer: Option<bool>,` and likewise `header`, `quiet`.
- Add `ConfigKey::Timer/Header/Quiet` variants with `#[value(name = "timer")]` etc.
- Extend `ConfigKey::ALL` (now length 10) to include the three.
- `as_str`: `Timer => "timer"`, `Header => "header"`, `Quiet => "quiet"`.
- `allowed`: add `Timer | Header | Quiet` to the `RespectGitignore | Hidden | Lenient => Allowed::OneOf(&["true","false"])` arm.
- `set`: `Timer => config.timer = Some(parse_bool(key, value)?)`, same for header/quiet.
- `unset`: set each to `None`.
- `get`: `Timer => config.timer.map(|b| b.to_string())`, same for header/quiet.

- [ ] **Step 4: Write failing settings tests**

Add to `src/settings.rs` tests:

```rust
#[test]
fn header_defaults_true_config_and_flags_resolve() {
    // default
    assert!(resolve(&["querymatter"], &Config::default()).header.value);
    // config false
    let cfg = config_with(|c| c.header = Some(false));
    assert!(!resolve(&["querymatter"], &cfg).header.value);
    // --header overrides config false
    let s = resolve(&["querymatter", "--header"], &cfg);
    assert!(s.header.value);
    assert_eq!(s.header.source, Source::Flag);
    // --no-header overrides config true
    let cfg_t = config_with(|c| c.header = Some(true));
    assert!(!resolve(&["querymatter", "--no-header"], &cfg_t).header.value);
}

#[test]
fn quiet_defaults_false_and_flag_beats_config() {
    assert!(!resolve(&["querymatter"], &Config::default()).quiet.value);
    let cfg = config_with(|c| c.quiet = Some(false));
    assert!(resolve(&["querymatter", "--quiet"], &cfg).quiet.value);
    assert!(resolve(&["querymatter", "-q"], &cfg).quiet.value);
    let cfg_t = config_with(|c| c.quiet = Some(true));
    assert!(!resolve(&["querymatter", "--no-quiet"], &cfg_t).quiet.value);
}

#[test]
fn timer_config_beats_default_no_flag() {
    let cfg = config_with(|c| c.timer = Some(true));
    let s = resolve(&["querymatter"], &cfg);
    assert!(s.timer.value);
    assert_eq!(s.timer.source, Source::Config);
}
```

- [ ] **Step 5: Run and confirm failure**

Run: `cargo test --quiet settings::tests::header_defaults_true_config_and_flags_resolve`
Expected: FAIL — no `header` field on `Settings`.

- [ ] **Step 6: Implement settings + CLI additions**

In `src/cli.rs` `Cli`, after `lenient`/`no_lenient`, add:

```rust
/// Suppress the header row in table/csv/tsv/md output.
#[arg(long)]
pub no_header: bool,
/// Force the header row on, overriding a config `header = false`.
#[arg(long, conflicts_with = "no_header")]
pub header: bool,
/// Suppress non-error stderr chatter (skipped-file warnings, scan summaries).
#[arg(long, short = 'q')]
pub quiet: bool,
/// Force chatter on, overriding a config `quiet = true`.
#[arg(long, conflicts_with = "quiet")]
pub no_quiet: bool,
```

In `src/settings.rs`:
- Add `pub timer/header/quiet: Resolved<bool>` to `Settings`.
- In `Default`: `timer: Resolved::new(false, Source::Default)`, `header: Resolved::new(true, Source::Default)`, `quiet: Resolved::new(false, Source::Default)`.
- In `resolve` (the query-mode fn), add:
  ```rust
  header: resolve_bool(matches, "header", "no_header", config.header, defaults.header.value),
  quiet: resolve_bool(matches, "quiet", "no_quiet", config.quiet, defaults.quiet.value),
  timer: match config.timer { Some(v) => Resolved::new(v, Source::Config), None => Resolved::new(defaults.timer.value, Source::Default) },
  ```
  (timer has no flag layer.) Keep `..Settings::resolve_walk(&cli.walk, config, matches)` last; ensure `resolve_walk`'s `..defaults` still fills timer/header/quiet with defaults for the init path.
- In `cells()`, add three `BTreeMap` entries so `config list`/`.settings` show them:
  ```rust
  (ConfigKey::Timer, (self.timer.value.to_string(), self.timer.source)),
  (ConfigKey::Header, (self.header.value.to_string(), self.header.source)),
  (ConfigKey::Quiet, (self.quiet.value.to_string(), self.quiet.source)),
  ```

- [ ] **Step 7: Run all tests**

Run: `cargo test --quiet && cargo clippy --quiet --all-targets && cargo fmt --check`
Expected: PASS. (The `all_agrees_with_value_variants` and `rows_name_every_key_and_its_source` tests now cover 10 keys.)

- [ ] **Step 8: Commit**

```bash
git add src/config.rs src/settings.rs src/cli.rs
git commit -m "feat(config): add timer/header/quiet config keys + flags (W32/W34/W35)"
```

---

### Task 2: Header suppression in `render` + session plumbing

Threads a `header: bool` through `render::render` and the session so `header =
false` drops the header row for table/md/csv/tsv (JSON and vertical unchanged).

**Files:**
- Modify: `src/render.rs` (`render`, `render_table`, `render_markdown`, `new_table`, `render_delimited`, `write_delimited`)
- Modify: `src/session.rs` (`Session::header`, `set_header`, pass into `render`)
- Test: inline in render.rs

**Interfaces:**
- Consumes: `Settings.header` (Task 1).
- Produces: `render::render(table, output, style, header: bool) -> String`;
  `Session::header() -> bool`, `Session::set_header(bool)`.

- [ ] **Step 1: Write failing render tests**

Add to `src/render.rs` tests:

```rust
#[test]
fn csv_no_header_omits_header_row() {
    let s = render(&table(), Output::Format(Format::Csv), TableStyle::Ascii, false);
    assert_eq!(s.lines().next().unwrap(), "synced,2");
    assert!(!s.contains("status,Count"));
}

#[test]
fn tsv_no_header_omits_header_row() {
    let s = render(&table(), Output::Format(Format::Tsv), TableStyle::Ascii, false);
    assert_eq!(s.lines().next().unwrap(), "synced\t2");
}

#[test]
fn table_no_header_omits_header_row() {
    let s = render(&table(), Output::Format(Format::Table), TableStyle::Ascii, false);
    assert!(!s.contains("status"), "header must be gone, got:\n{s}");
    assert!(s.contains("synced"));
}

#[test]
fn json_and_vertical_ignore_header_flag() {
    let j_on = render(&table(), Output::Format(Format::Json), TableStyle::Ascii, true);
    let j_off = render(&table(), Output::Format(Format::Json), TableStyle::Ascii, false);
    assert_eq!(j_on, j_off, "JSON is keyed by header; the flag must not change it");
    let v_on = render(&table(), Output::Vertical, TableStyle::Ascii, true);
    let v_off = render(&table(), Output::Vertical, TableStyle::Ascii, false);
    assert_eq!(v_on, v_off, "vertical labels are not a header row");
}
```

Update the existing render.rs tests' `render(...)` calls to pass `true` as the new 4th arg (with-header, today's behavior). This keeps every existing assertion (including the insta snapshots) unchanged.

- [ ] **Step 2: Run and confirm failure**

Run: `cargo test --quiet render::tests::csv_no_header_omits_header_row`
Expected: FAIL — `render` takes 3 args.

- [ ] **Step 3: Implement header threading**

In `src/render.rs`:
- `pub fn render(table, output, style, header: bool) -> String` — pass `header` to `render_table`, `render_markdown`, `render_delimited`; `render_json`/`render_vertical` ignore it.
- `fn new_table(table: &ResultTable, header: bool) -> Table`: only `ct.set_header(&table.headers)` when `header`.
- `render_table`/`render_markdown` take `header` and pass to `new_table`.
- `render_delimited(table, delimiter, header)` → `write_delimited(table, delimiter, header)`; in `write_delimited`, only `writer.write_record(&table.headers)?` when `header`.

- [ ] **Step 4: Session plumbing**

In `src/session.rs`, mirror `format()`/`set_style()`:
```rust
pub fn header(&self) -> bool { self.settings.header.value }
pub fn set_header(&mut self, on: bool) {
    self.settings.header = Resolved { value: on, source: Source::Session };
}
```
In `render_statement_counted`, change the render call to:
`let rendered = render::render(&table, output, self.style(), self.header());`

- [ ] **Step 5: Run tests**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets`
Expected: PASS (snapshots unchanged — existing calls pass `true`).

- [ ] **Step 6: Commit**

```bash
git add src/render.rs src/session.rs
git commit -m "feat(render): --no-header/header suppresses the header row (W32)"
```

---

### Task 3: `.header [on|off]` REPL session toggle

**Files:**
- Modify: `src/repl.rs` (`DOT_COMMAND_NAMES`, `DotCommand`, `parse_dot`, `dispatch_dot`, help text)
- Test: inline in repl.rs

**Interfaces:**
- Consumes: `Session::set_header`, `Session::header` (Task 2).
- Produces: `DotCommand::Header(Option<bool>)`.

- [ ] **Step 1: Write failing parse test**

```rust
#[test]
fn parses_header_on_off_and_report() {
    assert_eq!(parse_dot(".header on"), DotCommand::Header(Some(true)));
    assert_eq!(parse_dot(".header off"), DotCommand::Header(Some(false)));
    assert_eq!(parse_dot(".header"), DotCommand::Header(None));
}
```

- [ ] **Step 2: Run, confirm failure**

Run: `cargo test --quiet repl::tests::parses_header_on_off_and_report`
Expected: FAIL — no `DotCommand::Header`.

- [ ] **Step 3: Implement**

- Add `".header"` to `DOT_COMMAND_NAMES`.
- Add `Header(Option<bool>)` to `DotCommand` (doc: on/off/report).
- Add a shared on/off parser near `parse_dot`:
  ```rust
  fn parse_on_off(arg: Option<&str>) -> Option<Option<bool>> {
      match arg {
          None => Some(None),
          Some(a) if a.eq_ignore_ascii_case("on") => Some(Some(true)),
          Some(a) if a.eq_ignore_ascii_case("off") => Some(Some(false)),
          Some(_) => None, // caller maps to an error
      }
  }
  ```
  In `parse_dot`, add arm `"header" => match parse_on_off(words.next()) { Some(v) => DotCommand::Header(v), None => DotCommand::Unknown(line.to_string()) }`.
- In `dispatch_dot`:
  ```rust
  DotCommand::Header(Some(on)) => session.set_header(on),
  DotCommand::Header(None) => println!("header: {}", if session.header() { "on" } else { "off" }),
  ```
- Add a `.help` line: `"  .header [on|off]   show, or set, whether results include a header row"`.

- [ ] **Step 4: Write a dispatch test**

```rust
#[test]
fn dispatch_header_off_sets_session() {
    let mut session = /* build a test Session as other repl tests do */;
    let mut sink = OutputSink::Stdout;
    dispatch_dot(DotCommand::Header(Some(false)), &mut session, &mut sink);
    assert!(!session.header());
}
```
(Reuse whatever Session builder the existing repl/session tests use; if none is exposed, assert via `parse_dot` alone and cover dispatch through the session unit test in session.rs.)

- [ ] **Step 5: Run tests**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/repl.rs
git commit -m "feat(repl): .header [on|off] session toggle (W32)"
```

---

### Task 4: `.timer [on|off]` + elapsed time on the row-count line

**Files:**
- Modify: `src/session.rs` (`timer()`, `set_timer()`)
- Modify: `src/repl.rs` (`DotCommand::Timer`, dispatch, help, `row_count_line` gains elapsed, `run_statement` times the query)
- Test: inline in repl.rs

**Interfaces:**
- Consumes: `Settings.timer` (Task 1).
- Produces: `Session::timer() -> bool`, `Session::set_timer(bool)`;
  `DotCommand::Timer(Option<bool>)`; `row_count_line(n: usize, elapsed: Option<Duration>) -> String`.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn row_count_line_appends_elapsed_when_timed() {
    use std::time::Duration;
    assert_eq!(row_count_line(3, None), "-- 3 rows");
    assert_eq!(row_count_line(1, None), "-- 1 row");
    assert_eq!(row_count_line(3, Some(Duration::from_millis(12))), "-- 3 rows (0.012s)");
}

#[test]
fn parses_timer_on_off() {
    assert_eq!(parse_dot(".timer on"), DotCommand::Timer(Some(true)));
    assert_eq!(parse_dot(".timer"), DotCommand::Timer(None));
}
```

- [ ] **Step 2: Run, confirm failure**

Run: `cargo test --quiet repl::tests::row_count_line_appends_elapsed_when_timed`
Expected: FAIL — `row_count_line` takes 1 arg.

- [ ] **Step 3: Implement session + line**

In `src/session.rs`, mirror `set_header`:
```rust
pub fn timer(&self) -> bool { self.settings.timer.value }
pub fn set_timer(&mut self, on: bool) {
    self.settings.timer = Resolved { value: on, source: Source::Session };
}
```
In `src/repl.rs`, change `row_count_line`:
```rust
fn row_count_line(n: usize, elapsed: Option<std::time::Duration>) -> String {
    let plural = if n == 1 { "" } else { "s" };
    match elapsed {
        Some(d) => format!("-- {n} row{plural} ({:.3}s)", d.as_secs_f64()),
        None => format!("-- {n} row{plural}"),
    }
}
```

- [ ] **Step 4: Time the run + dot-command**

In `run_statement`, wrap the render+count:
```rust
let start = std::time::Instant::now();
match session.render_statement_counted(statement) {
    Ok((rendered, count)) => {
        if let Err(err) = sink.write_block(&rendered) {
            eprintln!("querymatter: failed to write results: {err}");
        }
        let elapsed = session.timer().then(|| start.elapsed());
        eprintln!("{}", row_count_line(count, elapsed));
    }
    Err(err) => eprintln!("querymatter: {err:#}"),
}
```
Note: `run_statement` takes `&Session` today. `session.timer()` is `&self`, so no signature change. Add `".timer"` to `DOT_COMMAND_NAMES`, `Timer(Option<bool>)` to `DotCommand`, the `"timer" =>` parse arm (reuse `parse_on_off`), the dispatch arms (`Some(on) => session.set_timer(on)`, `None => println!("timer: {}", …)`), and a `.help` line.

- [ ] **Step 5: Run tests**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/session.rs src/repl.rs
git commit -m "feat(repl): .timer + config timer key show elapsed query time (W35)"
```

---

### Task 5: `--quiet` suppresses scan/refresh chatter (query mode)

**Files:**
- Modify: `src/main.rs` (`build_session` warning loop)
- Test: `tests/cli.rs` (integration)

**Interfaces:**
- Consumes: `Settings.quiet` (Task 1). `settings` is already in scope in `build_session`.

- [ ] **Step 1: Write failing integration test**

Add to `tests/cli.rs` (follow the file's existing `assert_cmd` helpers/fixtures). Build a temp dir with one valid frontmatter file and one file that triggers a skipped/unparsable warning, then:

```rust
#[test]
fn quiet_suppresses_skipped_file_warnings_but_not_errors() {
    let dir = /* tempdir with a good .md and a warning-triggering file */;
    // Without --quiet: a warning appears on stderr.
    Command::cargo_bin("querymatter").unwrap()
        .arg("-e").arg("SELECT status").arg(dir.path())
        .assert().stderr(predicates::str::contains("querymatter:"));
    // With --quiet: stderr carries no "querymatter: <warning>" chatter for the
    // successful query (the "-- N rows" line is REPL-only, absent in -e mode).
    Command::cargo_bin("querymatter").unwrap()
        .arg("--quiet").arg("-e").arg("SELECT status").arg(dir.path())
        .assert().success().stderr(predicates::str::is_empty());
    // --quiet must NOT swallow a real query error.
    Command::cargo_bin("querymatter").unwrap()
        .arg("--quiet").arg("-e").arg("SELECT FROM WHERE bogus(((").arg(dir.path())
        .assert().failure().stderr(predicates::str::is_empty().not());
}
```
(Adjust the warning-trigger to whatever the discovery/store path warns on — inspect `LoadReport.warnings` producers; a non-UTF-8 or unparsable-frontmatter file is the usual trigger. If a clean warning trigger is hard to construct, assert the positive+error halves and drop the middle.)

- [ ] **Step 2: Run, confirm failure**

Run: `cargo test --quiet --test cli quiet_suppresses`
Expected: FAIL — `--quiet` unknown or warnings still printed.

- [ ] **Step 3: Implement**

In `src/main.rs` `build_session`, gate the warning loop (~line 680):
```rust
if !settings.quiet.value {
    for warning in &report.warnings {
        eprintln!("querymatter: {warning}");
    }
}
```
`--quiet` already parses (Task 1 added the flag). Errors propagate via `anyhow` and are printed by `main` unconditionally, so they are unaffected.

- [ ] **Step 4: Run tests**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs tests/cli.rs
git commit -m "feat(cli): --quiet suppresses scan-warning chatter, not errors (W34)"
```

---

### Task 6: Terminal-width-aware table (`--format table` only)

**Files:**
- Modify: `src/render.rs` (`render_table` + a testable width-decision helper)
- Test: inline in render.rs

**Interfaces:**
- Consumes: `render_table(table, style, header)` (Task 2).

- [ ] **Step 1: Write failing helper test**

```rust
#[test]
fn table_uses_dynamic_arrangement_only_on_a_tty() {
    // non-TTY: no dynamic width (byte-identical to today).
    assert!(!want_dynamic_width(false));
    // TTY: dynamic width on.
    assert!(want_dynamic_width(true));
}
```

- [ ] **Step 2: Run, confirm failure**

Run: `cargo test --quiet render::tests::table_uses_dynamic_arrangement_only_on_a_tty`
Expected: FAIL — `want_dynamic_width` undefined.

- [ ] **Step 3: Implement**

In `src/render.rs`:
```rust
/// Table width-fitting is enabled only when stdout is a real terminal, so
/// piped/redirected output stays byte-identical (the interchange-format rule).
fn want_dynamic_width(is_tty: bool) -> bool { is_tty }
```
In `render_table`, before applying the style preset:
```rust
use std::io::IsTerminal;
if want_dynamic_width(std::io::stdout().is_terminal()) {
    ct.set_content_arrangement(comfy_table::ContentArrangement::Dynamic);
}
```
comfy-table auto-detects the terminal width for `Dynamic`. Do NOT touch
`render_markdown`/`new_table` — Markdown stays reproducible. Non-TTY skips the
call entirely, so `ContentArrangement` stays the default (Disabled) and the
existing insta snapshots (run non-TTY) are unchanged.

- [ ] **Step 4: Guard the reproducibility invariant**

Extend the existing `non_table_formats_ignore_style` spirit with a width guard —
Markdown output must not depend on terminal width. Add:
```rust
#[test]
fn markdown_render_is_terminal_independent() {
    // render_markdown never consults the terminal, so its output is fixed.
    let a = render(&table(), Output::Format(Format::Md), TableStyle::Ascii, true);
    let b = render(&table(), Output::Format(Format::Md), TableStyle::Ascii, true);
    assert_eq!(a, b);
    assert!(a.contains("status"), "md keeps its header/content, got:\n{a}");
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets`
Expected: PASS — all snapshots green (non-TTY path unchanged).

- [ ] **Step 6: Commit**

```bash
git add src/render.rs
git commit -m "feat(render): fit table width to the terminal on a TTY (W37)"
```

---

### Task 7: `.output |cmd` pipe target (REPL)

**Files:**
- Modify: `src/output.rs` (`OutputSink::Command` variant + lifecycle)
- Modify: `src/repl.rs` (apply-output parsing of a leading `|`)
- Test: inline in output.rs

**Interfaces:**
- Produces: `OutputSink::open_command(cmd: &str) -> io::Result<Self>`;
  `OutputSink::Command(std::process::Child)`.

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn command_sink_pipes_blocks_through_the_shell() {
    let dir = tempdir().unwrap();
    let out = dir.path().join("piped.txt");
    // `cat > file` via sh -c: blocks written to the child's stdin land in the file.
    let mut sink = OutputSink::open_command(&format!("cat > {}", out.display())).unwrap();
    sink.write_block("alpha").unwrap();
    sink.write_block("beta").unwrap();
    sink.finish().unwrap(); // closes stdin, waits for the child
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "alpha\nbeta\n");
}
```

- [ ] **Step 2: Run, confirm failure**

Run: `cargo test --quiet output::tests::command_sink_pipes_blocks_through_the_shell`
Expected: FAIL — no `open_command`.

- [ ] **Step 3: Implement the sink**

In `src/output.rs`:
```rust
use std::process::{Child, Command, Stdio};

pub enum OutputSink {
    Stdout,
    File(File),
    /// A child process (spawned via `sh -c`) receiving results on its stdin.
    Command(Child),
}

impl OutputSink {
    pub fn open_command(cmd: &str) -> io::Result<Self> {
        let child = Command::new("sh")
            .arg("-c").arg(cmd)
            .stdin(Stdio::piped())
            .spawn()?;
        Ok(OutputSink::Command(child))
    }

    pub fn write_block(&mut self, block: &str) -> io::Result<()> {
        match self {
            OutputSink::Stdout => { println!("{block}"); Ok(()) }
            OutputSink::File(file) => writeln!(file, "{block}"),
            OutputSink::Command(child) => {
                let stdin = child.stdin.as_mut().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "child stdin closed")
                })?;
                writeln!(stdin, "{block}")
            }
        }
    }

    /// Closes a piped child's stdin and reaps it. No-op for Stdout/File.
    pub fn finish(&mut self) -> io::Result<()> {
        if let OutputSink::Command(child) = self {
            drop(child.stdin.take()); // EOF to the child
            child.wait()?;
        }
        Ok(())
    }
}
```

- [ ] **Step 4: Wire the REPL parsing**

In `src/repl.rs`, find the `.output` application (the `apply_output`/`Output(Some(arg))` path). Before switching to a new sink, call `sink.finish()` on the old one (so a prior pipe is reaped). Then:
```rust
// `.output |cmd` (sqlite3 convention): pipe results through a shell command.
if let Some(cmd) = arg.strip_prefix('|') {
    match OutputSink::open_command(cmd.trim()) {
        Ok(new_sink) => { *sink = new_sink; /* confirm on stderr */ }
        Err(err) => eprintln!("querymatter: cannot start pipe: {err}"),
    }
} else {
    // existing file path handling
}
```
`.output` / `.output stdout` reset: `sink.finish()?; *sink = OutputSink::Stdout;`. Also call `sink.finish()` once at REPL exit (after the loop in `run`). The `.output` argument currently strips whitespace — ensure a leading `|` survives parsing (the arg is taken verbatim after `.output`); adjust `parse_dot`'s `"output"` arm if it splits on whitespace so `|cmd with args` is kept whole (use the `rest_after_key`-style verbatim capture, like `.set`'s value).

- [ ] **Step 5: Run tests**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/output.rs src/repl.rs
git commit -m "feat(repl): .output |cmd pipes results through a shell command (W45)"
```

---

### Task 8: `.query save <name> [sql]` (REPL)

**Files:**
- Modify: `src/repl.rs` (`QueryCmd::Save`, parse, dispatch, `last_sql` tracking in `run`)
- Test: inline in repl.rs + a `tests/cli.rs` round-trip if convenient

**Interfaces:**
- Consumes: `queries::{load, save}` and the CLI's name/SQL validation (see
  `run_query_action`'s `Save` arm in `main.rs` — reuse the same validation).
- Produces: `QueryCmd::Save(String, Option<String>)`; REPL loop state `last_sql: Option<String>`.

- [ ] **Step 1: Write failing parse tests**

```rust
#[test]
fn parses_query_save_with_and_without_sql() {
    assert_eq!(parse_dot(".query save stale SELECT status"),
        DotCommand::Query(QueryCmd::Save("stale".into(), Some("SELECT status".into()))));
    assert_eq!(parse_dot(".query save stale"),
        DotCommand::Query(QueryCmd::Save("stale".into(), None)));
}
```

- [ ] **Step 2: Run, confirm failure**

Run: `cargo test --quiet repl::tests::parses_query_save_with_and_without_sql`
Expected: FAIL — no `QueryCmd::Save`.

- [ ] **Step 3: Implement parsing**

- Add `Save(String, Option<String>)` to `QueryCmd`.
- In `parse_dot`'s `"query"` arm, add before the `Some(action) => BadQueryAction`:
  ```rust
  Some(action) if action.eq_ignore_ascii_case("save") => match words.next() {
      Some(name) => {
          // SQL is everything after `query save <name>`, verbatim (may be empty).
          let sql = rest_after_key(rest, 3); // skip "query", "save", "<name>"
          DotCommand::Query(QueryCmd::Save(name.to_string(), sql))
      }
      None => DotCommand::MissingArg("query"),
  },
  ```

- [ ] **Step 4: Track last statement + dispatch save**

In `run`, add `let mut last_sql: Option<String> = None;` and set it after a successful statement:
```rust
Line::Statement(statement) => {
    run_statement(&session, &statement, &mut sink);
    last_sql = Some(statement.sql.clone());
}
```
Pass `last_sql.as_deref()` into `dispatch_dot` (widen its signature) or handle save in the loop. In the `.query save` dispatch:
```rust
QueryCmd::Save(name, sql) => {
    let sql = sql.or_else(|| last_sql.map(str::to_string));
    match sql {
        None => eprintln!("querymatter: .query save: no SQL given and no statement has run yet"),
        Some(sql) => match save_named_query(&name, &sql) { // reuse CLI validation
            Ok(path) => eprintln!("querymatter: saved query '{name}' in {}", path.display()),
            Err(err) => eprintln!("querymatter: {err:#}"),
        },
    }
}
```
Factor the CLI `query save` body (validate name chars, `query::parse(sql)?` to reject bad SQL, `queries::load` → insert → `queries::save`) from `main.rs`'s `run_query_action` into a shared `pub(crate) fn save_named_query(name: &str, sql: &str) -> anyhow::Result<PathBuf>` so REPL and CLI share one implementation (DRY). Update `main.rs` to call it too.

- [ ] **Step 5: Behavior test**

```rust
#[test]
fn query_save_rejects_when_no_prior_statement_and_no_sql() {
    // With SQL omitted and last_sql None, save must error (assert via the
    // dispatch path or the shared helper returning the "no SQL" condition).
}
```
Plus a `tests/cli.rs` (or queries.rs) round-trip: saving `.query save foo SELECT status` makes `foo` resolvable by `queries::get`.

- [ ] **Step 6: Run tests**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/repl.rs src/main.rs
git commit -m "feat(repl): .query save persists a query from the REPL (W46)"
```

---

### Task 9: `CASE WHEN` — AST, lowering, evaluation

**Files:**
- Modify: `src/query/ast.rs` (`Expr::Case` variant + its `referenced_fields` walk)
- Modify: `src/query/parse.rs` (`lower_expr` Case arm)
- Modify: `src/query/exec.rs` (`eval_expr`, `expr_columns`, `rewrite_expr_literals` Case arms)
- Test: inline in parse.rs / exec.rs

**Interfaces:**
- Produces: `Expr::Case { operand: Option<Box<Expr>>, whens: Vec<(Expr, Expr)>, else_expr: Option<Box<Expr>> }`.
- **Adding this variant makes every exhaustive `match` on `Expr` fail to
  compile.** Grep `Expr::Coalesce` to find every site that must also handle
  `Expr::Case`: `ast.rs` `referenced_fields` walk (~455), `exec.rs`
  `rewrite_expr_literals` (165), `expr_columns` (518), `eval_expr` (953). Fix all.

- [ ] **Step 1: Write failing end-to-end tests**

Add to `src/query/exec.rs` tests (mirror existing eval tests; build a `Record` and eval a parsed `Expr`, or run a full query if the test harness supports it):

```rust
#[test]
fn searched_case_selects_first_true_branch() {
    // SELECT CASE WHEN status='draft' THEN 'D' ELSE 'X' END
    // record status=draft -> "D"; status=done -> "X"
}

#[test]
fn simple_case_matches_operand() {
    // SELECT CASE status WHEN 'draft' THEN 'D' WHEN 'done' THEN 'Z' END
    // status=done -> "Z"; status=other -> NULL (no ELSE)
}
```
Write these against the same test scaffolding the file already uses for
`Coalesce`/scalar eval (find an existing `eval_expr` or end-to-end query test and
copy its shape).

- [ ] **Step 2: Run, confirm failure**

Run: `cargo test --quiet query::exec::tests::searched_case_selects_first_true_branch`
Expected: FAIL (won't parse / no `Expr::Case`).

- [ ] **Step 3: Add the AST variant**

In `src/query/ast.rs`, add to `enum Expr`:
```rust
/// `CASE [operand] WHEN cond THEN val ... [ELSE val] END`. Searched form has
/// `operand: None` (each WHEN is a boolean condition); simple form carries an
/// operand each WHEN value is compared against for equality.
Case {
    operand: Option<Box<Expr>>,
    whens: Vec<(Expr, Expr)>,
    else_expr: Option<Box<Expr>>,
},
```
Update the `referenced_fields`/`collect` walk (the exhaustive `match expr` near line 455) to recurse into `operand`, each `whens` pair, and `else_expr`.

- [ ] **Step 4: Lower from sqlparser**

In `src/query/parse.rs` `lower_expr`, add an arm for `sql::Expr::Case`:
```rust
sql::Expr::Case { operand, conditions, results, else_result, .. } => {
    let operand = operand.as_deref().map(lower_expr).transpose()?.map(Box::new);
    // sqlparser pairs conditions[i] with results[i].
    let whens = conditions.iter().zip(results.iter())
        .map(|(c, r)| Ok((lower_expr(c)?, lower_expr(r)?)))
        .collect::<Result<Vec<_>, ParseError>>()?;
    let else_expr = else_result.as_deref().map(lower_expr).transpose()?.map(Box::new);
    Ok(Expr::Case { operand, whens, else_expr })
}
```
(Confirm sqlparser 0.62's `Expr::Case` field names/shapes; recent versions use
`conditions: Vec<CaseWhen>` where each holds `condition`+`result`. Adapt the zip
accordingly — the shape is the only thing to verify, the lowering logic is the
same.) Because `lower_expr` recurses, relative-date string literals inside CASE
arms are lowered to `RelDate` automatically.

- [ ] **Step 5: Evaluate + rewrite + columns**

In `src/query/exec.rs`:
- `rewrite_expr_literals` — add:
  ```rust
  Expr::Case { operand, whens, else_expr } => {
      if let Some(op) = operand { rewrite_expr_literals(op, now); }
      for (c, r) in whens { rewrite_expr_literals(c, now); rewrite_expr_literals(r, now); }
      if let Some(e) = else_expr { rewrite_expr_literals(e, now); }
  }
  ```
- `expr_columns` — add a Case arm that flat-maps `expr_columns` over operand, both halves of each when, and else.
- `eval_expr` — add:
  ```rust
  Expr::Case { operand, whens, else_expr } => {
      match operand {
          None => { // searched
              for (cond, then) in whens {
                  if is_truthy(&eval_expr(record, cond)) { return eval_expr(record, then); }
              }
          }
          Some(op) => { // simple
              let target = eval_expr(record, op);
              for (val, then) in whens {
                  if values_equal(&target, &eval_expr(record, val)) { return eval_expr(record, then); }
              }
          }
      }
      else_expr.as_ref().map_or(Value::Null, |e| eval_expr(record, e))
  }
  ```
  Reuse the existing truthiness/equality helpers (find how `WHERE` evaluates a
  boolean and how `compare_values` does equality; `is_truthy`/`values_equal`
  here stand for those existing helpers — use the real names).

- [ ] **Step 6: Run tests**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/query/ast.rs src/query/parse.rs src/query/exec.rs
git commit -m "feat(query): CASE WHEN (searched + simple) expressions (W38)"
```

---

### Task 10: `CASE WHEN` — invariant coverage at the crossed seams

Pins the seams the compiler forced but can't prove correct (per spec-discipline).

**Files:**
- Test: `src/query/parse.rs` and/or `src/query/exec.rs`

- [ ] **Step 1: Relative-date recursion into CASE arms**

Mirror the existing "rewrite recurses into COALESCE arguments" test for CASE: a
query with a relative-date literal (e.g. `'-7d'`) inside a CASE arm must have that
literal resolved to a concrete ISO date after `rewrite_relative_dates`. Assert the
arm's literal is no longer a `Literal::RelativeDate`.

- [ ] **Step 2: Unknown-column validation walks CASE arms**

A `SELECT CASE WHEN bogus_col = 'x' THEN 1 END` must be rejected in strict mode
(unknown column `bogus_col`) and treated as NULL in `--lenient`. Assert both,
proving `expr_columns`/validation descends into CASE.

- [ ] **Step 3: CASE in WHERE and ORDER BY**

```rust
#[test]
fn case_usable_in_where_and_order_by() {
    // WHERE (CASE WHEN status='draft' THEN 1 ELSE 0 END) = 1  -> only drafts
    // ORDER BY CASE WHEN status='draft' THEN 0 ELSE 1 END     -> drafts first
}
```

- [ ] **Step 4: Run + commit**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets`
```bash
git add src/query/parse.rs src/query/exec.rs
git commit -m "test(query): pin CASE date-rewrite recursion, validation, WHERE/ORDER BY (W38)"
```

---

### Task 11: Single-pass multi-aggregate GROUP BY

**Files:**
- Modify: `src/query/exec.rs` (`project_group` / `compute_aggregate`)
- Test: inline in exec.rs

- [ ] **Step 1: Write failing equivalence test**

```rust
#[test]
fn multiple_aggregates_per_group_match_single_pass() {
    // GROUP BY status SELECT status, COUNT(*), SUM(estimate), AVG(estimate),
    //   MIN(estimate), MAX(estimate)
    // Assert the exact expected values for a small fixed record set (so the
    // test also pins correctness, not just internal equivalence).
}
```

- [ ] **Step 2: Run, confirm it passes today (characterization) then refactor**

Run: `cargo test --quiet query::exec::tests::multiple_aggregates_per_group_match_single_pass`
Expected: PASS on the current code (this is a characterization test). Keep it green through the refactor.

- [ ] **Step 3: Refactor `project_group` to one pass**

Restructure so a single iteration over `group.rows` feeds every aggregate's
accumulator, instead of calling `compute_aggregate` (which itself scans
`group.rows`) once per aggregate SELECT item. Preserve every aggregate's exact
result semantics (COUNT/COUNT DISTINCT/SUM/AVG/MIN/MAX/GROUP_CONCAT). No public
interface change.

- [ ] **Step 4: Run tests**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets`
Expected: PASS (the new test plus all existing aggregate tests).

- [ ] **Step 5: Commit**

```bash
git add src/query/exec.rs
git commit -m "perf(query): fold a group's aggregates into one pass (W40)"
```

---

### Task 12: Bounded top-k for `ORDER BY ... LIMIT`

**Files:**
- Modify: `src/query/exec.rs` (`execute_ungrouped`, `execute_grouped`)
- Test: inline in exec.rs

**Interfaces:**
- Consumes: `order_cmp` (exec.rs:1302) and the multi-key ordering comparators.

- [ ] **Step 1: Write failing tie-stability test**

```rust
#[test]
fn order_by_limit_preserves_tie_input_order_like_full_sort() {
    // Records with several equal sort keys and distinct ids. ORDER BY <key>
    // LIMIT k must return the SAME rows in the SAME order as a full stable
    // sort then take(k) — i.e. equal-key rows keep input order.
}

#[test]
fn order_by_limit_offset_window_matches_full_sort() {
    // ORDER BY key DESC LIMIT 3 OFFSET 2 == full-sort().skip(2).take(3)
}
```

- [ ] **Step 2: Run, confirm current pass (characterization)**

Run: `cargo test --quiet query::exec::tests::order_by_limit_preserves_tie_input_order_like_full_sort`
Expected: PASS today (full sort is stable). This locks the behavior the top-k
rewrite must preserve.

- [ ] **Step 3: Implement bounded selection**

In `execute_ungrouped` and `execute_grouped`, when `q.limit.is_some()`, replace
`rows.sort_by(order_cmp…)` + `skip(offset).take(limit)` with a bounded top-k
keeping only `offset + limit` rows. Carry each row's **original index** as a
final ascending tiebreaker in the heap comparator so equal-key rows retain input
order (byte-identical to the stable full sort). When `q.limit` is `None`, keep
the existing full sort. Extract a small helper if it keeps both call sites DRY:
```rust
/// Top `n` rows under `cmp`, ties broken by original index (stable), sorted.
fn bounded_top_k<T>(rows: Vec<T>, n: usize, cmp: impl Fn(&T, &T) -> Ordering) -> Vec<T> { … }
```

- [ ] **Step 4: Run tests**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets`
Expected: PASS (new tests + all existing ordering/limit tests).

- [ ] **Step 5: Commit**

```bash
git add src/query/exec.rs
git commit -m "perf(query): bounded top-k for ORDER BY + LIMIT, stable ties (W44)"
```

---

### Task 13: `cache status` subcommand

**Files:**
- Modify: `src/cli.rs` (`Command::Cache`, `CacheArgs`, `CacheAction`)
- Modify: `src/cache.rs` (a `cache_summary` function computing the report)
- Modify: `src/main.rs` (dispatch `Command::Cache`)
- Test: inline in cache.rs + `tests/cli.rs`

**Interfaces:**
- Consumes: `cache::{find_vault, cache_dir, load_cache}`, `ManifestBody`,
  `ManifestEntry`.
- Produces: `pub struct CacheSummary { pub root: PathBuf, pub dir_count: usize,
  pub file_count: usize, pub bytes: u64, pub ttl_secs: u64, pub crate_version:
  String, pub dirs: Vec<(PathBuf, SystemTime)> }`;
  `pub fn cache_summary(vault_dir: &Path) -> anyhow::Result<CacheSummary>`.

- [ ] **Step 1: Write failing cache test**

```rust
#[test]
fn cache_summary_reports_counts_size_and_ttl() {
    // Build a temp vault via build_vault over a dir with N md files, then:
    let s = cache_summary(&vault).unwrap();
    assert_eq!(s.file_count, N);
    assert!(s.dir_count >= 1);
    assert!(s.bytes > 0);
    assert_eq!(s.ttl_secs, /* the ttl passed to build_vault */);
    assert_eq!(s.dirs.len(), s.dir_count);
}
```

- [ ] **Step 2: Run, confirm failure**

Run: `cargo test --quiet cache::tests::cache_summary_reports_counts_size_and_ttl`
Expected: FAIL — no `cache_summary`.

- [ ] **Step 3: Implement `cache_summary`**

In `src/cache.rs`:
```rust
pub fn cache_summary(vault_dir: &Path) -> anyhow::Result<CacheSummary> {
    let (body, dirs) = load_cache(vault_dir)
        .context("no readable .querymatter cache found")?;
    let file_count = dirs.iter().map(|d| d.files.len()).sum();
    let cache = cache_dir(vault_dir);
    let mut bytes = 0u64;
    for entry in std::fs::read_dir(&cache)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            bytes += entry.metadata()?.len();
        }
    }
    Ok(CacheSummary {
        root: vault_dir.to_path_buf(),
        dir_count: body.dirs.len(),
        file_count,
        bytes,
        ttl_secs: body.ttl_secs,
        crate_version: body.crate_version.clone(),
        dirs: body.dirs.iter().map(|e| (e.dir.clone(), e.scanned_at)).collect(),
    })
}
```

- [ ] **Step 4: CLI subcommand + dispatch + rendering**

In `src/cli.rs`:
```rust
// in enum Command:
/// Inspect the .querymatter cache.
Cache(CacheArgs),

#[derive(Debug, Args)]
pub struct CacheArgs {
    #[command(subcommand)]
    pub action: CacheAction,
}

#[derive(Debug, Subcommand)]
pub enum CacheAction {
    /// Show cache location, size, counts, TTL, and per-directory scan times.
    Status {
        /// Directory whose vault to inspect; defaults to the current directory.
        dir: Option<PathBuf>,
    },
}
```
In `src/main.rs`, dispatch `Command::Cache` (alongside `Config`/`Query`/`Explain`):
resolve the start dir (arg or cwd), `find_vault(&start)` → error naming
`querymatter init` if `None`, else `cache_summary` and print an aligned report to
**stdout** (inspection output). Size formatted human-readably (bytes/KiB/MiB);
scan times via a simple RFC3339/`%Y-%m-%d %H:%M` rendering (chrono is already a
dep).

- [ ] **Step 5: Integration test**

Add to `tests/cli.rs`: `querymatter cache status <dir-with-a-vault>` exits 0 and
its stdout contains the vault root and a file count; `cache status` in a dir with
no vault exits non-zero and mentions `init`.

- [ ] **Step 6: Run tests**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/cache.rs src/cli.rs src/main.rs tests/cli.rs
git commit -m "feat(cache): querymatter cache status introspection command (W33)"
```

---

### Task 14: Multi-statement failure attribution

**Files:**
- Modify: `src/main.rs` (`run_statements`)
- Test: `tests/cli.rs`

- [ ] **Step 1: Write failing integration test**

```rust
#[test]
fn batch_failure_names_the_statement_index() {
    // Pipe three statements; statement 2 is invalid SQL.
    Command::cargo_bin("querymatter").unwrap()
        .arg("-e").arg("SELECT status; SELECT bogus((( ; SELECT status")
        .arg(fixture_dir())
        .assert().failure()
        .stderr(predicates::str::contains("statement 2 of 3"));
}

#[test]
fn single_statement_failure_is_not_indexed() {
    Command::cargo_bin("querymatter").unwrap()
        .arg("-e").arg("SELECT bogus(((").arg(fixture_dir())
        .assert().failure()
        .stderr(predicates::str::contains("statement 1 of 1").not());
}
```
(Use the same statement-splitting the tool applies to `-e`; if `-e` runs a single
statement, use piped/batch stdin instead — match `run_statements`' real entry
point.)

- [ ] **Step 2: Run, confirm failure**

Run: `cargo test --quiet --test cli batch_failure_names_the_statement_index`
Expected: FAIL — error has no index.

- [ ] **Step 3: Implement**

In `src/main.rs` `run_statements`:
```rust
let statements = split_statements(input);
let total = statements.len();
let mut total_rows = 0;
for (i, statement) in statements.iter().enumerate() {
    let (rendered, rows) = session
        .render_statement_counted(statement)
        .map_err(|e| if total > 1 {
            e.context(format!("statement {} of {}", i + 1, total))
        } else { e })?;
    sink.write_block(&rendered).context("failed to write query results")?;
    total_rows += rows;
}
Ok(total_rows)
```

- [ ] **Step 4: Run tests**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs tests/cli.rs
git commit -m "feat(cli): attribute batch failures to their statement index (W36)"
```

---

### Task 15: Documentation

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update README**

Add/extend:
- Flags table / options: `--header` / `--no-header`, `--quiet` / `-q` / `--no-quiet`.
- Subcommands: `querymatter cache status [DIR]`.
- REPL dot-commands section: `.timer [on|off]`, `.header [on|off]`, `.query save
  <name> [sql]`, `.output |cmd`.
- The query DSL section: `CASE WHEN … THEN … [ELSE …] END` (searched and simple).
- The config-keys table: `timer` (bool, default false), `header` (bool, default
  true), `quiet` (bool, default false).
- A one-line note that `config set timer true` (etc.) makes a REPL toggle the
  durable default.

- [ ] **Step 2: Verify no stale claims**

Skim the README for statements the bundle contradicts (e.g. "csv/tsv always have
a header row", "the only pager escape is `\G`") and correct them.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: document W32-W46 flags, commands, config keys, and CASE WHEN"
```

---

## Self-Review

**Spec coverage** — every spec item maps to a task:
W32 → T1 (config/flag) + T2 (render) + T3 (.header); W33 → T13; W34 → T1 + T5;
W35 → T1 + T4; W36 → T14; W37 → T6; W38 → T9 + T10; W40 → T11; W44 → T12;
W45 → T7; W46 → T8. Docs → T15. No gaps.

**Placeholders** — remaining "use the real helper name" notes (T9's
`is_truthy`/`values_equal`, T3's Session test builder, T5's warning trigger) are
deliberate pointers to existing code the implementer must grep, not missing
content; each names the exact function/site to locate. sqlparser 0.62's
`Expr::Case` field shape is flagged for verification in T9.

**Type consistency** — `render(…, header: bool)` (T2) is consumed by T6's
`render_table`; `Session::header/timer/set_header/set_timer` defined in T2/T4 and
used in T3/T4; `OutputSink::{Command, open_command, finish}` (T7) used by T7's
REPL wiring; `Expr::Case { operand, whens, else_expr }` (T9) used identically in
T9's four match sites and T10's tests; `cache_summary`/`CacheSummary` (T13)
consistent across cache.rs and main.rs; `save_named_query` (T8) shared by REPL
and CLI. Consistent.
