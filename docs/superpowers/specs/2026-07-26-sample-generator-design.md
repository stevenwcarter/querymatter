# querymatter-samples: deterministic sample-vault generator + sample-query walkthrough

**Date:** 2026-07-26
**Status:** Approved (via /ship-it --ask; approach 1 selected)

## 1. Purpose

Give users (and ourselves) a familiar, regenerable playground:

- A **second bin target**, `querymatter-samples`, that generates a sample
  directory tree of Markdown files with YAML frontmatter, sized `1k`/`10k`/`100k`
  files for testing at various scales.
- A fixed, scale-independent **`starwars/` folder** modeled on the canonical
  GraphQL/Juniper star-wars data set, so users see familiar data.
- **Committed sample queries** — `docs/sample-queries.md` (walkthrough grouped
  by DSL capability, with expected output) and `docs/sample-queries.sql`
  (runnable via batch mode) — exercising most of the tool's query capabilities.
- **Determinism:** wiping the directory and regenerating from the same build
  produces byte-identical files *and* identical mtimes. (Cross-version stability
  is explicitly NOT required.)

The sample tree itself is **never committed** (`samples/` is already gitignored;
docs suggest `samples/` as the conventional target).

## 2. CLI

```
querymatter-samples [--scale <1k|10k|100k>] [--force] <DIR>
```

- `<DIR>` — required positional output directory.
  - Missing → created (including parents).
  - Exists but empty → used.
  - Exists non-empty → **error** naming `--force`, unless `--force` is given, in
    which case `DIR` is deleted entirely and regenerated.
- `--scale` — value enum `1k` (default), `10k`, `100k`. The scale is the **exact
  total file count** of the generated tree (1000 / 10000 / 100000 files),
  including the fixed `starwars/` files. `SELECT count(*)` over the tree returns
  exactly the scale number — a crisp, documentable property.
- clap derive, `#[command(version, about)]`, matching the main binary's style.
- All human output (progress summary, errors) goes to **stderr**, matching the
  repo's "stdout carries data" convention; stdout stays empty. Exit 0 on
  success, 1 on error (via `anyhow`).

Implemented as `src/bin/querymatter-samples/` (directory-style bin target with
`main.rs` + private modules). `cargo install --path .` installs it alongside
`querymatter`. **Zero new dependencies.**

## 3. Tree layout

```
<dir>/
  starwars/            # fixed 35 files at every scale
    characters/        # 20 — Luke, Vader, Han, Leia, Tarkin, C-3PO, R2-D2, … 
    starships/         # 8  — Millennium Falcon, X-wing, TIE fighter, …
    planets/           # 7  — Tatooine, Alderaan, Hoth, Dagobah, Bespin, Endor, Yavin IV
  work/                # ~50% of the remainder (mixed-theme contrived data)
    plans/  prs/  qa/
  recipes/             # ~30% of the remainder
    <cuisine>/
  reading/             # ~20% of the remainder
    <year>/
```

The remainder (`scale − 35`) is split 50/30/20 across work/recipes/reading with
deterministic integer arithmetic (leftover files go to `work/`); within each
theme, files spread across subdirectories deterministically so no directory
holds tens of thousands of entries at 100k. Only `.md` files are generated —
the tree is pure data (queries live in committed docs, not in the tree).

## 4. Data design

Every value below is chosen so the sample queries can demonstrate a specific
DSL capability. Field-type coverage across the tree: strings, integers, floats
(written as fixed literal strings from const tables, never formatted from
computed `f64`), ISO dates, RFC3339 datetimes, a deliberately non-ISO date
string (for `DATE(x, fmt)`), lists (for `MEMBER OF`), nested maps (for dotted
paths), and optional/absent fields (for `IS NULL` / `COALESCE`).

### 4.1 `starwars/` (hand-authored const tables, fixed content)

