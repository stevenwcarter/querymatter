//! Scaled contrived themes (spec §3, §4.2–4.4).

// This module's public surface (`split_counts`, `generate_work`) is a Task 4
// interface contract that Task 5 (recipes/reading) and Task 6 (pipeline
// wiring) consume (see docs/superpowers/plans/2026-07-26-sample-generator.md);
// `main()` doesn't call into it until Task 6 lands. The tests below already
// pin the implementation. Drop this once Task 6 lands and every item has a
// real caller.
#![allow(dead_code)]

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

/// Generates `n` work-hub files spread round-robin across `plans/`, `prs/`,
/// and `qa/` (spec §4.2).
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
        let (uh, um, us) = (
            rng.range(24) as u32,
            rng.range(60) as u32,
            rng.range(60) as u32,
        );
        let low = 1 + rng.range(8);
        let high = low + 1 + rng.range(8);

        let mut fm = String::new();
        writeln!(fm, "jira: DCP-{}", 100 + i)?;
        writeln!(fm, "status: {status}")?;
        writeln!(fm, "prd: '{:03}'", (rng.range(20) + 1) * 10)?;
        if rng.chance(70) {
            writeln!(fm, "epic: {}", rng.pick(&data::EPICS))?;
        }
        let tag_count = 2 + rng.range(2) as usize;
        writeln!(
            fm,
            "tags: [{}]",
            rng.pick_k(&data::WORK_TAGS, tag_count)
                .iter()
                .map(|s| **s)
                .collect::<Vec<_>>()
                .join(", ")
        )?;
        writeln!(fm, "lead: {}", rng.pick(&data::NAMES))?;
        let reviewer_count = 2 + rng.range(2) as usize;
        writeln!(
            fm,
            "reviewers: [{}]",
            rng.pick_k(&data::NAMES, reviewer_count)
                .iter()
                .map(|s| **s)
                .collect::<Vec<_>>()
                .join(", ")
        )?;
        writeln!(fm, "estimate:")?;
        writeln!(fm, "  low: {low}")?;
        writeln!(fm, "  high: {high}")?;
        writeln!(fm, "priority: {}", 1 + rng.range(5))?;
        writeln!(fm, "created: {}", created.format("%Y-%m-%d"))?;
        writeln!(fm, "updated: {}", rfc3339_at(updated_date, uh, um, us))?;
        if rng.chance(60) {
            writeln!(
                fm,
                "due: {}",
                (created + chrono::Days::new(30 + rng.range(90))).format("%Y-%m-%d")
            )?;
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
        let name = std::fs::read_dir(a.path().join("work/plans"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name();
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
                if e.file_type().unwrap().is_dir() {
                    stack.push(e.path());
                    continue;
                }
                let text = std::fs::read_to_string(e.path()).unwrap();
                for key in [
                    "jira: DCP-",
                    "status: ",
                    "prd: '",
                    "tags: [",
                    "lead: ",
                    "reviewers: [",
                    "estimate:",
                    "  low: ",
                    "  high: ",
                    "priority: ",
                    "created: ",
                    "updated: ",
                ] {
                    assert!(text.contains(key), "{} missing {key}", e.path().display());
                }
                seen += 1;
            }
        }
        assert_eq!(seen, 9);
    }
}
