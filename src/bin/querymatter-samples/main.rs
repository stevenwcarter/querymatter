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
    std::fs::create_dir_all(&cli.dir).with_context(|| format!("creating {}", cli.dir.display()))?;
    Ok(())
}
