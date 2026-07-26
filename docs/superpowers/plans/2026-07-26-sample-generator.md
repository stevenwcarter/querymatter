# querymatter-samples Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A second bin target `querymatter-samples` that deterministically generates a sample Markdown/frontmatter tree (fixed `starwars/` + scaled `work/`/`recipes/`/`reading/` themes, exactly 1k/10k/100k files), plus committed sample-query docs (`docs/sample-queries.md` + runnable `docs/sample-queries.sql`) pinned by integration tests.

**Architecture:** Directory-style bin target `src/bin/querymatter-samples/` (main.rs + rng.rs + write.rs + data.rs + starwars.rs + themes.rs), sharing the crate's existing deps only. Determinism via an embedded SplitMix64 PRNG (per-file streams keyed off path/index hashes), fixed absolute dates, and `File::set_modified` mtimes. The main `querymatter` binary is untouched.

**Tech Stack:** Rust edition 2024; clap (derive), anyhow, chrono (already deps); assert_cmd + tempfile + insta for tests. **Zero new dependencies.**

**Spec:** `docs/superpowers/specs/2026-07-26-sample-generator-design.md`

## Global Constraints

- Edition 2024; run `cargo fmt --all` before every commit (repo has NO pre-commit hook); code must be `cargo clippy --all-targets` clean.
- Binary-only crate: no `cargo test --lib`; unit tests live in the bin's modules (`cargo test --bin querymatter-samples`), integration tests in `tests/` run the real binaries via `assert_cmd`.
- **Zero new dependencies** (spec §2). `Cargo.lock` must not change.
- All human output from `querymatter-samples` goes to **stderr**; stdout stays empty (spec §2).
- Determinism (spec §5): no clock reads, no `HashMap` iteration, `\n` line endings, ASCII filenames, floats only from const literal strings.
- Exact totals: the tree holds exactly 1000/10000/100000 files, 35 of them under `starwars/` (spec §2, §3).
- Commit messages end with:
  `Claude-Session: https://claude.ai/code/session_01BsfkatoCFtfkZbXmdDkBnH`

---

### Task 1: PRNG module (`rng.rs`)

**Files:**
- Create: `src/bin/querymatter-samples/main.rs` (minimal shell so the bin compiles)
- Create: `src/bin/querymatter-samples/rng.rs`

**Interfaces:**
- Produces (later tasks consume exactly these):
  - `pub const GLOBAL_SEED: u64 = 0x5EED_5A17_1E5A_3B1E;`
  - `pub struct SplitMix64 { ... }` with `pub fn new(seed: u64) -> Self`, `pub fn next_u64(&mut self) -> u64`, `pub fn range(&mut self, n: u64) -> u64` (uniform-ish `0..n`, panics on `n == 0`), `pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T`, `pub fn pick_k<'a, T>(&mut self, items: &'a [T], k: usize) -> Vec<&'a T>` (k distinct items, first-seen order, k clamped to `items.len()`), `pub fn chance(&mut self, pct: u64) -> bool`
  - `pub fn fnv1a(s: &str) -> u64`
  - `pub fn file_rng(rel_path: &str) -> SplitMix64` — `SplitMix64::new(GLOBAL_SEED ^ fnv1a(rel_path))`
  - `pub fn stream_rng(tag: &str, i: u64) -> SplitMix64` — `SplitMix64::new(GLOBAL_SEED ^ fnv1a(tag) ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15))`

- [ ] **Step 1: Create the bin shell** — `src/bin/querymatter-samples/main.rs`:

```rust
//! Deterministic sample-vault generator for querymatter.
//!
//! See docs/superpowers/specs/2026-07-26-sample-generator-design.md.

mod rng;

fn main() {}
```

(The `mod` list grows as later tasks add modules; `main()` gets its real body in Task 6.)

- [ ] **Step 2: Write failing unit tests** in `src/bin/querymatter-samples/rng.rs` (tests first, module skeleton with `todo!()` bodies so it compiles):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Known SplitMix64 test vectors for seed 0 — pins the algorithm itself,
    /// which is what makes regeneration stable across builds.
    #[test]
    fn splitmix64_known_vectors() {
        let mut r = SplitMix64::new(0);
        assert_eq!(r.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(r.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(r.next_u64(), 0x06C4_5D18_8009_454F);
    }

    #[test]
    fn range_is_bounded_and_deterministic() {
        let mut r = SplitMix64::new(42);
        let vals: Vec<u64> = (0..100).map(|_| r.range(7)).collect();
        assert!(vals.iter().all(|v| *v < 7));
        let mut r2 = SplitMix64::new(42);
        let vals2: Vec<u64> = (0..100).map(|_| r2.range(7)).collect();
        assert_eq!(vals, vals2);
    }

    #[test]
    fn pick_k_returns_distinct_items() {
        let items = ["a", "b", "c", "d", "e"];
        let mut r = SplitMix64::new(7);
        let picked = r.pick_k(&items, 3);
        assert_eq!(picked.len(), 3);
        let mut sorted: Vec<_> = picked.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 3, "picks must be distinct");
    }

    #[test]
    fn pick_k_clamps_to_len() {
        let items = ["a", "b"];
        let mut r = SplitMix64::new(7);
        assert_eq!(r.pick_k(&items, 10).len(), 2);
    }

    #[test]
    fn fnv1a_known_values() {
        // FNV-1a 64-bit: empty string hashes to the offset basis.
        assert_eq!(fnv1a(""), 0xCBF2_9CE4_8422_2325);
        assert_ne!(fnv1a("a"), fnv1a("b"));
    }

    #[test]
    fn file_rng_depends_only_on_path() {
        let a1: Vec<u64> = { let mut r = file_rng("work/plans/DCP-100-x.md"); (0..5).map(|_| r.next_u64()).collect() };
        let a2: Vec<u64> = { let mut r = file_rng("work/plans/DCP-100-x.md"); (0..5).map(|_| r.next_u64()).collect() };
        let b: Vec<u64> = { let mut r = file_rng("work/plans/DCP-101-x.md"); (0..5).map(|_| r.next_u64()).collect() };
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
    }
}
```

- [ ] **Step 3: Run tests, verify they fail** — `cargo test --bin querymatter-samples` → panics on `todo!()`.

- [ ] **Step 4: Implement**:

```rust
//! Embedded deterministic PRNG (SplitMix64) — no `rand` dependency, so the
//! output stream can never shift under a dependency upgrade.

pub const GLOBAL_SEED: u64 = 0x5EED_5A17_1E5A_3B1E;

pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform-ish value in `0..n` (modulo bias is irrelevant for sample data).
    pub fn range(&mut self, n: u64) -> u64 {
        assert!(n > 0, "range(0) is meaningless");
        self.next_u64() % n
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.range(items.len() as u64) as usize]
    }

    /// `k` distinct items in first-seen order (partial Fisher–Yates over indices).
    pub fn pick_k<'a, T>(&mut self, items: &'a [T], k: usize) -> Vec<&'a T> {
        let k = k.min(items.len());
        let mut idx: Vec<usize> = (0..items.len()).collect();
        for i in 0..k {
            let j = i + self.range((idx.len() - i) as u64) as usize;
            idx.swap(i, j);
        }
        idx[..k].iter().map(|&i| &items[i]).collect()
    }

    /// True `pct`% of the time.
    pub fn chance(&mut self, pct: u64) -> bool {
        self.range(100) < pct
    }
}

pub fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

/// Content stream for one file, keyed off its tree-relative path.
pub fn file_rng(rel_path: &str) -> SplitMix64 {
    SplitMix64::new(GLOBAL_SEED ^ fnv1a(rel_path))
}

