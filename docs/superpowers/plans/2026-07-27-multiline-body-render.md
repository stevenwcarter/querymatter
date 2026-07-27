# Multiline Value Rendering Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Multiline values (`file.body`, multiline frontmatter strings) render their newlines as real line breaks in `table` and `\G` output instead of U+FFFD blobs, and as `<br>` in `md` output.

**Architecture:** All changes live in `src/render.rs`. The REPL and CLI batch mode share one render path (`session::render_statement_to` → `render::render_to`), so both surfaces are fixed by construction. `sanitize_for_terminal` gains `\n` as a second exempt character (with `\r\n` → `\n` normalization); comfy-table's native multiline-cell support and `render_vertical`'s verbatim printing do the rest. `new_table`'s `sanitize: bool` becomes a two-variant `CellEscape` enum so the md path can escape line breaks as `<br>` without becoming terminal-sanitized.

**Tech Stack:** Rust (edition 2024), comfy-table, insta snapshots, assert_cmd integration tests.

**Spec:** `docs/superpowers/specs/2026-07-27-multiline-body-render-design.md`

## Global Constraints

- Binary-only crate: run tests with plain `cargo test` (there is no lib target; `cargo test --lib` fails).
- No pre-commit hook: run `cargo fmt` and `cargo clippy --all-targets -- -D warnings` yourself before every commit.
- `cargo-insta` is NOT installed. Create new snapshots with `INSTA_UPDATE=always cargo test <name>`, then inspect the generated `.snap` file content and re-run plain `cargo test` to confirm green. After snapshot creation, check `git status` — only the snapshots you intended may appear.
- json/csv/tsv output must not change by a single byte (interchange invariant). The full suite pins this; never touch those paths.
- Existing integration tests in `tests/cli.rs` (`table_output_neutralizes_ansi_escapes_from_frontmatter`, `vertical_output_neutralizes_ansi_escapes_from_frontmatter`, `json/csv/md_output_unchanged_by_terminal_sanitizer`) must stay green untouched.
- End every commit message with the trailer line `Claude-Session: https://claude.ai/code/session_01BsfkatoCFtfkZbXmdDkBnH`.

---

### Task 1: Sanitizer exempts `\n` (multiline table + `\G` rendering)

**Files:**
- Modify: `src/render.rs` (`sanitize_for_terminal` ~line 334 and its doc comment; tests module)
- Modify: `tests/cli.rs` (one new integration test, near the B3 tests ~line 3948)
- Create: `src/snapshots/querymatter__render__tests__table_multiline_cell.snap` and `...__vertical_multiline_value.snap` (via insta)

**Interfaces:**
- Produces: `sanitize_for_terminal(&str) -> Cow<'_, str>` with the NEW contract: `\t` and `\n` pass through; `\r\n` collapses to `\n`; every other control char (lone `\r`, ESC, all other C0/C1) becomes U+FFFD. Task 2 relies on this exact behavior staying in place for `CellEscape::Terminal`.

- [ ] **Step 1: Rewrite the newline unit test and add CRLF tests (RED)**

In `src/render.rs`'s `mod tests`, DELETE the test `sanitize_for_terminal_neutralizes_newline` (its doc comment says it pins that `\n` is neutralized — this plan deliberately inverts that contract) and add in its place:

```rust
    /// Inverts the old FIX 4 pin on purpose: `\n` is real content in
    /// multiline values like `file.body` (comfy-table renders it as lines
    /// inside a cell; `\G` prints it raw like mysql), so it is exempt from
    /// sanitization alongside `\t`. See the 2026-07-27 multiline-render spec.
    #[test]
    fn sanitize_for_terminal_preserves_newline() {
        let sanitized = sanitize_for_terminal("before\nafter");
        assert_eq!(sanitized, "before\nafter");
        assert!(
            matches!(sanitized, Cow::Borrowed(_)),
            "newline-only input must take the no-alloc fast path"
        );
    }

    /// `\r\n` collapses to `\n` so CRLF-authored files don't render a U+FFFD
    /// blob at the end of every line — the very bug the newline exemption
    /// fixes, in Windows trim.
    #[test]
    fn sanitize_for_terminal_collapses_crlf() {
        assert_eq!(sanitize_for_terminal("a\r\nb\r\nc"), "a\nb\nc");
    }

    /// A lone `\r` is a real cursor-abuse vector (line overwrite forgery) and
    /// stays neutralized — only the two-char `\r\n` sequence is normalized.
    #[test]
    fn sanitize_for_terminal_still_neutralizes_lone_cr() {
        assert_eq!(sanitize_for_terminal("a\rb"), "a\u{FFFD}b");
    }
```