- **characters/** (20): `name`, `kind` (`human`/`droid`/`wookiee`/`hutt`/…),
  `episodes` (list drawn from `NEWHOPE`/`EMPIRE`/`JEDI` — the Juniper trio),
  `friends` (list of character names), `home_planet`, `height_cm` (int),
  `mass_kg` (int, absent for some — `IS NULL` demo), `primary_function`
  (droids only — the classic GraphQL field), `affiliation`. Body: a short
  descriptive paragraph.
- **starships/** (8): `name`, `model`, `manufacturer`, `crew` (int),
  `hyperdrive_rating` (float literal, e.g. `0.5`), `pilots` (list),
  `episodes` (list).
- **planets/** (7): `name`, `climate`, `terrain`, `population` (large int),
  `residents` (list).

The canonical seven (Luke Skywalker, Darth Vader, Han Solo, Leia Organa,
Wilhuff Tarkin, C-3PO, R2-D2) carry the exact episode/friend relationships
from the GraphQL sample; the rest of the cast (Obi-Wan, Yoda, Chewbacca,
Lando, Palpatine, Boba Fett, …) fills the folder out in the same spirit.
Because this folder is fixed at every scale, its query outputs are pinned
exactly in the docs.

### 4.2 `work/` — work-doc hub theme

Files `DCP-<n>-<slug>.md` under `plans/`, `prs/`, `qa/`. Frontmatter: `jira`
(`DCP-<n>`), `status` (weighted: `draft`/`in-review`/`synced`/`done`/`blocked`),
`prd` (3-digit string), `epic` (sometimes absent — `COALESCE` demo), `tags`
(list from a pool: `mobile`, `web`, `api`, `infra`, `ux`, `docs`, …), `lead`
(a name), `reviewers` (list of names — enables `WHERE lead MEMBER OF(reviewers)`,
the column-on-the-left form), `estimate` (nested map `{low, high}` — dotted-path
demo), `priority` (int 1–5), `created` (ISO date), `updated` (RFC3339
datetime), `due` (ISO date, sometimes absent). Body: generated sentences with
occasional `TODO`/`FIXME` markers (for `file.body REGEXP`) and varying lengths
(for `file.word_count`).

### 4.3 `recipes/` — recipe-box theme

Files `recipes/<cuisine>/<slug>.md`. Frontmatter: `title`, `cuisine`,
`servings` (int), `prep_minutes` / `cook_minutes` (ints — arithmetic demo:
`prep_minutes + cook_minutes AS total`), `rating` (1–5, sometimes absent),
`ingredients` (list), `tags` (list: `vegetarian`, `spicy`, `quick`, …),
`added` (ISO date), `source` (string, sometimes absent). Body: numbered steps.

### 4.4 `reading/` — reading-log theme

Files `reading/<year>/<slug>.md`. Frontmatter: `title`, `author`, `status`
(`queued`/`reading`/`finished`/`abandoned`), `rating` (only when finished),
`pages` (int), `genres` (list), `series` (occasional nested map
`{name, book}` — second dotted-path demo), `started` (ISO date, sometimes
absent), `finished` (ISO date, only when finished), `purchased` (US-format
`MM/DD/YYYY` string — the `DATE(x, '%m/%d/%Y')` demo). Body: reading notes.

## 5. Determinism strategy

- **PRNG:** an embedded SplitMix64 (~15 lines, no `rand` dep — sidesteps
  `rand`'s documented cross-version instability and keeps output stable
  effectively forever unless our own tables change). Each file's stream is
  seeded as `mix(GLOBAL_SEED, fnv1a(relative_path))`, so a file's content
  depends only on its path, not on generation order or total count.
- **No nondeterministic iteration:** const arrays and explicit sorts only;
  never iterate a `HashMap`/`HashSet`.
- **Dates:** fixed absolute dates, deterministically picked inside a fixed
  window (2025-01-01 .. 2026-07-01) for `work/` and `recipes/`; `reading/`
  files pick dates inside their own folder year (2019–2026) so the
  by-year layout stays coherent. Never derived from the clock.
- **mtimes:** after writing each file, set its modification time with
  `std::fs::File::set_modified` (std, stable) to a deterministic per-file
  `SystemTime` (`UNIX_EPOCH + secs`). Themed files derive it from their own
  primary frontmatter date (work: `updated`; recipes: `added`; reading:
  `finished`, else `started`, else a fixed fallback) at a fixed UTC clock
  time; every `starwars/` file uses the fixed constant
  `1977-05-25T00:00:00Z`. `file.mtime` / `file.size` query results are
  therefore fully pinned.
- **YAML emission:** frontmatter is emitted by hand-written formatting —
  keys in fixed authored order, values from controlled pools, quoting only
  where a value demands it. No YAML-emitting dependency.
- **Bytes:** `\n` line endings always; ASCII filenames; floats only ever
  written from literal strings in const tables.
- The only intentional non-determinism in the *demos* (not the data): sample
  queries using relative-date literals (`'-7d'`, `'today'`) resolve against
  the wall clock at query time; the docs flag those as time-dependent and do
  not pin their output.

## 6. Sample queries

### 6.1 `docs/sample-queries.md` (committed walkthrough)

Sections grouped by capability; each shows the query, one sentence of intent,
and expected output. `starwars/`-only queries pin exact output (fixed data);
scaled-data queries state they assume `--scale 1k`. Capability checklist the
doc must cover:

1. Basic `SELECT` + `WHERE` comparisons; string-vs-numeric literal semantics
2. `SELECT *`; `SELECT DISTINCT`
3. `file.*` pseudo-columns incl. `file.word_count`, `file.mtime`, `file.body`
4. Nested dotted paths (`estimate.low`, `series.name`)
5. Lists + `MEMBER OF` (literal-on-left and column-on-left `lead MEMBER OF(reviewers)`)
6. `LIKE`/`NOT LIKE`; `REGEXP` on a computed expression
7. `IN`/`NOT IN`; `IS NULL`/`IS NOT NULL`
8. Scalar functions (`lower`, `upper`, `length`, `trim`, `substr`, `replace`),
   `||` concat, arithmetic `+ - * / %`
9. `COALESCE`
10. `CASE` (searched and simple; in `SELECT` and `ORDER BY`)
11. All aggregates: `count(*)`, `count(col)`, `count(distinct col)`, `min`,
    `max`, `sum`, `avg`, `group_concat`
12. `GROUP BY` (incl. alias key) + `HAVING` (aggregate and key leaves, alias)
13. `ORDER BY` column/alias/bare-aggregate/computed expression, incl. the
    parenthesized-scalar-function workaround `ORDER BY (upper(x))`
14. `LIMIT n OFFSET m`
15. `FROM 'glob'` subtree filtering (`FROM 'starwars/characters/**'`)
16. Dates: auto-detected ISO comparison, `DATE(x)`, `DATE(x, '%m/%d/%Y')`,
    relative-date literals (flagged time-dependent)
17. Total-count sanity check: `SELECT count(*)` = the scale number

Prose-only callouts (not in the runnable script): `\G` vertical output,
`--format json|csv`, unknown-column did-you-mean (it errors by design),
`--exit-code`, and a "testing at scale" section — generate `--scale 100k`,
time a query, then `querymatter init` the tree and compare.

### 6.2 `docs/sample-queries.sql` (committed, runnable)

The runnable subset of the walkthrough, executed as:

```sh
querymatter-samples samples
querymatter samples < docs/sample-queries.sql
```

Format constraints imposed by the batch-mode statement splitter (which splits
on top-level `;` and is quote-aware but *not* comment-aware, while sqlparser
skips `--` comments inside a statement's text):

- `--` comment lines are allowed, but **no `;` inside any comment**, and the
  file must **end with a statement**, never a trailing comment after the final
  `;` (a comment-only chunk would reach the parser alone and error).
- Every statement must succeed against a `--scale 1k` tree.

## 7. Testing

`tests/samples.rs` (integration, real binaries via `assert_cmd`), plus unit
tests inside the bin's modules:

1. **Determinism (the headline guarantee):** generate `--scale 1k` into two
   tempdirs; recursively compare — identical relative path sets, identical
   bytes, identical mtimes. This is the load-bearing test for "wipe and
   regenerate ⇒ same results"; it must not be waived by appeal to the design.
2. **Exact counts:** the 1k tree holds exactly 1000 files, 35 of them under
   `starwars/`.
3. **Runnable queries stay runnable:** generate a 1k tree, pipe the real
   committed `docs/sample-queries.sql` through the real `querymatter` binary
   in batch mode, assert exit 0 and **insta-snapshot the full stdout**. This
   pins every pinned output in the docs and fails if either the generator's
   data or the DSL's behavior drifts — sample queries can never silently rot.
4. **`--force` semantics:** non-empty dir without `--force` errors (and leaves
   the dir untouched); with `--force` it wipes and regenerates.
5. **Unit tests:** PRNG stream pinned (first few outputs of a known seed);
   remainder-split arithmetic sums to `scale − 35` for all three scales;
   star-wars const-table invariants (20/8/7 entries, canonical seven present).

10k/100k generation is not exercised in the default test run (kept fast); the
count-split arithmetic for those scales is unit-tested instead.

## 8. Documentation updates

- `README.md`: a "Sample data & sample queries" section — how to build/run
  `querymatter-samples`, pointer to `docs/sample-queries.md`, the exact-count
  property, and the 100k + `init` scale-testing suggestion.
- `TODO.md`: check off the sample-generator item.
- `justfile`: a `samples` recipe (`cargo run --bin querymatter-samples -- --force --scale 1k samples`).

## 9. Invariants this feature depends on

Enumerated so a later change touching these funnels can grep for dependents;
each is pinned by test 3 (the `.sql` snapshot run) unless noted:

- Batch mode splits piped stdin on top-level `;` and passes chunks to
  sqlparser, which tolerates leading/embedded `--` comments (§6.2).
- Default discovery includes `.md` files and needs no flags for a plain
  directory tree (no `.querymatter` vault present in tempdir tests).
- `file.mtime` renders as ISO-8601 UTC seconds from the file's stat — pinned
  mtimes ⇒ pinned query output.
- `samples/` stays gitignored (pinned by nothing mechanical; noted in README
  next to the suggested target dir).
- Determinism additionally depends on `File::set_modified` second-or-better
  granularity on the target filesystem; the determinism test (test 1)
  compares mtimes and would surface a filesystem that can't represent them.

## 10. Out of scope

- Cross-version output stability, migrations, or version-stamping the tree.
- Committing any generated data.
- Windows-specific path/mtime handling beyond what std provides.
- Benchmark automation over the 100k tree (docs describe the manual flow).