/// Naming stream for the i-th file of a theme (paths need names before they exist).
pub fn stream_rng(tag: &str, i: u64) -> SplitMix64 {
    SplitMix64::new(GLOBAL_SEED ^ fnv1a(tag) ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}
```

- [ ] **Step 5: Run tests, verify pass** — `cargo test --bin querymatter-samples`. If the SplitMix64 vectors fail, the algorithm is wrong — fix the implementation, never the vectors (they are the published reference values).

- [ ] **Step 6: fmt, clippy, commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings
git add src/bin/querymatter-samples/ && git commit -m "feat(samples): querymatter-samples bin shell + SplitMix64 PRNG"
```

---

### Task 2: File writer + date helpers (`write.rs`)

**Files:**
- Create: `src/bin/querymatter-samples/write.rs`
- Modify: `src/bin/querymatter-samples/main.rs` (add `mod write;`)

**Interfaces:**
- Consumes: `rng::SplitMix64` (Task 1)
- Produces:
  - `pub fn write_md(root: &Path, rel: &str, frontmatter: &str, body: &str, mtime: SystemTime) -> anyhow::Result<()>` — writes `---\n{frontmatter}---\n\n{body}` (caller guarantees `frontmatter` and `body` each end with `\n`), creating parent dirs, then sets the file's mtime.
  - `pub fn day_in_window(rng: &mut SplitMix64) -> NaiveDate` — 2025-01-01 plus `range(546)` days (window end 2026-06-30, spec §5).
  - `pub fn day_in_year(rng: &mut SplitMix64, year: i32) -> NaiveDate` — Jan 1 of `year` plus `range(365)` days (never Dec 32; leap day ignored).
  - `pub fn mtime_at(date: NaiveDate, h: u32, m: u32, s: u32) -> SystemTime` — `UNIX_EPOCH + date-at-h:m:s-UTC` in whole seconds.
  - `pub fn rfc3339_at(date: NaiveDate, h: u32, m: u32, s: u32) -> String` — `YYYY-MM-DDTHH:MM:SSZ`.
  - `pub fn slugify(s: &str) -> String` — lowercase; alphanumerics kept; every other char → `-`; runs collapsed; no leading/trailing `-`.

- [ ] **Step 1: Write failing unit tests** in `write.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn slugify_basics() {
        assert_eq!(slugify("C-3PO"), "c-3po");
        assert_eq!(slugify("Millennium Falcon"), "millennium-falcon");
        assert_eq!(slugify("Spicy  Chickpea!! Curry"), "spicy-chickpea-curry");
    }

    #[test]
    fn rfc3339_formats_utc_seconds() {
        let d = chrono::NaiveDate::from_ymd_opt(2026, 3, 14).unwrap();
        assert_eq!(rfc3339_at(d, 9, 30, 5), "2026-03-14T09:30:05Z");
    }

    #[test]
    fn mtime_at_is_epoch_seconds() {
        let d = chrono::NaiveDate::from_ymd_opt(1970, 1, 2).unwrap();
        assert_eq!(mtime_at(d, 0, 0, 0), UNIX_EPOCH + std::time::Duration::from_secs(86_400));
    }

    #[test]
    fn day_windows_stay_in_bounds() {
        let mut r = crate::rng::SplitMix64::new(1);
        for _ in 0..500 {
            let d = day_in_window(&mut r);
            assert!(d >= chrono::NaiveDate::from_ymd_opt(2025, 1, 1).unwrap());
            assert!(d <= chrono::NaiveDate::from_ymd_opt(2026, 6, 30).unwrap());
            let y = day_in_year(&mut r, 2021);
            assert_eq!(y.year(), 2021);
        }
    }

    #[test]
    fn write_md_writes_exact_bytes_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let mtime = mtime_at(chrono::NaiveDate::from_ymd_opt(1977, 5, 25).unwrap(), 0, 0, 0);
        write_md(dir.path(), "a/b/test.md", "name: Luke\n", "Body.\n", mtime).unwrap();
        let p = dir.path().join("a/b/test.md");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "---\nname: Luke\n---\n\nBody.\n"
        );
        assert_eq!(std::fs::metadata(&p).unwrap().modified().unwrap(), mtime);
    }
}
```

(`use chrono::Datelike;` where needed for `.year()`.)

- [ ] **Step 2: Run tests, verify fail** — `cargo test --bin querymatter-samples`.

- [ ] **Step 3: Implement**:

```rust
//! File emission: exact bytes, deterministic mtimes.

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use chrono::NaiveDate;

use crate::rng::SplitMix64;

pub fn write_md(
    root: &Path,
    rel: &str,
    frontmatter: &str,
    body: &str,
    mtime: SystemTime,
) -> anyhow::Result<()> {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut f = fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    write!(f, "---\n{frontmatter}---\n\n{body}")?;
    f.flush()?;
    f.set_modified(mtime)
        .with_context(|| format!("setting mtime on {}", path.display()))?;
    Ok(())
}

/// 2025-01-01 ..= 2026-06-30 — the spec §5 window (546 days).
pub fn day_in_window(rng: &mut SplitMix64) -> NaiveDate {
    NaiveDate::from_ymd_opt(2025, 1, 1).unwrap() + chrono::Days::new(rng.range(546))
}

/// A day inside `year` (offset 0..365 from Jan 1 — leap day unused, harmless).
pub fn day_in_year(rng: &mut SplitMix64, year: i32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, 1, 1).unwrap() + chrono::Days::new(rng.range(365))
}

pub fn mtime_at(date: NaiveDate, h: u32, m: u32, s: u32) -> SystemTime {
    let secs = date
        .and_hms_opt(h, m, s)
        .unwrap()
        .and_utc()
        .timestamp();
    UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).expect("dates are post-epoch"))
}

pub fn rfc3339_at(date: NaiveDate, h: u32, m: u32, s: u32) -> String {
    format!("{}T{h:02}:{m:02}:{s:02}Z", date.format("%Y-%m-%d"))
}

pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    out
}
```

- [ ] **Step 4: Run tests, verify pass.**

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings
git add src/bin/querymatter-samples/ && git commit -m "feat(samples): file writer with deterministic mtimes + date/slug helpers"
```

---

### Task 3: Star-wars data + generator (`data.rs`, `starwars.rs`)

**Files:**
- Create: `src/bin/querymatter-samples/data.rs` (star-wars tables only; theme pools arrive in Tasks 4–5)
- Create: `src/bin/querymatter-samples/starwars.rs`
- Modify: `src/bin/querymatter-samples/main.rs` (add `mod data; mod starwars;`)

**Interfaces:**
- Consumes: `write::{write_md, mtime_at, slugify}` (Task 2)
- Produces:
  - `data::Character { name, kind, episodes, friends, home_planet, height_cm, mass_kg: Option<u32>, primary_function: Option<&'static str>, affiliation }`, `data::Starship { name, model, manufacturer, crew: u32, hyperdrive_rating: &'static str, pilots, episodes }`, `data::Planet { name, climate, terrain, population: Option<u64>, residents }`
  - `data::CHARACTERS: [Character; 20]`, `data::STARSHIPS: [Starship; 8]`, `data::PLANETS: [Planet; 7]`
  - `starwars::FILE_COUNT: u64 = 35`
  - `starwars::generate(root: &Path) -> anyhow::Result<()>` — writes all 35 files under `root/starwars/`.

- [ ] **Step 1: Author the const tables** in `data.rs`. Exact content (the canonical GraphQL seven carry the sample's episode/friend relationships; `kind` values: `human`, `droid`, `wookiee`, `hutt`, `other`):

```rust
//! Hand-authored sample data tables. Star-wars entities are fixed at every
//! scale; edits here intentionally change generated output (and snapshots).

pub struct Character {
    pub name: &'static str,
    pub kind: &'static str,
    pub episodes: &'static [&'static str],
    pub friends: &'static [&'static str],
    pub home_planet: &'static str,
    pub height_cm: u32,
    pub mass_kg: Option<u32>,
    pub primary_function: Option<&'static str>,
    pub affiliation: &'static str,
}

pub struct Starship {
    pub name: &'static str,
    pub model: &'static str,
    pub manufacturer: &'static str,
    pub crew: u32,
    pub hyperdrive_rating: &'static str, // written verbatim — float determinism
    pub pilots: &'static [&'static str],
    pub episodes: &'static [&'static str],
}

pub struct Planet {
    pub name: &'static str,
    pub climate: &'static str,
    pub terrain: &'static str,
    pub population: Option<u64>,
    pub residents: &'static [&'static str],
}

const TRILOGY: &[&str] = &["NEWHOPE", "EMPIRE", "JEDI"];

pub const CHARACTERS: [Character; 20] = [
    Character { name: "Luke Skywalker", kind: "human", episodes: TRILOGY, friends: &["Han Solo", "Leia Organa", "C-3PO", "R2-D2"], home_planet: "Tatooine", height_cm: 172, mass_kg: Some(77), primary_function: None, affiliation: "Rebel Alliance" },
    Character { name: "Darth Vader", kind: "human", episodes: TRILOGY, friends: &["Wilhuff Tarkin"], home_planet: "Tatooine", height_cm: 202, mass_kg: Some(136), primary_function: None, affiliation: "Galactic Empire" },
    Character { name: "Han Solo", kind: "human", episodes: TRILOGY, friends: &["Luke Skywalker", "Leia Organa", "R2-D2", "Chewbacca"], home_planet: "Corellia", height_cm: 180, mass_kg: Some(80), primary_function: None, affiliation: "Rebel Alliance" },
    Character { name: "Leia Organa", kind: "human", episodes: TRILOGY, friends: &["Luke Skywalker", "Han Solo", "C-3PO", "R2-D2"], home_planet: "Alderaan", height_cm: 150, mass_kg: Some(49), primary_function: None, affiliation: "Rebel Alliance" },
    Character { name: "Wilhuff Tarkin", kind: "human", episodes: &["NEWHOPE"], friends: &["Darth Vader"], home_planet: "Eriadu", height_cm: 180, mass_kg: None, primary_function: None, affiliation: "Galactic Empire" },
    Character { name: "C-3PO", kind: "droid", episodes: TRILOGY, friends: &["Luke Skywalker", "Han Solo", "Leia Organa", "R2-D2"], home_planet: "Tatooine", height_cm: 167, mass_kg: Some(75), primary_function: Some("Protocol"), affiliation: "Rebel Alliance" },
    Character { name: "R2-D2", kind: "droid", episodes: TRILOGY, friends: &["Luke Skywalker", "Han Solo", "Leia Organa"], home_planet: "Naboo", height_cm: 96, mass_kg: Some(32), primary_function: Some("Astromech"), affiliation: "Rebel Alliance" },
    Character { name: "Obi-Wan Kenobi", kind: "human", episodes: TRILOGY, friends: &["Luke Skywalker", "Yoda"], home_planet: "Stewjon", height_cm: 182, mass_kg: Some(77), primary_function: None, affiliation: "Jedi Order" },
    Character { name: "Yoda", kind: "other", episodes: &["EMPIRE", "JEDI"], friends: &["Obi-Wan Kenobi", "Luke Skywalker"], home_planet: "Dagobah", height_cm: 66, mass_kg: Some(17), primary_function: None, affiliation: "Jedi Order" },
    Character { name: "Chewbacca", kind: "wookiee", episodes: TRILOGY, friends: &["Han Solo", "Luke Skywalker", "Leia Organa"], home_planet: "Kashyyyk", height_cm: 228, mass_kg: Some(112), primary_function: None, affiliation: "Rebel Alliance" },
    Character { name: "Lando Calrissian", kind: "human", episodes: &["EMPIRE", "JEDI"], friends: &["Han Solo", "Chewbacca"], home_planet: "Socorro", height_cm: 177, mass_kg: Some(79), primary_function: None, affiliation: "Rebel Alliance" },
    Character { name: "Emperor Palpatine", kind: "human", episodes: &["EMPIRE", "JEDI"], friends: &["Darth Vader"], home_planet: "Naboo", height_cm: 170, mass_kg: Some(75), primary_function: None, affiliation: "Galactic Empire" },
    Character { name: "Boba Fett", kind: "human", episodes: &["EMPIRE", "JEDI"], friends: &[], home_planet: "Kamino", height_cm: 183, mass_kg: Some(78), primary_function: None, affiliation: "Bounty Hunters Guild" },
    Character { name: "Jabba the Hutt", kind: "hutt", episodes: &["NEWHOPE", "JEDI"], friends: &["Boba Fett"], home_planet: "Nal Hutta", height_cm: 175, mass_kg: Some(1358), primary_function: None, affiliation: "Hutt Cartel" },
    Character { name: "Wedge Antilles", kind: "human", episodes: TRILOGY, friends: &["Luke Skywalker"], home_planet: "Corellia", height_cm: 170, mass_kg: Some(77), primary_function: None, affiliation: "Rebel Alliance" },
    Character { name: "Admiral Ackbar", kind: "other", episodes: &["JEDI"], friends: &["Mon Mothma"], home_planet: "Mon Cala", height_cm: 180, mass_kg: Some(83), primary_function: None, affiliation: "Rebel Alliance" },
    Character { name: "Mon Mothma", kind: "human", episodes: &["JEDI"], friends: &["Admiral Ackbar", "Leia Organa"], home_planet: "Chandrila", height_cm: 150, mass_kg: None, primary_function: None, affiliation: "Rebel Alliance" },
    Character { name: "Greedo", kind: "other", episodes: &["NEWHOPE"], friends: &[], home_planet: "Rodia", height_cm: 173, mass_kg: Some(74), primary_function: None, affiliation: "Hutt Cartel" },
    Character { name: "Lobot", kind: "human", episodes: &["EMPIRE"], friends: &["Lando Calrissian"], home_planet: "Bespin", height_cm: 175, mass_kg: Some(79), primary_function: None, affiliation: "Cloud City" },
    Character { name: "IG-88", kind: "droid", episodes: &["EMPIRE"], friends: &[], home_planet: "Holowan", height_cm: 200, mass_kg: Some(140), primary_function: Some("Assassin"), affiliation: "Bounty Hunters Guild" },
];

