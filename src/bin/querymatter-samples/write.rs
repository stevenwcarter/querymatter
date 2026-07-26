//! File emission: exact bytes, deterministic mtimes.

// This module's full public surface is the Task 2 interface contract that
// later tasks in the sample-generator plan consume (see
// docs/superpowers/plans/2026-07-26-sample-generator.md); `main()` doesn't
// call into it until Task 6 wires the generator pipeline together. The
// tests below already pin the implementation. Drop this once Task 6 lands
// and every item has a real caller.
#![allow(dead_code)]

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use chrono::NaiveDate;

use crate::rng::SplitMix64;

/// Writes `---\n{frontmatter}---\n\n{body}` to `root/rel`, creating parent
/// dirs as needed, then sets the file's mtime explicitly (never the clock).
///
/// Callers guarantee `frontmatter` and `body` each already end with `\n`.
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

/// Whole-second Unix timestamp for `date` at `h:m:s` UTC.
pub fn mtime_at(date: NaiveDate, h: u32, m: u32, s: u32) -> SystemTime {
    let secs = date.and_hms_opt(h, m, s).unwrap().and_utc().timestamp();
    UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).expect("dates are post-epoch"))
}

/// RFC-3339 `YYYY-MM-DDTHH:MM:SSZ` for `date` at `h:m:s` UTC.
pub fn rfc3339_at(date: NaiveDate, h: u32, m: u32, s: u32) -> String {
    format!("{}T{h:02}:{m:02}:{s:02}Z", date.format("%Y-%m-%d"))
}

/// Lowercases, keeps alphanumerics, collapses every other run of chars into
/// a single `-`, and trims leading/trailing dashes.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    use chrono::Datelike as _;

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
        assert_eq!(
            mtime_at(d, 0, 0, 0),
            UNIX_EPOCH + std::time::Duration::from_secs(86_400)
        );
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
        let mtime = mtime_at(
            chrono::NaiveDate::from_ymd_opt(1977, 5, 25).unwrap(),
            0,
            0,
            0,
        );
        write_md(dir.path(), "a/b/test.md", "name: Luke\n", "Body.\n", mtime).unwrap();
        let p = dir.path().join("a/b/test.md");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "---\nname: Luke\n---\n\nBody.\n"
        );
        assert_eq!(std::fs::metadata(&p).unwrap().modified().unwrap(), mtime);
    }
}
