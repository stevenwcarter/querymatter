# Multiline value rendering (`file.body` newline blobs) — design

**Date:** 2026-07-27
**Status:** Approved
**Problem:** `SELECT file.body` (or any multiline string value) renders every
`\n` as U+FFFD — a "blob with a question mark" — in the interactive `table`
format and vertical `\G` output, in both the REPL and CLI batch mode.

## Root cause

`render.rs`'s `sanitize_for_terminal` (security fix B3) neutralizes **all**
control characters except `\t` to U+FFFD, to stop frontmatter values from
forging terminal escape sequences. `\n` was swept up with the genuinely
dangerous characters (ESC, `\r`, C1 controls). Two tests pin that behavior
deliberately (`sanitize_for_terminal_neutralizes_control_bytes_but_keeps_tab`,
`sanitize_for_terminal_neutralizes_newline`); this design *changes that
contract on purpose*.

A separate latent bug rides along: the `md` format is unsanitized, so a raw
newline in a cell splits a Markdown table row across physical lines, breaking
the table syntax.

## Decisions (from Q&A)

1. **Table format: full multiline cells.** comfy-table renders `\n` inside a
   cell natively — multiple lines within the cell's borders. Newlines cannot
   forge rows because the cell box contains them. No truncation, no `\n`
   escaping.
2. **Vertical `\G`: raw newlines, MySQL parity.** Continuation lines start at
   column 0, exactly like `mysql`'s `\G`. (A crafted body can visually mimic a
   `label: value` line; MySQL accepts the same, and we accept it here.)
3. **`md` format: escape line breaks as `<br>`.** Each of `\r\n`, `\n`, and
   lone `\r` in a cell (and header) becomes `<br>` — the standard way to get
   multiline content into a Markdown table cell while keeping each row one
   physical line.

## Design

All changes live in `src/render.rs`. The REPL and CLI batch mode share one
render path (`session::render_statement_to` → `render::render_to`), so both
surfaces are fixed by construction — no per-surface work.

### 1. `sanitize_for_terminal`: exempt `\n`, normalize CRLF

New contract: **`\t` and `\n` pass through; `\r\n` normalizes to `\n`; every
other control character (lone `\r`, ESC, all other C0/C1) becomes U+FFFD.**

- Fast path (borrow, no allocation): no char is a control character other than
  `\t`/`\n`. A string containing `\r` — lone or as part of `\r\n` — fails this
  test (`\r` is a non-exempt control char), so all CRLF handling happens on
  the slow path only.
- Slow path: replace `"\r\n"` with `"\n"` first, then map remaining control
  chars (except `\t`, `\n`) to U+FFFD.

CRLF normalization exists so a Windows-authored file doesn't show a U+FFFD
blob at the end of every line — which would be this very bug all over again.
It applies to the terminal paths only (table + vertical); interchange formats
never call the sanitizer.

The doc comments on `sanitize_for_terminal` and `sanitized_display` are
updated to state the new contract and why `\n` is safe in each caller (table:
comfy-table contains it in the cell box; vertical: MySQL-parity raw output).

### 2. Table format: no further change

With `\n` surviving sanitization, comfy-table's native multiline cell handling
does the rest, under every `TableStyle` (ascii/unicode/compact/plain) and with
the TTY-only `Dynamic` content arrangement.

### 3. Vertical `\G`: no further change

`render_vertical` already prints `sanitized_display(value)` verbatim; with
`\n` surviving, continuation lines appear raw at column 0.

### 4. `md`: escape line breaks as `<br>`

`render_markdown` / `new_table`'s unsanitized path gains a md-specific cell
transform: replace `\r\n` → `<br>`, then `\n` → `<br>`, then `\r` → `<br>`,
applied to both cells and headers. (Order matters: `\r\n` first so it becomes
one `<br>`, not two.) `md` remains otherwise unsanitized — it is a fixed
interchange format and must stay terminal-independent; `<br>` is dialect
syntax, not terminal sanitization.

### Out of scope

- json/csv/tsv: untouched. JSON escapes `\n` in string literals; the csv
  writer quotes multiline fields. Both already correct.
- No new settings, flags, or dot-commands (YAGNI — nothing wants the blob
  back).
- No truncation/paging of tall table rows.

## Invariants this feature depends on

- **Interchange byte-identity:** json/csv/tsv output must not change by a
  single byte. Guarded by existing tests (`json_roundtrips`,
  `csv_preserves_trailing_whitespace_in_last_cell`, streaming-equality tests).
- **comfy-table multiline cells:** `Table` cells containing `\n` render as
  multiple lines inside the cell across all presets used here. Pinned by new
  snapshot tests (below), not assumed.
- **Sanitizer still neutralizes escape vectors:** ESC, lone `\r`, and other
  C0/C1 controls still become U+FFFD. Pinned by the existing B3
  characterization test, which is *kept and extended*, not deleted.

## Test plan

Inverted (deliberate contract change):

- `sanitize_for_terminal_neutralizes_newline` → becomes
  `sanitize_for_terminal_preserves_newline`: `\n` survives, no U+FFFD
  introduced for it.

Kept:

- `sanitize_for_terminal_neutralizes_control_bytes_but_keeps_tab` — ESC and
  lone `\r` still neutralized, `\t` still survives.

New:

- Unit: `"a\r\nb"` sanitizes to `"a\nb"` (CRLF collapses, no U+FFFD); lone
  `\r` still becomes U+FFFD.
- Snapshot: table format with a multiline cell (ascii style) — lines render
  inside the cell.
- Snapshot: vertical `\G` with a multiline value — raw continuation lines.
- Unit: md cell containing `"a\r\nb\nc\rd"` renders `a<br>b<br>c<br>d`; output
  has each table row on one physical line.
- Regression: a cell containing ESC in table format still shows U+FFFD (the
  security fix survives the newline exemption).