pub const STARSHIPS: [Starship; 8] = [
    Starship { name: "Millennium Falcon", model: "YT-1300 light freighter", manufacturer: "Corellian Engineering Corporation", crew: 4, hyperdrive_rating: "0.5", pilots: &["Han Solo", "Chewbacca", "Lando Calrissian"], episodes: TRILOGY },
    Starship { name: "X-wing", model: "T-65 X-wing", manufacturer: "Incom Corporation", crew: 1, hyperdrive_rating: "1.0", pilots: &["Luke Skywalker", "Wedge Antilles"], episodes: TRILOGY },
    Starship { name: "TIE Advanced x1", model: "Twin Ion Engine Advanced x1", manufacturer: "Sienar Fleet Systems", crew: 1, hyperdrive_rating: "1.0", pilots: &["Darth Vader"], episodes: &["NEWHOPE"] },
    Starship { name: "Imperial Star Destroyer", model: "Imperial I-class Star Destroyer", manufacturer: "Kuat Drive Yards", crew: 47060, hyperdrive_rating: "2.0", pilots: &[], episodes: TRILOGY },
    Starship { name: "Slave I", model: "Firespray-31-class patrol craft", manufacturer: "Kuat Systems Engineering", crew: 1, hyperdrive_rating: "3.0", pilots: &["Boba Fett"], episodes: &["EMPIRE", "JEDI"] },
    Starship { name: "Y-wing", model: "BTL Y-wing", manufacturer: "Koensayr Manufacturing", crew: 2, hyperdrive_rating: "1.0", pilots: &[], episodes: &["NEWHOPE", "JEDI"] },
    Starship { name: "A-wing", model: "RZ-1 A-wing interceptor", manufacturer: "Alliance Underground Engineering", crew: 1, hyperdrive_rating: "1.0", pilots: &[], episodes: &["JEDI"] },
    Starship { name: "Executor", model: "Executor-class Star Dreadnought", manufacturer: "Kuat Drive Yards", crew: 279144, hyperdrive_rating: "2.0", pilots: &[], episodes: &["EMPIRE", "JEDI"] },
];

pub const PLANETS: [Planet; 7] = [
    Planet { name: "Tatooine", climate: "arid", terrain: "desert", population: Some(200000), residents: &["Luke Skywalker", "Darth Vader", "C-3PO"] },
    Planet { name: "Alderaan", climate: "temperate", terrain: "grasslands, mountains", population: Some(2000000000), residents: &["Leia Organa"] },
    Planet { name: "Hoth", climate: "frozen", terrain: "tundra, ice caves", population: None, residents: &[] },
    Planet { name: "Dagobah", climate: "murky", terrain: "swamp, jungles", population: None, residents: &["Yoda"] },
    Planet { name: "Bespin", climate: "temperate", terrain: "gas giant", population: Some(6000000), residents: &["Lando Calrissian", "Lobot"] },
    Planet { name: "Endor", climate: "temperate", terrain: "forests, mountains", population: Some(30000000), residents: &[] },
    Planet { name: "Yavin IV", climate: "temperate, tropical", terrain: "jungle, rainforests", population: Some(1000), residents: &[] },
];
```

- [ ] **Step 2: Write failing tests** in `starwars.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::data;

    #[test]
    fn table_sizes_are_locked() {
        assert_eq!(data::CHARACTERS.len(), 20);
        assert_eq!(data::STARSHIPS.len(), 8);
        assert_eq!(data::PLANETS.len(), 7);
        assert_eq!(FILE_COUNT, 35);
    }

    #[test]
    fn canonical_seven_present() {
        for name in ["Luke Skywalker", "Darth Vader", "Han Solo", "Leia Organa", "Wilhuff Tarkin", "C-3PO", "R2-D2"] {
            assert!(data::CHARACTERS.iter().any(|c| c.name == name), "missing {name}");
        }
    }

    #[test]
    fn generate_writes_35_files_with_fixed_mtime() {
        let dir = tempfile::tempdir().unwrap();
        generate(dir.path()).unwrap();
        let mut count = 0;
        let mut stack = vec![dir.path().join("starwars")];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).unwrap() {
                let e = entry.unwrap();
                if e.file_type().unwrap().is_dir() { stack.push(e.path()); } else { count += 1; }
            }
        }
        assert_eq!(count, 35);
        let luke = dir.path().join("starwars/characters/luke-skywalker.md");
        let expected_mtime = crate::write::mtime_at(chrono::NaiveDate::from_ymd_opt(1977, 5, 25).unwrap(), 0, 0, 0);
        assert_eq!(std::fs::metadata(&luke).unwrap().modified().unwrap(), expected_mtime);
    }

    /// Pins one full file byte-for-byte: frontmatter key order, list style,
    /// optional-field omission, body template.
    #[test]
    fn c3po_file_is_byte_exact() {
        let dir = tempfile::tempdir().unwrap();
        generate(dir.path()).unwrap();
        let got = std::fs::read_to_string(dir.path().join("starwars/characters/c-3po.md")).unwrap();
        let want = "---\n\
name: C-3PO\n\
kind: droid\n\
episodes: [NEWHOPE, EMPIRE, JEDI]\n\
friends: [Luke Skywalker, Han Solo, Leia Organa, R2-D2]\n\
home_planet: Tatooine\n\
height_cm: 167\n\
mass_kg: 75\n\
primary_function: Protocol\n\
affiliation: Rebel Alliance\n\
---\n\n\
C-3PO is a droid from Tatooine, affiliated with Rebel Alliance.\n\n\
Appears in: NEWHOPE, EMPIRE, JEDI.\n";
        assert_eq!(got, want);
    }
}
```

- [ ] **Step 3: Run tests, verify fail.**

- [ ] **Step 4: Implement `starwars.rs`:**

```rust
//! Fixed star-wars folder — identical at every scale (spec §4.1).

use std::fmt::Write as _;
use std::path::Path;

use chrono::NaiveDate;

use crate::data::{CHARACTERS, PLANETS, STARSHIPS};
use crate::write::{mtime_at, slugify, write_md};

pub const FILE_COUNT: u64 = 35;

/// A New Hope's release date — the fixed mtime for every starwars file (spec §5).
fn starwars_mtime() -> std::time::SystemTime {
    mtime_at(NaiveDate::from_ymd_opt(1977, 5, 25).unwrap(), 0, 0, 0)
}

fn yaml_list(items: &[&str]) -> String {
    format!("[{}]", items.join(", "))
}