Keep `sanitize_for_terminal_neutralizes_control_bytes_but_keeps_tab` exactly as is (its input `"before\u{1b}[2Jafter\rend\ttab"` carries a *lone* `\r`, so it still passes under the new contract).

- [ ] **Step 2: Run the new tests to verify they fail**

Run: `cargo test sanitize_for_terminal`
Expected: `sanitize_for_terminal_preserves_newline`, `sanitize_for_terminal_collapses_crlf`, and `sanitize_for_terminal_still_neutralizes_lone_cr` FAIL (current code turns `\n` into U+FFFD and does not collapse CRLF); `..._neutralizes_control_bytes_but_keeps_tab` passes.

- [ ] **Step 3: Implement the new sanitizer contract**

Replace `sanitize_for_terminal`'s body and doc comment in `src/render.rs`:

```rust
/// Neutralizes C0/C1 control characters — ESC, lone `\r`, and their kin — so
/// a frontmatter value can't forge terminal escape sequences (screen clears,
/// cursor moves, forged rows) when rendered into a table cell or vertical
/// (`\G`) line. `\t` and `\n` are exempt: tabs are harmless, and newlines
/// are real content in multiline values like `file.body` — comfy-table
/// renders them as lines *inside* a cell's borders (they can't forge rows),
/// and vertical output prints them raw exactly as `mysql`'s `\G` does.
/// `\r\n` collapses to `\n` first, so a CRLF-authored file doesn't render a
/// U+FFFD at every line end; a lone `\r` (a line-overwrite vector) still
/// becomes U+FFFD.
///
/// Used by the interactive [`Format::Table`] path (via [`new_table`]) and the
/// vertical ([`render_vertical`]) path. Never call this from the
/// csv/tsv/json paths — those are the stable, byte-identity interchange
/// contract (see the module doc) and must keep receiving raw
/// [`Value::display`] output untouched.
fn sanitize_for_terminal(s: &str) -> Cow<'_, str> {
    if !s.chars().any(|c| c.is_control() && c != '\t' && c != '\n') {
        return Cow::Borrowed(s);
    }
    // `\r` fails the fast path whether lone or part of a CRLF, so the
    // collapse below runs on every string that could need it.
    Cow::Owned(
        s.replace("\r\n", "\n")
            .chars()
            .map(|c| {
                if c.is_control() && c != '\t' && c != '\n' {
                    '\u{FFFD}'
                } else {
                    c
                }
            })
            .collect(),
    )
}
```

