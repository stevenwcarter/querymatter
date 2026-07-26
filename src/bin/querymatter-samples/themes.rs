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

/// Generates `n` recipe files spread across cuisine subdirectories of
/// `recipes/` (spec §4.3).
pub fn generate_recipes(root: &Path, n: u64) -> anyhow::Result<()> {
    for i in 0..n {
        // Naming stream: the filename must exist before the content stream
        // (which is keyed off the path) can run.
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
        let ingredient_count = 3 + rng.range(4) as usize;
        writeln!(
            fm,
            "ingredients: [{}]",
            rng.pick_k(&data::INGREDIENTS, ingredient_count)
                .iter()
                .map(|s| **s)
                .collect::<Vec<_>>()
                .join(", ")
        )?;
        let tag_count = 1 + rng.range(3) as usize;
        writeln!(
            fm,
            "tags: [{}]",
            rng.pick_k(&data::RECIPE_TAGS, tag_count)
                .iter()
                .map(|s| **s)
                .collect::<Vec<_>>()
                .join(", ")
        )?;
        writeln!(fm, "added: {}", added.format("%Y-%m-%d"))?;
        if rng.chance(50) {
            writeln!(fm, "source: https://example.com/recipes/{slug}")?;
        }

        let mut body = String::from("## Steps\n\n");
        let step_count = 3 + rng.range(4) as usize;
        let steps = rng.pick_k(&data::STEPS, step_count);
        for (idx, step) in steps.iter().enumerate() {
            writeln!(body, "{}. {step}", idx + 1)?;
        }

        // mtime mirrors `added` at a fixed noon UTC (spec §5).
        write_md(root, &rel, &fm, &body, mtime_at(added, 12, 0, 0))?;
    }
    Ok(())
}

/// Generates `n` reading-log files spread across year subdirectories of
/// `reading/` (spec §4.4).
pub fn generate_reading(root: &Path, n: u64) -> anyhow::Result<()> {
    const STATUSES: [&str; 4] = ["queued", "reading", "finished", "abandoned"];

    for i in 0..n {
        let mut name_rng = stream_rng("reading", i);
        let year = 2019 + (i % 8) as i32;
        let title = format!(
            "The {} {}",
            name_rng.pick(&data::TITLE_ADJ),
            name_rng.pick(&data::TITLE_NOUN)
        );
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
        let genre_count = 1 + rng.range(2) as usize;
        writeln!(
            fm,
            "genres: [{}]",
            rng.pick_k(&data::GENRES, genre_count)
                .iter()
                .map(|s| **s)
                .collect::<Vec<_>>()
                .join(", ")
        )?;
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

    #[test]
    fn recipes_land_under_cuisine_dirs_with_exact_count() {
        let dir = tempfile::tempdir().unwrap();
        generate_recipes(dir.path(), 40).unwrap();
        let mut count = 0;
        for entry in std::fs::read_dir(dir.path().join("recipes")).unwrap() {
            let e = entry.unwrap();
            assert!(
                e.file_type().unwrap().is_dir(),
                "recipes/* must be cuisine dirs"
            );
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
                if e.file_type().unwrap().is_dir() {
                    stack.push(e.path());
                    continue;
                }
                let text = std::fs::read_to_string(e.path()).unwrap();
                if text.contains("status: finished") {
                    assert!(text.contains("rating: "), "finished book missing rating");
                    assert!(
                        text.contains("finished: "),
                        "finished book missing finished date"
                    );
                    checked += 1;
                } else {
                    assert!(
                        !text.contains("finished: "),
                        "unfinished book has finished date"
                    );
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
                    if e.file_type().unwrap().is_dir() {
                        stack.push(e.path());
                        continue;
                    }
                    out.push((
                        e.path().strip_prefix(p).unwrap().display().to_string(),
                        std::fs::read(e.path()).unwrap(),
                    ));
                }
            }
            out.sort();
            out
        }
        assert_eq!(tree(a.path()), tree(b.path()));
    }
}