pub fn generate(root: &Path) -> anyhow::Result<()> {
    let mtime = starwars_mtime();

    for c in &CHARACTERS {
        let mut fm = String::new();
        writeln!(fm, "name: {}", c.name)?;
        writeln!(fm, "kind: {}", c.kind)?;
        writeln!(fm, "episodes: {}", yaml_list(c.episodes))?;
        writeln!(fm, "friends: {}", yaml_list(c.friends))?;
        writeln!(fm, "home_planet: {}", c.home_planet)?;
        writeln!(fm, "height_cm: {}", c.height_cm)?;
        if let Some(m) = c.mass_kg {
            writeln!(fm, "mass_kg: {m}")?;
        }
        if let Some(f) = c.primary_function {
            writeln!(fm, "primary_function: {f}")?;
        }
        writeln!(fm, "affiliation: {}", c.affiliation)?;
        let body = format!(
            "{} is a {} from {}, affiliated with {}.\n\nAppears in: {}.\n",
            c.name, c.kind, c.home_planet, c.affiliation, c.episodes.join(", "),
        );
        let rel = format!("starwars/characters/{}.md", slugify(c.name));
        write_md(root, &rel, &fm, &body, mtime)?;
    }

    for s in &STARSHIPS {
        let mut fm = String::new();
        writeln!(fm, "name: {}", s.name)?;
        writeln!(fm, "model: {}", s.model)?;
        writeln!(fm, "manufacturer: {}", s.manufacturer)?;
        writeln!(fm, "crew: {}", s.crew)?;
        writeln!(fm, "hyperdrive_rating: {}", s.hyperdrive_rating)?;
        writeln!(fm, "pilots: {}", yaml_list(s.pilots))?;
        writeln!(fm, "episodes: {}", yaml_list(s.episodes))?;
        let body = format!("The {} is a {} built by {}.\n", s.name, s.model, s.manufacturer);
        let rel = format!("starwars/starships/{}.md", slugify(s.name));
        write_md(root, &rel, &fm, &body, mtime)?;
    }

    for p in &PLANETS {
        let mut fm = String::new();
        writeln!(fm, "name: {}", p.name)?;
        writeln!(fm, "climate: {}", p.climate)?;
        writeln!(fm, "terrain: {}", p.terrain)?;
        if let Some(pop) = p.population {
            writeln!(fm, "population: {pop}")?;
        }
        writeln!(fm, "residents: {}", yaml_list(p.residents))?;
        let body = format!("{} has a {} climate and {} terrain.\n", p.name, p.climate, p.terrain);
        let rel = format!("starwars/planets/{}.md", slugify(p.name));
        write_md(root, &rel, &fm, &body, mtime)?;
    }

    Ok(())
}
```

Note: `terrain: grasslands, mountains` is a plain YAML scalar containing a
comma — valid, parses as one string (it is not inside `[...]`).

- [ ] **Step 5: Run tests, verify pass.** If `c3po_file_is_byte_exact` disagrees, fix the *generator* to match the pinned bytes (the test is the contract), unless the generator is right and the test's `want` had a typo.

- [ ] **Step 6: fmt, clippy, commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings
git add src/bin/querymatter-samples/ && git commit -m "feat(samples): fixed star-wars folder (20 characters, 8 starships, 7 planets)"
```

---

### Task 4: Count split + work theme (`themes.rs`, pools in `data.rs`)

**Files:**
- Modify: `src/bin/querymatter-samples/data.rs` (append shared + work pools)
- Create: `src/bin/querymatter-samples/themes.rs`
- Modify: `src/bin/querymatter-samples/main.rs` (add `mod themes;`)

**Interfaces:**
- Consumes: `rng::{stream_rng, file_rng, SplitMix64}`, `write::{write_md, day_in_window, mtime_at, rfc3339_at}`, data pools below.
- Produces:
  - `themes::split_counts(remainder: u64) -> (u64, u64, u64)` — `(work, recipes, reading)`; `recipes = remainder * 30 / 100`, `reading = remainder * 20 / 100`, `work = remainder - recipes - reading` (spec §3: leftover goes to work).
  - `themes::generate_work(root: &Path, n: u64) -> anyhow::Result<()>`
  - `data::NAMES: [&str; 12]`, `data::WORK_TAGS: [&str; 8]`, `data::EPICS: [&str; 6]`, `data::SLUG_WORDS: [&str; 16]`, `data::SENTENCES: [&str; 10]`

- [ ] **Step 1: Append pools to `data.rs`:**

```rust
// ---- shared/theme pools (plain scalars only: no ':', '#', quotes) ----

pub const NAMES: [&str; 12] = [
    "Avery Chen", "Jordan Patel", "Sam Rivera", "Morgan Lee", "Casey Nguyen",
    "Riley Brooks", "Quinn Foster", "Alex Murphy", "Dana Kim", "Jesse Ortiz",
    "Robin Walsh", "Taylor Singh",
];

pub const WORK_TAGS: [&str; 8] = ["mobile", "web", "api", "infra", "ux", "docs", "security", "perf"];

pub const EPICS: [&str; 6] = ["checkout-revamp", "search-v2", "mobile-parity", "billing-cleanup", "onboarding-flow", "data-platform"];

pub const SLUG_WORDS: [&str; 16] = [
    "login", "cache", "export", "sync", "audit", "metrics", "retry", "webhook",
    "profile", "search", "billing", "notify", "upload", "archive", "session", "report",
];

pub const SENTENCES: [&str; 10] = [
    "The current behavior diverges from the design doc in two places.",
    "We agreed to gate this behind a feature flag until QA signs off.",
    "Latency regressions show up only under concurrent writes.",
    "The migration needs a rollback path before it can ship.",
    "Error handling swallows the root cause and logs a generic message.",
    "The retry loop should back off exponentially instead of hammering the API.",
    "Customer feedback suggests the empty state is confusing.",
    "This depends on the platform team exposing a stable endpoint.",
    "Old clients will keep sending the legacy payload for a while.",
    "The dashboard should surface the failure count per tenant.",
];
```

- [ ] **Step 2: Write failing tests** in `themes.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Exact-total property (spec §2): splits must sum back to the remainder
    /// for all three scales.
    #[test]
    fn split_counts_sums_exactly() {
        for total in [1_000u64, 10_000, 100_000] {
            let rem = total - 35;
            let (w, rc, rd) = split_counts(rem);
            assert_eq!(w + rc + rd, rem, "scale {total}");
            assert!(w > rc && rc > rd, "50/30/20 ordering at scale {total}");
        }
        assert_eq!(split_counts(965), (483, 289, 193));
    }

    #[test]
    fn work_files_land_in_three_subdirs_with_exact_count() {
        let dir = tempfile::tempdir().unwrap();
        generate_work(dir.path(), 30).unwrap();
        let mut count = 0;
        for sub in ["plans", "prs", "qa"] {
            let d = dir.path().join("work").join(sub);
            let n = std::fs::read_dir(&d).unwrap().count();
            assert_eq!(n, 10, "{sub} should hold every 3rd file");
            count += n;
        }
        assert_eq!(count, 30);
    }

    /// Regenerating produces identical bytes for the same file.
    #[test]
    fn work_generation_is_deterministic() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        generate_work(a.path(), 12).unwrap();
        generate_work(b.path(), 12).unwrap();
        let name = std::fs::read_dir(a.path().join("work/plans")).unwrap().next().unwrap().unwrap().file_name();
        let pa = a.path().join("work/plans").join(&name);
        let pb = b.path().join("work/plans").join(&name);
        assert_eq!(std::fs::read(pa).unwrap(), std::fs::read(pb).unwrap());
    }

    /// Every generated work file must carry the fields the sample queries
    /// rely on (spec §4.2) — parseable YAML shape checked textually.
    #[test]
    fn work_frontmatter_has_required_fields() {
        let dir = tempfile::tempdir().unwrap();
        generate_work(dir.path(), 9).unwrap();
        let mut stack = vec![dir.path().join("work")];
        let mut seen = 0;
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).unwrap() {
                let e = entry.unwrap();
                if e.file_type().unwrap().is_dir() { stack.push(e.path()); continue; }
                let text = std::fs::read_to_string(e.path()).unwrap();
                for key in ["jira: DCP-", "status: ", "prd: '", "tags: [", "lead: ", "reviewers: [", "estimate:", "  low: ", "  high: ", "priority: ", "created: ", "updated: "] {
                    assert!(text.contains(key), "{} missing {key}", e.path().display());
                }
                seen += 1;
            }
        }
        assert_eq!(seen, 9);
    }
}
```

- [ ] **Step 3: Run tests, verify fail.**

- [ ] **Step 4: Implement in `themes.rs`:**