(The `[`Format::Md`]`/`sanitize: false` sentences from the old doc comment are dropped here; Task 2 rewrites that cross-reference when the bool becomes `CellEscape`. If Task 2 hasn't run yet, keep the old sentence "[`Format::Md`] shares [`new_table`] but calls it with `sanitize: false` … must stay terminal-independent." verbatim.)

- [ ] **Step 4: Run the sanitizer tests to verify they pass**

Run: `cargo test sanitize_for_terminal`
Expected: all 4 PASS.

- [ ] **Step 5: Add rendering-seam tests (snapshots + ESC regression + CLI)**

In `src/render.rs`'s `mod tests`, add:

```rust
    /// A multiline value (the `file.body` case) renders as multiple lines
    /// inside its table cell — borders intact, no U+FFFD blobs.
    #[test]
    fn table_multiline_cell() {
        let t = ResultTable {
            headers: vec!["path".into(), "body".into()],
            rows: vec![vec![
                Value::Str("a.md".into()),
                Value::Str("# Title\n\nFirst paragraph.\nSecond line.".into()),
            ]],
        };
        insta::assert_snapshot!(render(
            &t,
            Output::Format(Format::Table),
            TableStyle::Ascii,
            true
        ));
    }

    /// `\G` prints multiline values raw, mysql-style: continuation lines
    /// start at column 0.
    #[test]
    fn vertical_multiline_value() {
        let t = ResultTable {
            headers: vec!["path".into(), "body".into()],
            rows: vec![vec![
                Value::Str("a.md".into()),
                Value::Str("# Title\n\nFirst paragraph.".into()),
            ]],
        };
        insta::assert_snapshot!(render(&t, Output::Vertical, TableStyle::Ascii, true));
    }

    /// The newline exemption must not weaken the B3 security fix: an ESC
    /// riding inside a multiline value still becomes U+FFFD in table output.
    #[test]
    fn table_multiline_still_neutralizes_esc() {
        let t = ResultTable {
            headers: vec!["body".into()],
            rows: vec![vec![Value::Str("line one\n\u{1b}[2Jline two".into())]],
        };
        let s = render(&t, Output::Format(Format::Table), TableStyle::Ascii, true);
        assert!(!s.contains('\u{1b}'), "raw ESC leaked, got:\n{s}");
        assert!(s.contains('\u{FFFD}'), "ESC must become U+FFFD, got:\n{s}");
    }
```

In `tests/cli.rs`, directly after `table_output_neutralizes_ansi_escapes_from_frontmatter` (~line 3968), add the end-to-end pin (a multiline frontmatter string exercises the same render path `file.body` uses, without needing the body-read gate):

```rust
/// The multiline-render fix end-to-end: a value containing `\n` renders its
/// lines in the default table format — no U+FFFD blobs. Exercises the same
/// sanitize-then-render path `file.body` uses.
#[test]
fn table_output_renders_multiline_values() {
    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("multi.md"),
        "---\ntitle: \"line one\\nline two\"\n---\n",
    )
    .unwrap();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("-e")
        .arg("SELECT title")
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("line one"))
        .stdout(predicate::str::contains("line two"))
        .stdout(predicate::str::contains("\u{FFFD}").not());
}
```

- [ ] **Step 6: Create the snapshots, inspect them, verify green**

Run: `INSTA_UPDATE=always cargo test table_multiline_cell vertical_multiline_value` — the two new `.snap` files are written under `src/snapshots/`.
Then READ both `.snap` files and verify: the table snapshot shows `# Title`, the blank line, `First paragraph.`, and `Second line.` as four lines inside the `body` cell with `|` borders on every line; the vertical snapshot shows `body: # Title` with the following lines at column 0. No `\u{FFFD}` anywhere.
Run: `cargo test` (full suite)
Expected: all PASS (including the untouched B3 integration tests). `git status` shows only the two intended new `.snap` files plus the edited sources.

- [ ] **Step 7: Format, lint, commit**

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
git add src/render.rs src/snapshots tests/cli.rs
git commit -m "fix: render newlines in table and \G output instead of U+FFFD

\n joins \t as a sanitizer-exempt character: comfy-table renders it as
lines inside a cell's borders, and \G prints it raw like mysql. \r\n
collapses to \n so CRLF files don't blob at every line end. ESC, lone
\r, and all other C0/C1 controls still become U+FFFD (B3 stands).

Claude-Session: https://claude.ai/code/session_01BsfkatoCFtfkZbXmdDkBnH"
```

---

### Task 2: md format escapes line breaks as `<br>`

**Files:**
- Modify: `src/render.rs` (`new_table` ~line 372, `render_table` ~line 254, `render_markdown` ~line 314, `sanitized_display` doc ~line 351; tests module)

**Interfaces:**
- Consumes: `sanitize_for_terminal` with Task 1's contract (`\t`/`\n` exempt, CRLF collapsed).
- Produces: private `enum CellEscape { Terminal, Markdown }` replacing `new_table`'s `sanitize: bool` parameter; private `fn escape_md_linebreaks(&str) -> Cow<'_, str>`. Nothing outside `render.rs` changes.

- [ ] **Step 1: Write the failing md tests (RED)**

In `src/render.rs`'s `mod tests`, add:

```rust
    /// md cells turn every line-break form into `<br>` — the Markdown-table
    /// way to hold multiline content — so each row stays one physical line.
    #[test]
    fn md_multiline_cell_becomes_br() {
        let t = ResultTable {
            headers: vec!["body".into()],
            rows: vec![vec![Value::Str("a\r\nb\nc\rd".into())]],
        };
        let s = render(&t, Output::Format(Format::Md), TableStyle::Ascii, true);
        assert!(s.contains("a<br>b<br>c<br>d"), "got:\n{s}");
        for line in s.lines() {
            assert!(
                line.starts_with('|') && line.ends_with('|'),
                "a row split across physical lines, got:\n{s}"
            );
        }
    }

    /// Headers take the same escaping — a newline-bearing alias can't split
    /// the md header row either.
    #[test]
    fn md_multiline_header_becomes_br() {
        let t = ResultTable {
            headers: vec!["two\nlines".into()],
            rows: vec![vec![Value::Int(1)]],
        };
        let s = render(&t, Output::Format(Format::Md), TableStyle::Ascii, true);
        assert!(s.contains("two<br>lines"), "got:\n{s}");
    }

    /// `\r\n` becomes ONE `<br>`, and the escape only fires on line breaks —
    /// clean strings ride the borrowed fast path.
    #[test]
    fn escape_md_linebreaks_handles_all_break_forms() {
        assert_eq!(escape_md_linebreaks("a\r\nb"), "a<br>b");
        assert_eq!(escape_md_linebreaks("a\nb"), "a<br>b");
        assert_eq!(escape_md_linebreaks("a\rb"), "a<br>b");
        assert!(matches!(escape_md_linebreaks("plain"), Cow::Borrowed(_)));
    }
```

- [ ] **Step 2: Run the new tests to verify they fail**

Run: `cargo test md_multiline escape_md_linebreaks`
Expected: `escape_md_linebreaks_handles_all_break_forms` fails to COMPILE (function doesn't exist) — that is the RED signal; comment it out momentarily if needed to see the other two FAIL (raw newlines split the rows), then restore it.

- [ ] **Step 3: Implement `CellEscape` and `escape_md_linebreaks`**

In `src/render.rs`, add above `new_table`:

```rust
/// How [`new_table`] transforms cell and header text before it enters the
/// table: terminal sanitization for [`Format::Table`], line-break escaping
/// for [`Format::Md`].
#[derive(Clone, Copy)]
enum CellEscape {
    /// Neutralize terminal-escape vectors via [`sanitize_for_terminal`].
    Terminal,
    /// Replace `\r\n`/`\n`/`\r` with `<br>` so each Markdown row stays one
    /// physical line. Everything else — including ESC — passes through raw:
    /// md is a fixed interchange format, not a terminal display, and `<br>`
    /// is dialect syntax, not sanitization.
    Markdown,
}

/// Applies `escape`'s transform, borrowing when the string needs no change.
fn escape_cell(s: &str, escape: CellEscape) -> Cow<'_, str> {
    match escape {
        CellEscape::Terminal => sanitize_for_terminal(s),
        CellEscape::Markdown => escape_md_linebreaks(s),
    }
}

/// Replaces line breaks with `<br>` for Markdown table cells and headers.
/// `\r\n` is replaced first so a CRLF becomes one `<br>`, not two.
fn escape_md_linebreaks(s: &str) -> Cow<'_, str> {
    if !s.contains(['\n', '\r']) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(s.replace("\r\n", "<br>").replace(['\n', '\r'], "<br>"))
}
```

Replace `new_table` (delete its old `sanitize: bool` plumbing entirely):

```rust
/// A `comfy-table` [`Table`] carrying `table`'s headers (unless `header` is
/// `false`) and rows, with no preset loaded yet, cells and headers
/// transformed per `escape`: [`render_table`] (`Format::Table`) passes
/// [`CellEscape::Terminal`], [`render_markdown`] (`Format::Md`) passes
/// [`CellEscape::Markdown`].
fn new_table(table: &ResultTable, header: bool, escape: CellEscape) -> Table {
    let mut ct = Table::new();
    if header {
        ct.set_header(table.headers.iter().map(|h| escape_cell(h, escape)));
    }
    for row in &table.rows {
        ct.add_row(row.iter().map(|value| {
            // Like `sanitized_display`: reuse the owned display string when
            // the escape hands back a borrow of it unchanged.
            let s = value.display();
            match escape_cell(&s, escape) {
                Cow::Borrowed(_) => s,
                Cow::Owned(escaped) => escaped,
            }
        }));
    }
    ct
}
```

Update the two callers:
- `render_table`: `let mut ct = new_table(table, header, CellEscape::Terminal);`
- `render_markdown`: `let mut ct = new_table(table, header, CellEscape::Markdown);`

Update stale doc comments:
- `render_markdown`'s doc: replace the "Passes `sanitize: false` …" paragraph with: "Passes [`CellEscape::Markdown`] to [`new_table`]: line breaks become `<br>` (a md-table row must be one physical line), but cells are otherwise raw — Markdown is a fixed interchange format (see [`render_table`]'s doc), so its cells must NOT go through [`sanitize_for_terminal`]."
- `sanitized_display`'s doc: now used by the vertical (`\G`) path only — say so ("Used by the [`render_vertical`] (`\G`) path; the table path does the same dance inline in [`new_table`]").
- If Task 1 left the old "`[`Format::Md`] shares [`new_table`] but calls it with `sanitize: false`" sentence in `sanitize_for_terminal`'s doc, rewrite it to reference `CellEscape::Markdown`.

- [ ] **Step 4: Run the md tests to verify they pass**

Run: `cargo test md_ escape_md_linebreaks`
Expected: the three new tests PASS, and the existing `md_no_header_omits_header_row`, `md_snapshot`, `markdown_render_is_terminal_independent` still PASS.

- [ ] **Step 5: Run the full suite**

Run: `cargo test`
Expected: all PASS — in particular `md_output_unchanged_by_terminal_sanitizer` in `tests/cli.rs` (ESC still survives md raw; `escape_md_linebreaks` only touches `\r`/`\n`) and `non_table_formats_ignore_style`.

- [ ] **Step 6: Format, lint, commit**

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
git add src/render.rs
git commit -m "fix: escape line breaks as <br> in md table cells

A raw newline in a cell split a Markdown table row across physical
lines, breaking the table syntax. new_table's sanitize bool becomes a
CellEscape enum: Terminal keeps the B3 sanitizer, Markdown replaces
\r\n / \n / \r with one <br> each in cells and headers. md stays
otherwise raw — ESC still passes through untouched.

Claude-Session: https://claude.ai/code/session_01BsfkatoCFtfkZbXmdDkBnH"
```

---

## Self-review notes

- Spec coverage: decision 1+2 (table/`\G` multiline) → Task 1; decision 3 (md `<br>`) → Task 2; CRLF normalization → Task 1; interchange untouched → no task touches those paths, full-suite runs pin it; every test in the spec's test plan appears verbatim in a task.
- The two tasks are independently shippable: Task 1 alone fixes the reported bug; Task 2 alone fixes the md latent bug (its tests don't depend on Task 1's `\n` exemption since md never called the sanitizer).
- Type consistency: `CellEscape` is defined in Task 2 and used only there; Task 1 touches only `sanitize_for_terminal` internals, so the tasks can't drift apart on signatures.
