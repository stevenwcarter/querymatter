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
            c.name,
            c.kind,
            c.home_planet,
            c.affiliation,
            c.episodes.join(", "),
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
        let body = format!(
            "The {} is a {} built by {}.\n",
            s.name, s.model, s.manufacturer
        );
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
        let body = format!(
            "{} has a {} climate and {} terrain.\n",
            p.name, p.climate, p.terrain
        );
        let rel = format!("starwars/planets/{}.md", slugify(p.name));
        write_md(root, &rel, &fm, &body, mtime)?;
    }

    Ok(())
}

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
        for name in [
            "Luke Skywalker",
            "Darth Vader",
            "Han Solo",
            "Leia Organa",
            "Wilhuff Tarkin",
            "C-3PO",
            "R2-D2",
        ] {
            assert!(
                data::CHARACTERS.iter().any(|c| c.name == name),
                "missing {name}"
            );
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
                if e.file_type().unwrap().is_dir() {
                    stack.push(e.path());
                } else {
                    count += 1;
                }
            }
        }
        assert_eq!(count, 35);
        let luke = dir.path().join("starwars/characters/luke-skywalker.md");
        let expected_mtime = crate::write::mtime_at(
            chrono::NaiveDate::from_ymd_opt(1977, 5, 25).unwrap(),
            0,
            0,
            0,
        );
        assert_eq!(
            std::fs::metadata(&luke).unwrap().modified().unwrap(),
            expected_mtime
        );
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