```rust
//! Scaled contrived themes (spec §3, §4.2–4.4).

use std::fmt::Write as _;
use std::path::Path;

use crate::data;
use crate::rng::{file_rng, stream_rng};
use crate::write::{day_in_window, mtime_at, rfc3339_at, write_md};

/// (work, recipes, reading) — leftover integer-division files go to work.
pub fn split_counts(remainder: u64) -> (u64, u64, u64) {
    let recipes = remainder * 30 / 100;
    let reading = remainder * 20 / 100;
    (remainder - recipes - reading, recipes, reading)
}

pub fn generate_work(root: &Path, n: u64) -> anyhow::Result<()> {
    const SUBDIRS: [&str; 3] = ["plans", "prs", "qa"];
    const STATUSES: [&str; 5] = ["draft", "in-review", "synced", "done", "blocked"];

    for i in 0..n {
        // Naming stream: the filename must exist before the content stream
        // (which is keyed off the path) can run.
        let mut name_rng = stream_rng("work", i);
        let w1 = name_rng.pick(&data::SLUG_WORDS);
        let w2 = name_rng.pick(&data::SLUG_WORDS);
        let sub = SUBDIRS[(i % 3) as usize];
        let rel = format!("work/{sub}/DCP-{}-{w1}-{w2}.md", 100 + i);

        let mut rng = file_rng(&rel);
        // Weighted status: 30/20/20/20/10.
        let status = match rng.range(10) {
            0..=2 => STATUSES[0],
            3..=4 => STATUSES[1],
            5..=6 => STATUSES[2],
            7..=8 => STATUSES[3],
            _ => STATUSES[4],
        };
        let created = day_in_window(&mut rng);
        let updated_date = created + chrono::Days::new(rng.range(90));
        let (uh, um, us) = (rng.range(24) as u32, rng.range(60) as u32, rng.range(60) as u32);
        let low = 1 + rng.range(8);
        let high = low + 1 + rng.range(8);

        let mut fm = String::new();
        writeln!(fm, "jira: DCP-{}", 100 + i)?;
        writeln!(fm, "status: {status}")?;
        writeln!(fm, "prd: '{:03}'", (rng.range(20) + 1) * 10)?;
        if rng.chance(70) {
            writeln!(fm, "epic: {}", rng.pick(&data::EPICS))?;
        }
        writeln!(fm, "tags: [{}]", rng.pick_k(&data::WORK_TAGS, 2 + rng.range(2) as usize).iter().map(|s| **s).collect::<Vec<_>>().join(", "))?;
        writeln!(fm, "lead: {}", rng.pick(&data::NAMES))?;
        writeln!(fm, "reviewers: [{}]", rng.pick_k(&data::NAMES, 2 + rng.range(2) as usize).iter().map(|s| **s).collect::<Vec<_>>().join(", "))?;
        writeln!(fm, "estimate:")?;
        writeln!(fm, "  low: {low}")?;
        writeln!(fm, "  high: {high}")?;
        writeln!(fm, "priority: {}", 1 + rng.range(5))?;
        writeln!(fm, "created: {}", created.format("%Y-%m-%d"))?;
        writeln!(fm, "updated: {}", rfc3339_at(updated_date, uh, um, us))?;
        if rng.chance(60) {
            writeln!(fm, "due: {}", (created + chrono::Days::new(30 + rng.range(90))).format("%Y-%m-%d"))?;
        }

        let mut body = String::new();
        for _ in 0..(2 + rng.range(5)) {
            writeln!(body, "{}", rng.pick(&data::SENTENCES))?;
        }
        if rng.chance(15) {
            writeln!(body, "\nTODO: {}", rng.pick(&data::SENTENCES))?;
        }
        if rng.chance(10) {
            writeln!(body, "\nFIXME: {}", rng.pick(&data::SENTENCES))?;
        }

        // mtime mirrors `updated` exactly (spec §5).
        write_md(root, &rel, &fm, &body, mtime_at(updated_date, uh, um, us))?;
    }
    Ok(())
}
```

- [ ] **Step 5: Run tests, verify pass.** The `split_counts(965) == (483, 289, 193)` assertion is arithmetic ground truth — if it fails, the formula is wrong.

- [ ] **Step 6: fmt, clippy, commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings
git add src/bin/querymatter-samples/ && git commit -m "feat(samples): 50/30/20 count split + scaled work-hub theme"
```

---

### Task 5: Recipes + reading themes (`themes.rs`, pools in `data.rs`)

**Files:**
- Modify: `src/bin/querymatter-samples/data.rs` (append recipe/reading pools)
- Modify: `src/bin/querymatter-samples/themes.rs` (add the two generators + tests)

**Interfaces:**
- Consumes: Task 4's helpers (`split_counts` untouched).
- Produces:
  - `themes::generate_recipes(root: &Path, n: u64) -> anyhow::Result<()>`
  - `themes::generate_reading(root: &Path, n: u64) -> anyhow::Result<()>`
  - Pools: `data::CUISINES: [&str; 8]`, `data::DISH_ADJ: [&str; 8]`, `data::DISH_ING: [&str; 12]`, `data::DISH_FORM: [&str; 8]`, `data::INGREDIENTS: [&str; 16]`, `data::RECIPE_TAGS: [&str; 7]`, `data::STEPS: [&str; 8]`, `data::AUTHORS: [&str; 10]`, `data::TITLE_ADJ: [&str; 10]`, `data::TITLE_NOUN: [&str; 12]`, `data::GENRES: [&str; 8]`, `data::SERIES: [&str; 5]`, `data::NOTES: [&str; 6]`

- [ ] **Step 1: Append pools to `data.rs`** (`DISH_ING` deliberately contains both `Chicken` and `Chickpea` — a sample query regex-matches `chick(en|pea)`):

```rust
pub const CUISINES: [&str; 8] = ["italian", "thai", "mexican", "japanese", "indian", "french", "greek", "korean"];

pub const DISH_ADJ: [&str; 8] = ["Spicy", "Creamy", "Crispy", "Smoky", "Sweet", "Tangy", "Herbed", "Roasted"];

pub const DISH_ING: [&str; 12] = [
    "Chicken", "Chickpea", "Tofu", "Beef", "Mushroom", "Salmon",
    "Eggplant", "Lentil", "Shrimp", "Paneer", "Pork", "Cauliflower",
];

pub const DISH_FORM: [&str; 8] = ["Curry", "Stir-Fry", "Tacos", "Soup", "Salad", "Noodles", "Skewers", "Stew"];

pub const INGREDIENTS: [&str; 16] = [
    "garlic", "onion", "ginger", "soy sauce", "olive oil", "cumin", "basil",
    "lime", "coconut milk", "tomatoes", "rice", "chili flakes", "yogurt",
    "cilantro", "sesame oil", "paprika",
];

pub const RECIPE_TAGS: [&str; 7] = ["vegetarian", "spicy", "quick", "weeknight", "gluten-free", "grill", "comfort"];

pub const STEPS: [&str; 8] = [
    "Heat the oil in a large pan over medium heat.",
    "Add the aromatics and cook until fragrant.",
    "Stir in the main ingredient and sear on all sides.",
    "Deglaze with a splash of stock and scrape up the fond.",
    "Simmer until the sauce thickens slightly.",
    "Season to taste and adjust the acidity.",
    "Rest for five minutes off the heat.",
    "Garnish and serve immediately.",
];

pub const AUTHORS: [&str; 10] = [
    "Iris Malloy", "Theo Grant", "Nadia Osei", "Felix Aran", "June Park",
    "Marco Silva", "Priya Nair", "Owen Blake", "Zara Holt", "Ken Watanabe",
];

pub const TITLE_ADJ: [&str; 10] = ["Silent", "Burning", "Hidden", "Broken", "Endless", "Glass", "Iron", "Hollow", "Distant", "Golden"];

pub const TITLE_NOUN: [&str; 12] = ["Harbor", "Empire", "Garden", "Cipher", "Mountain", "Archive", "Voyage", "Orchard", "Signal", "Kingdom", "Meridian", "Atlas"];

pub const GENRES: [&str; 8] = ["sci-fi", "fantasy", "mystery", "history", "biography", "thriller", "essays", "poetry"];

pub const SERIES: [&str; 5] = ["The Meridian Cycle", "Archive Wars", "The Glass Chronicles", "Signal and Noise", "Kingdom of Ash"];

pub const NOTES: [&str; 6] = [
    "The pacing drags in the middle but the ending lands.",
    "Great worldbuilding with a memorable narrator.",
    "Read this for book club and it split the room.",
    "The research shows without smothering the story.",
    "A reread — holds up better than expected.",
    "Picked up on a recommendation from a colleague.",
];
```

- [ ] **Step 2: Write failing tests** (append to `themes.rs` tests module):

```rust
    #[test]
    fn recipes_land_under_cuisine_dirs_with_exact_count() {
        let dir = tempfile::tempdir().unwrap();
        generate_recipes(dir.path(), 40).unwrap();
        let mut count = 0;
        for entry in std::fs::read_dir(dir.path().join("recipes")).unwrap() {
            let e = entry.unwrap();
            assert!(e.file_type().unwrap().is_dir(), "recipes/* must be cuisine dirs");
            assert!(crate::data::CUISINES.contains(&e.file_name().to_str().unwrap()));
            count += std::fs::read_dir(e.path()).unwrap().count();
        }
        assert_eq!(count, 40);
    }

    #[test]
    fn reading_lands_under_year_dirs_with_exact_count() {
        let dir = tempfile::tempdir().unwrap();
        generate_reading(dir.path(), 24).unwrap();
        let mut count = 0;
        for entry in std::fs::read_dir(dir.path().join("reading")).unwrap() {
            let e = entry.unwrap();
            let year: i32 = e.file_name().to_str().unwrap().parse().unwrap();
            assert!((2019..=2026).contains(&year));
            count += std::fs::read_dir(e.path()).unwrap().count();
        }
        assert_eq!(count, 24);
    }

    #[test]
    fn finished_books_have_rating_and_finished_date() {
        let dir = tempfile::tempdir().unwrap();
        generate_reading(dir.path(), 40).unwrap();
        let mut checked = 0;
        let mut stack = vec![dir.path().join("reading")];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).unwrap() {
                let e = entry.unwrap();
                if e.file_type().unwrap().is_dir() { stack.push(e.path()); continue; }
                let text = std::fs::read_to_string(e.path()).unwrap();
                if text.contains("status: finished") {
                    assert!(text.contains("rating: "), "finished book missing rating");
                    assert!(text.contains("finished: "), "finished book missing finished date");
                    checked += 1;
                } else {
                    assert!(!text.contains("finished: "), "unfinished book has finished date");
                }
            }
        }
        assert!(checked > 0, "sample too small to hit a finished book");
    }

    #[test]
    fn recipe_generation_is_deterministic() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        generate_recipes(a.path(), 10).unwrap();
        generate_recipes(b.path(), 10).unwrap();
        fn tree(p: &std::path::Path) -> Vec<(String, Vec<u8>)> {
            let mut out = vec![];
            let mut stack = vec![p.to_path_buf()];
            while let Some(d) = stack.pop() {
                for entry in std::fs::read_dir(&d).unwrap() {
                    let e = entry.unwrap();
                    if e.file_type().unwrap().is_dir() { stack.push(e.path()); continue; }
                    out.push((e.path().strip_prefix(p).unwrap().display().to_string(), std::fs::read(e.path()).unwrap()));
                }
            }
            out.sort();
            out
        }
        assert_eq!(tree(a.path()), tree(b.path()));
    }
