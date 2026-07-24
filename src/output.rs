//! [`OutputSink`]: where a rendered query-result block goes — stdout (the
//! default) or a file, opened once and reused for the rest of a run/session.
//! Shared by the one-shot/batch `--output` flag (`main.rs`) and the REPL's
//! `.output` dot-command (`repl.rs`), so both redirect through the same
//! truncate-then-append semantics: opening a path clears whatever was there
//! before, and every write after that appends to the same open handle.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Where a statement's rendered result block is written.
pub enum OutputSink {
    /// The default: each block prints to stdout via `println!`, exactly as
    /// before this feature existed.
    Stdout,
    /// Redirected to an already-open file handle, positioned to append after
    /// whatever has already been written this run/session.
    File(File),
}

impl OutputSink {
    /// Opens `path` for writing, creating it if needed and truncating any
    /// existing content — the one point where a redirect target is
    /// established. Every [`write_block`](Self::write_block) call after this
    /// appends to the same handle, so callers open once per redirect and
    /// reuse the returned sink. Callers add path context to a failure via
    /// `anyhow`.
    pub fn open_file(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        Ok(OutputSink::File(file))
    }

    /// Writes `block` followed by a trailing newline — mirroring the
    /// `println!` this sink replaces — to stdout or the redirected file.
    pub fn write_block(&mut self, block: &str) -> io::Result<()> {
        match self {
            OutputSink::Stdout => {
                println!("{block}");
                Ok(())
            }
            OutputSink::File(file) => writeln!(file, "{block}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn write_block_appends_within_the_same_sink() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let mut sink = OutputSink::open_file(&path).unwrap();
        sink.write_block("first").unwrap();
        sink.write_block("second").unwrap();
        drop(sink);
        assert_eq!(fs::read_to_string(&path).unwrap(), "first\nsecond\n");
    }

    #[test]
    fn reopening_the_same_path_truncates_prior_contents() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("out.txt");

        let mut sink = OutputSink::open_file(&path).unwrap();
        sink.write_block("stale").unwrap();
        drop(sink);

        let mut sink = OutputSink::open_file(&path).unwrap();
        sink.write_block("fresh").unwrap();
        drop(sink);

        assert_eq!(fs::read_to_string(&path).unwrap(), "fresh\n");
    }

    #[test]
    fn open_file_errors_on_an_unwritable_path() {
        let dir = tempdir().unwrap();
        let missing_parent = dir.path().join("no-such-dir").join("out.txt");
        assert!(OutputSink::open_file(&missing_parent).is_err());
    }
}