```

- [ ] **Step 3: Run tests, verify fail.**

- [ ] **Step 4: Implement** (append to `themes.rs`):

```rust
pub fn generate_recipes(root: &Path, n: u64) -> anyhow::Result<()> {
    for i in 0..n {
        let mut name_rng = stream_rng("recipes", i);
        let cuisine = name_rng.pick(&data::CUISINES);
        let title = format!(
            "{} {} {}",
            name_rng.pick(&data::DISH_ADJ),
            name_rng.pick(&data::DISH_ING),
            name_rng.pick(&data::DISH_FORM)
        );
        let slug = crate::write::slugify(&title);
        let rel = format!("recipes/{cuisine}/{slug}-{i}.md");

        let mut rng = file_rng(&rel);
        let added = day_in_window(&mut rng);

        let mut fm = String::new();
        writeln!(fm, "title: {title}")?;
        writeln!(fm, "cuisine: {cuisine}")?;
        writeln!(fm, "servings: {}", 2 + rng.range(7))?;
        writeln!(fm, "prep_minutes: {}", 5 + rng.range(41))?;
        writeln!(fm, "cook_minutes: {}", 10 + rng.range(81))?;
        if rng.chance(70) {
            writeln!(fm, "rating: {}", 1 + rng.range(5))?;
        }
        writeln!(fm, "ingredients: [{}]", rng.pick_k(&data::INGREDIENTS, 3 + rng.range(4) as usize).iter().map(|s| **s).collect::<Vec<_>>().join(", "))?;
        writeln!(fm, "tags: [{}]", rng.pick_k(&data::RECIPE_TAGS, 1 + rng.range(3) as usize).iter().map(|s| **s).collect::<Vec<_>>().join(", "))?;
        writeln!(fm, "added: {}", added.format("%Y-%m-%d"))?;
        if rng.chance(50) {
            writeln!(fm, "source: https://example.com/recipes/{slug}")?;
        }

        let mut body = String::from("## Steps\n\n");
        let steps = rng.pick_k(&data::STEPS, 3 + rng.range(4) as usize);
        for (idx, step) in steps.iter().enumerate() {
            writeln!(body, "{}. {step}", idx + 1)?;
        }

        // mtime mirrors `added` at a fixed noon UTC (spec §5).
        write_md(root, &rel, &fm, &body, mtime_at(added, 12, 0, 0))?;
    }
    Ok(())
}

pub fn generate_reading(root: &Path, n: u64) -> anyhow::Result<()> {
    const STATUSES: [&str; 4] = ["queued", "reading", "finished", "abandoned"];

    for i in 0..n {
        let mut name_rng = stream_rng("reading", i);
        let year = 2019 + (i % 8) as i32;
        let title = format!("The {} {}", name_rng.pick(&data::TITLE_ADJ), name_rng.pick(&data::TITLE_NOUN));
        let slug = crate::write::slugify(&title);
        let rel = format!("reading/{year}/{slug}-{i}.md");

        let mut rng = file_rng(&rel);
        // Weighted status: queued 20%, reading 20%, finished 45%, abandoned 15%.
        let status = match rng.range(100) {
            0..=19 => STATUSES[0],
            20..=39 => STATUSES[1],
            40..=84 => STATUSES[2],
            _ => STATUSES[3],
        };
        let started = crate::write::day_in_year(&mut rng, year);

        let mut fm = String::new();
        writeln!(fm, "title: {title}")?;
        writeln!(fm, "author: {}", rng.pick(&data::AUTHORS))?;
        writeln!(fm, "status: {status}")?;
        if status == "finished" {
            writeln!(fm, "rating: {}", 1 + rng.range(5))?;
        }
        writeln!(fm, "pages: {}", 120 + rng.range(781))?;
        writeln!(fm, "genres: [{}]", rng.pick_k(&data::GENRES, 1 + rng.range(2) as usize).iter().map(|s| **s).collect::<Vec<_>>().join(", "))?;
        if rng.chance(25) {
            writeln!(fm, "series:")?;
            writeln!(fm, "  name: {}", rng.pick(&data::SERIES))?;
            writeln!(fm, "  book: {}", 1 + rng.range(5))?;
        }
        let mut finished_day = None;
        if status != "queued" {
            writeln!(fm, "started: {}", started.format("%Y-%m-%d"))?;
        }
        if status == "finished" {
            let f = started + chrono::Days::new(3 + rng.range(60));
            writeln!(fm, "finished: {}", f.format("%Y-%m-%d"))?;
            finished_day = Some(f);
        }
        if rng.chance(40) {
            // Deliberately non-ISO — the DATE(x, '%m/%d/%Y') demo (spec §4.4).
            writeln!(fm, "purchased: '{}'", started.format("%m/%d/%Y"))?;
        }

        let mut body = String::new();
        for _ in 0..(1 + rng.range(3)) {
            writeln!(body, "{}", rng.pick(&data::NOTES))?;
        }

        // mtime: finished, else started, else Jan 15 of the folder year (spec §5).
        let mday = finished_day.unwrap_or(if status == "queued" {
            chrono::NaiveDate::from_ymd_opt(year, 1, 15).unwrap()
        } else {
            started
        });
        write_md(root, &rel, &fm, &body, mtime_at(mday, 8, 0, 0))?;
    }
    Ok(())
}
```

- [ ] **Step 5: Run tests, verify pass.**

- [ ] **Step 6: fmt, clippy, commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings
git add src/bin/querymatter-samples/ && git commit -m "feat(samples): scaled recipe-box and reading-log themes"
```

---

### Task 6: CLI, dir handling, orchestration + determinism integration tests

**Files:**
- Modify: `src/bin/querymatter-samples/main.rs` (full CLI + orchestration)
- Create: `tests/samples_generator.rs`

**Interfaces:**
- Consumes: `starwars::{generate, FILE_COUNT}`, `themes::{split_counts, generate_work, generate_recipes, generate_reading}`.
- Produces: the finished `querymatter-samples` binary. CLI (spec §2): `querymatter-samples [--scale <1k|10k|100k>] [--force] <DIR>`.

- [ ] **Step 1: Write failing integration tests** in `tests/samples_generator.rs`:

```rust
//! Integration tests for the querymatter-samples generator binary.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;

use assert_cmd::Command;

/// Recursive map of rel-path -> (bytes, mtime).
fn tree(root: &Path) -> BTreeMap<String, (Vec<u8>, SystemTime)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let e = entry.unwrap();
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
                continue;
            }
            let rel = e.path().strip_prefix(root).unwrap().display().to_string();
            let mtime = e.metadata().unwrap().modified().unwrap();
            out.insert(rel, (std::fs::read(e.path()).unwrap(), mtime));
        }
    }
    out
}

fn generate(dir: &Path, extra_args: &[&str]) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("querymatter-samples").unwrap();
    cmd.args(extra_args).arg(dir);
    cmd.assert()
}

/// Spec §7 test 1 — the headline determinism guarantee: two generations are
/// identical in paths, bytes, AND mtimes.
#[test]
fn regeneration_is_byte_and_mtime_identical() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    generate(a.path(), &["--scale", "1k"]).success();
    generate(b.path(), &["--scale", "1k"]).success();
    let (ta, tb) = (tree(a.path()), tree(b.path()));
    assert_eq!(ta.len(), tb.len());
    assert_eq!(ta, tb);
}

/// Spec §7 test 2 — exact counts: 1000 total, 35 under starwars/.
#[test]
fn one_k_scale_holds_exactly_1000_files() {
    let dir = tempfile::tempdir().unwrap();
    generate(dir.path(), &["--scale", "1k"]).success();
    let t = tree(dir.path());
    assert_eq!(t.len(), 1000);
    assert_eq!(t.keys().filter(|k| k.starts_with("starwars/")).count(), 35);
    assert!(t.keys().all(|k| k.ends_with(".md")), "tree must be pure .md data");
}

/// Default scale is 1k.
#[test]
fn default_scale_is_1k() {
    let dir = tempfile::tempdir().unwrap();
    generate(dir.path(), &[]).success();
    assert_eq!(tree(dir.path()).len(), 1000);
}

/// Spec §7 test 4 — --force semantics.
#[test]
fn nonempty_dir_refused_without_force() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("precious.txt"), "keep me").unwrap();
    generate(dir.path(), &[])
        .failure()
        .stderr(predicates::str::contains("--force"));
    // The refusal must leave the dir untouched.
    assert!(dir.path().join("precious.txt").exists());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn force_wipes_and_regenerates() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stale.txt"), "old").unwrap();
    generate(dir.path(), &["--force"]).success();
    assert!(!dir.path().join("stale.txt").exists());
    assert_eq!(tree(dir.path()).len(), 1000);
}

/// A missing directory is created (with parents).
#[test]
fn missing_dir_is_created() {
    let parent = tempfile::tempdir().unwrap();
    let target = parent.path().join("deep/nested/samples");
    generate(&target, &[]).success();
    assert_eq!(tree(&target).len(), 1000);
}

/// stdout stays empty — the repo convention is stdout carries data only.
#[test]
fn stdout_is_empty_summary_on_stderr() {
    let dir = tempfile::tempdir().unwrap();
    generate(dir.path(), &[])
        .success()
        .stdout(predicates::str::is_empty())
        .stderr(predicates::str::contains("1000"));
}
```

- [ ] **Step 2: Run tests, verify fail** — `cargo test --test samples_generator` (the binary's `main` is still empty, so every test fails).

- [ ] **Step 3: Implement `main.rs`:**

```rust
//! Deterministic sample-vault generator for querymatter.
//!
//! See docs/superpowers/specs/2026-07-26-sample-generator-design.md.

mod data;
mod rng;
mod starwars;
mod themes;
mod write;

use std::path::PathBuf;

use anyhow::{Context as _, bail};
use clap::{Parser, ValueEnum};

/// Generate a deterministic sample directory of Markdown files with YAML
/// frontmatter, for exploring querymatter and testing it at scale.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Output directory (created if missing; must be empty unless --force).
    dir: PathBuf,

    /// Total number of files to generate.
    #[arg(long, value_enum, default_value_t = Scale::OneK)]
    scale: Scale,

    /// Delete DIR and regenerate it from scratch if it is not empty.
    #[arg(long)]
    force: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum Scale {
    #[value(name = "1k")]
    OneK,
    #[value(name = "10k")]
    TenK,
    #[value(name = "100k")]
    HundredK,
}

impl Scale {
    fn total(self) -> u64 {
        match self {
            Scale::OneK => 1_000,
            Scale::TenK => 10_000,
            Scale::HundredK => 100_000,
        }
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    prepare_dir(&cli)?;

    let total = cli.scale.total();
    let (work, recipes, reading) = themes::split_counts(total - starwars::FILE_COUNT);

    starwars::generate(&cli.dir)?;
    themes::generate_work(&cli.dir, work)?;
    themes::generate_recipes(&cli.dir, recipes)?;
    themes::generate_reading(&cli.dir, reading)?;

    // Summary on stderr — stdout carries data in this repo, and this
    // program's data is the tree itself.
    eprintln!(
        "querymatter-samples: wrote {total} files to {} (starwars {}, work {work}, recipes {recipes}, reading {reading})",
        cli.dir.display(),
        starwars::FILE_COUNT,
    );
    Ok(())
}

fn prepare_dir(cli: &Cli) -> anyhow::Result<()> {
    if cli.dir.exists() {
        let non_empty = std::fs::read_dir(&cli.dir)
            .with_context(|| format!("reading {}", cli.dir.display()))?
            .next()
            .is_some();
        if non_empty {
            if !cli.force {
                bail!(
                    "{} is not empty; pass --force to delete and regenerate it",
                    cli.dir.display()
                );
            }
            std::fs::remove_dir_all(&cli.dir)
                .with_context(|| format!("removing {}", cli.dir.display()))?;
        }
    }
    std::fs::create_dir_all(&cli.dir)
        .with_context(|| format!("creating {}", cli.dir.display()))?;
    Ok(())
}
```

- [ ] **Step 4: Run all tests, verify pass** — `cargo test`. Also sanity-run by hand and eyeball a few files:

```bash
cargo run --bin querymatter-samples -- --scale 1k /tmp/claude-1000/-home-steve-src-hub-reader/*/scratchpad/samples-smoke
find /tmp/claude-1000/-home-steve-src-hub-reader/*/scratchpad/samples-smoke -name '*.md' | wc -l   # → 1000
cargo run --bin querymatter -- -e "SELECT count(*)" /tmp/claude-1000/-home-steve-src-hub-reader/*/scratchpad/samples-smoke
```

The last command must report 1000 — this proves querymatter parses every generated frontmatter block (a file with broken YAML is skipped and would lower the count).

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings
git add src/bin/querymatter-samples/ tests/samples_generator.rs
git commit -m "feat(samples): querymatter-samples CLI with --scale/--force + determinism tests"
```

---

### Task 7: Sample-query docs + snapshot integration test

**Files:**
- Create: `docs/sample-queries.sql`
- Create: `docs/sample-queries.md`
- Create: `tests/sample_queries.rs`

**Interfaces:**
- Consumes: the finished `querymatter-samples` binary (Task 6) and the existing `querymatter` binary.
- Produces: the committed docs; the insta snapshot `tests/snapshots/sample_queries__sample_queries_output.snap`.

- [ ] **Step 1: Write `docs/sample-queries.sql`** — exact content below. Format rules (spec §6.2): `--` comments only, never a `;` inside a comment, file ends with a statement. Every statement must run cleanly against a `--scale 1k` tree.

```sql
-- querymatter sample queries — run against a generated sample tree:
--   cargo run --bin querymatter-samples -- --scale 1k samples
--   querymatter samples < docs/sample-queries.sql
-- Each statement below is explained in docs/sample-queries.md.

-- The whole tree: exactly the --scale you generated
SELECT count(*) AS total;

-- Basic SELECT + WHERE with a numeric comparison
SELECT name, height_cm FROM 'starwars/characters/**' WHERE height_cm > 180 ORDER BY height_cm DESC;

-- String equality WHERE (quoted literal = string comparison)
SELECT name, primary_function FROM 'starwars/characters/**' WHERE kind = 'droid' ORDER BY name;

-- SELECT * — every frontmatter key seen, sorted
SELECT * FROM 'starwars/planets/**' ORDER BY name LIMIT 3;

-- DISTINCT drops duplicate projected rows
SELECT DISTINCT affiliation FROM 'starwars/characters/**' ORDER BY affiliation;

-- file.* pseudo-columns come from the path and stat, not frontmatter
SELECT file.name, file.folder, file.size, file.word_count FROM 'starwars/starships/**' ORDER BY file.size DESC LIMIT 5;

-- file.mtime is deterministic in generated trees (starwars pins 1977-05-25)
SELECT file.name, file.mtime FROM 'starwars/planets/**' ORDER BY file.name LIMIT 3;

-- file.body is read lazily at query time; REGEXP scans it
SELECT file.name FROM 'work/**' WHERE file.body REGEXP 'TODO|FIXME' ORDER BY file.name LIMIT 5;

-- Nested dotted paths walk into YAML mappings
SELECT jira, estimate.low, estimate.high FROM 'work/plans/**' WHERE estimate.high > 12 ORDER BY jira LIMIT 5;

-- MEMBER OF: literal on the left, list-valued column on the right
SELECT name FROM 'starwars/characters/**' WHERE 'EMPIRE' MEMBER OF(episodes) AND NOT 'NEWHOPE' MEMBER OF(episodes) ORDER BY name;

-- MEMBER OF: a column on the left works too
SELECT jira, lead FROM 'work/**' WHERE lead MEMBER OF(reviewers) ORDER BY jira LIMIT 5;

-- LIKE with % wildcards
SELECT name FROM 'starwars/starships/**' WHERE manufacturer LIKE '%Kuat%' ORDER BY name;

-- REGEXP against a computed expression, not just a bare column
SELECT title, cuisine FROM 'recipes/**' WHERE lower(title) REGEXP 'chick(en|pea)' ORDER BY title LIMIT 5;

-- IN over a literal list
SELECT name, home_planet FROM 'starwars/characters/**' WHERE home_planet IN ('Tatooine', 'Naboo') ORDER BY name;

-- IS NULL: absent frontmatter keys read as NULL
SELECT name, climate FROM 'starwars/planets/**' WHERE population IS NULL ORDER BY name;

-- Scalar functions and aliases
SELECT upper(name) AS loud, length(name) AS len FROM 'starwars/characters/**' ORDER BY len DESC LIMIT 3;

-- String concatenation with ||
SELECT substr(name, 1, 8) || '...' AS clipped FROM 'starwars/starships/**' ORDER BY clipped LIMIT 4;

-- Arithmetic in SELECT and WHERE
SELECT title, prep_minutes + cook_minutes AS total_minutes FROM 'recipes/**' WHERE prep_minutes + cook_minutes > 110 ORDER BY total_minutes DESC LIMIT 5;

-- COALESCE picks the first non-null argument
SELECT jira, COALESCE(epic, 'unassigned') AS epic FROM 'work/plans/**' ORDER BY jira LIMIT 5;

-- Searched CASE
SELECT name, CASE WHEN mass_kg IS NULL THEN 'unknown' WHEN mass_kg >= 100 THEN 'heavy' ELSE 'light' END AS build FROM 'starwars/characters/**' ORDER BY name LIMIT 8;

-- CASE as an ORDER BY expression: blocked work first
SELECT jira, status FROM 'work/**' ORDER BY CASE WHEN status = 'blocked' THEN 0 ELSE 1 END, jira LIMIT 5;

-- GROUP BY + count + ORDER BY the alias
SELECT status, count(*) AS n FROM 'work/**' GROUP BY status ORDER BY n DESC;

-- Aggregates with HAVING on an alias
SELECT cuisine, count(*) AS n, avg(prep_minutes) AS avg_prep FROM 'recipes/**' GROUP BY cuisine HAVING n >= 35 ORDER BY n DESC;

-- min / max / sum without GROUP BY
SELECT min(height_cm) AS shortest, max(height_cm) AS tallest, sum(mass_kg) AS total_mass FROM 'starwars/characters/**';

-- count(distinct col)
SELECT count(distinct author) AS authors FROM 'reading/**';

-- group_concat
SELECT kind, group_concat(name) AS members FROM 'starwars/characters/**' GROUP BY kind ORDER BY kind;

-- Auto-detected ISO dates compare chronologically
SELECT count(*) AS created_2026 FROM 'work/**' WHERE created >= '2026-01-01';

-- DATE() with an explicit chrono format parses non-ISO strings
SELECT title, DATE(purchased, '%m/%d/%Y') AS purchased_on FROM 'reading/2026/**' WHERE purchased IS NOT NULL ORDER BY purchased_on LIMIT 5;

-- ORDER BY a bare scalar fn needs parens (or an alias)
SELECT name FROM 'starwars/planets/**' ORDER BY (upper(name)) LIMIT 3;

-- LIMIT/OFFSET pagination
SELECT name FROM 'starwars/characters/**' ORDER BY name LIMIT 5 OFFSET 5;

-- \G renders one row as name: value lines (great for wide rows)
SELECT * FROM 'starwars/planets/**' WHERE name = 'Dagobah'\G
```

- [ ] **Step 2: Write the failing snapshot test** in `tests/sample_queries.rs`:

```rust
//! Pins docs/sample-queries.sql against a generated 1k tree.
//!
//! This is the load-bearing test for the committed sample queries: it fails
//! if the generator's data drifts, if the DSL's behavior drifts, or if the
//! .sql file stops parsing — the queries can never silently rot.

use assert_cmd::Command;

#[test]
fn sample_queries_run_clean_and_match_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("querymatter-samples")
        .unwrap()
        .args(["--scale", "1k"])
        .arg(dir.path())
        .assert()
        .success();

    let sql = std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/docs/sample-queries.sql"),
    )
    .unwrap();

    let assert = Command::cargo_bin("querymatter")
        .unwrap()
        .arg(dir.path())
        .write_stdin(sql)
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    insta::assert_snapshot!("sample_queries_output", stdout);
}
```

- [ ] **Step 3: Run the test** — `cargo test --test sample_queries`. First run fails with a new-snapshot diff (or a query error — fix the `.sql` if any statement is rejected; the DSL boundaries in README.md §"Boundaries worth knowing" are the reference). Inspect the pending snapshot **line by line** — every query's output must be plausible and non-empty where the doc claims results (`cargo insta review`, or open `tests/snapshots/*.snap.new`). Only accept once each section looks right; if a WHERE threshold yields zero rows at 1k scale (e.g. `estimate.high > 12` or `total_minutes > 110` or `HAVING n >= 35`), tune the threshold in the `.sql` (and later the `.md`) until the output is illustrative, then re-run.

- [ ] **Step 4: Accept the snapshot** — `cargo insta accept` (or move the `.snap.new`). Re-run `cargo test --test sample_queries` → PASS.

- [ ] **Step 5: Write `docs/sample-queries.md`.** Structure (fill every "output" block by copying the real rendered output from the snapshot file — never hand-type result tables):

````markdown
# Sample queries

Generate the sample tree first, then run any query below against it:

```sh
cargo run --bin querymatter-samples -- --scale 1k samples
querymatter -e "SELECT count(*) AS total" samples
```

`samples/` is gitignored — regenerate it any time; generation is fully
deterministic (same build ⇒ byte-identical tree, mtimes included).

The whole file is also runnable in one shot via batch mode:

```sh
querymatter samples < docs/sample-queries.sql
```

Every result shown below assumes `--scale 1k`. `starwars/` output is
identical at every scale (that folder is fixed); the scaled folders
(`work/`, `recipes/`, `reading/`) change with `--scale`.

## The data

| Folder | Files at 1k | Theme |
| --- | --- | --- |
| `starwars/` | 35 (every scale) | The classic GraphQL star-wars cast: characters, starships, planets |
| `work/` | 483 | Work-doc hub: jira tickets with status, tags, nested estimates |
| `recipes/` | 289 | Recipe box: cuisines, timings, ingredient lists |
| `reading/` | 193 | Reading log by year: authors, ratings, series |

<!-- One H2 section per capability, in the same order as sample-queries.sql.
     Each section: one or two sentences of intent, the query in a ```sql block,
     the real output in a ``` block copied from the accepted snapshot. -->

## Counting the tree
...

## Relative-date literals (time-dependent)

These resolve against the clock at query time, so their results depend on
when you run them — they're not in `sample-queries.sql` (whose output is
pinned by a test):

```sql
SELECT jira, updated FROM 'work/**' WHERE updated >= '-6mo' ORDER BY updated DESC LIMIT 5
SELECT count(*) AS overdue FROM 'work/**' WHERE due < 'today'
```

## Erroring on purpose: unknown-column validation

```console
$ querymatter -e "SELECT staus" samples
Error: ... unknown column `staus`, did you mean 'status'?
```

## Other output formats

```sh
querymatter -e "SELECT name, climate FROM 'starwars/planets/**'" --format json samples | jq '.[0]'
querymatter -e "SELECT status, count(*) AS n FROM 'work/**' GROUP BY status" --format csv samples
```

## Testing at scale

```sh
cargo run --release --bin querymatter-samples -- --scale 100k --force samples
time querymatter -e "SELECT status, count(*) AS n FROM 'work/**' GROUP BY status ORDER BY n DESC" samples
querymatter init samples        # build the .querymatter cache
time querymatter -e "SELECT status, count(*) AS n FROM 'work/**' GROUP BY status ORDER BY n DESC" samples
```
````

Cover, as `##` sections with real output, every capability in spec §6.1's
checklist items 1–17 (the `.sql` above already maps one query to each; the
`.md` mirrors its order and adds the prose-only callouts: `\G` appears as the
last runnable query, relative dates / unknown-column / formats / scale as the
prose sections shown).

- [ ] **Step 6: Verify the doc's commands work** — spot-run at minimum: the `--format json | jq` line, the unknown-column error line, and one relative-date query, against a generated `samples/` tree in the scratchpad. (The `.sql` statements are already pinned by the snapshot test.)

- [ ] **Step 7: fmt, clippy, commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings
git add docs/sample-queries.sql docs/sample-queries.md tests/sample_queries.rs tests/snapshots/
git commit -m "docs(samples): sample-query walkthrough + runnable script, pinned by snapshot test"
```

---

### Task 8: README, TODO, justfile

**Files:**
- Modify: `README.md` (new section; place it after the query-DSL/flags material, before or alongside the caching section — match the existing TOC style)
- Modify: `TODO.md` (check off the sample-generator item)
- Modify: `justfile` (add `samples` recipe)

- [ ] **Step 1: README section** (adjust anchor links to the README's actual TOC conventions):

```markdown
## Sample data & sample queries

The repo ships a deterministic sample-vault generator as a second binary:

```sh
cargo run --bin querymatter-samples -- --scale 1k samples
```

This writes exactly 1000 Markdown files (`--scale 10k` / `--scale 100k` for
10,000 / 100,000) into `samples/` (gitignored): a fixed `starwars/` folder —
the classic GraphQL star-wars cast, identical at every scale — plus three
scaled themes (`work/`, `recipes/`, `reading/`). Generation is fully
deterministic: wiping the directory and regenerating from the same build
produces byte-identical files, mtimes included. A non-empty target directory
is refused unless you pass `--force` (which deletes and regenerates it).

[`docs/sample-queries.md`](docs/sample-queries.md) walks through queries
exercising most of the DSL against this tree, with expected output;
[`docs/sample-queries.sql`](docs/sample-queries.sql) is the runnable version
(`querymatter samples < docs/sample-queries.sql`), pinned by an integration
test so the examples can never silently rot. The 100k scale plus
`querymatter init samples` is a quick way to feel the cache speedup on a
large vault.
```

- [ ] **Step 2: TODO.md** — change the first item to checked and annotate:

```markdown
- [x] Add a script to generate a deterministic sample directory … *(shipped 2026-07-26: `querymatter-samples` bin, `docs/sample-queries.{md,sql}` — see docs/superpowers/specs/2026-07-26-sample-generator-design.md)*
```

(Keep the original item text; append the annotation.)

- [ ] **Step 3: justfile recipe:**

```make
samples:
  cargo run --bin querymatter-samples -- --force --scale 1k samples
```

- [ ] **Step 4: Verify** — `just samples` generates; `cargo test` fully green; `cargo clippy --all-targets -- -D warnings` clean; `cargo fmt --all` produces no diff.

- [ ] **Step 5: Commit**

```bash
git add README.md TODO.md justfile
git commit -m "docs: README sample-data section, TODO check-off, just samples recipe"
```

---

## Self-Review Notes

- **Spec coverage:** §2 CLI → Task 6; §3 layout/split → Tasks 4–6; §4.1 → Task 3; §4.2 → Task 4; §4.3–4.4 → Task 5; §5 determinism → Tasks 1–2 (mechanisms) + Task 6 test 1 (guarantee); §6.1 → Task 7 step 5; §6.2 → Task 7 step 1; §7 tests 1–5 → Tasks 6 (1, 2, 4), 7 (3), 1/3/4 (5); §8 → Task 8.
- **Types:** `split_counts` returns `(work, recipes, reading)` — Task 6's destructuring matches. `FILE_COUNT` is `u64`; `Scale::total()` is `u64`; subtraction is safe (min scale 1000 > 35).
- **Known judgment points for implementers:** if any `.sql` statement trips a DSL boundary the README documents, prefer rewriting the query to the documented form over touching the query engine; if a threshold yields empty output at 1k, tune the threshold (Task 7 step 3 explicitly allows this and requires updating the `.md` to match).
