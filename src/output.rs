//! [`OutputSink`]: where a rendered query-result block goes — stdout (the
//! default) or a file, opened once and reused for the rest of a run/session.
//! Shared by the one-shot/batch `--output` flag (`main.rs`) and the REPL's
//! `.output` dot-command (`repl.rs`), so both redirect through the same
//! truncate-then-append semantics: opening a path clears whatever was there
//! before, and every write after that appends to the same open handle.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

/// Where a statement's rendered result block is written.
pub enum OutputSink {
    /// The default: each block prints to stdout via `println!`, exactly as
    /// before this feature existed.
    Stdout,
    /// Redirected to an already-open file handle, positioned to append after
    /// whatever has already been written this run/session.
    File(File),
    /// A shell command (REPL's `.output |cmd`, the sqlite3 convention)
    /// receiving each block on its stdin. Stdout/stderr are inherited so an
    /// interactive pager or filter draws straight to the terminal.
    Command(Child),
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

    /// Spawns `sh -c "<cmd>"` with a piped stdin, so pipelines, arguments,
    /// and redirects all work exactly as they would if `cmd` were typed at a
    /// shell prompt (the sqlite3 `.output |cmd` convention). Stdout/stderr
    /// are inherited, not captured, so an interactive pager or filter still
    /// draws to the terminal.
    pub fn open_command(cmd: &str) -> io::Result<Self> {
        let child = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .stdin(Stdio::piped())
            .spawn()?;
        Ok(OutputSink::Command(child))
    }

    /// Runs `f` with a writer aimed at this sink's destination — a locked
    /// stdout handle, the redirected file, or a piped command's stdin — then
    /// flushes. The streaming counterpart of [`write_block`](Self::write_block):
    /// the caller writes the fully-formatted, newline-terminated block itself
    /// (see `render::render_to`), so no intermediate `String` is built.
    pub fn write_result(
        &mut self,
        f: impl FnOnce(&mut dyn Write) -> io::Result<()>,
    ) -> io::Result<()> {
        match self {
            OutputSink::Stdout => {
                let stdout = io::stdout();
                let mut lock = stdout.lock();
                f(&mut lock)?;
                lock.flush()
            }
            OutputSink::File(file) => {
                f(file)?;
                file.flush()
            }
            OutputSink::Command(child) => {
                let stdin = child.stdin.as_mut().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "child stdin closed")
                })?;
                f(stdin)?;
                stdin.flush()
            }
        }
    }

    /// Writes `block` followed by a trailing newline — mirroring the
    /// `println!` this sink replaces — to stdout, the redirected file, or a
    /// piped command's stdin.
    pub fn write_block(&mut self, block: &str) -> io::Result<()> {
        self.write_result(|w| {
            w.write_all(block.as_bytes())?;
            w.write_all(b"\n")
        })
    }

    /// Closes a piped child's stdin (signaling EOF) and waits for it to
    /// exit, so a pager or filter has flushed and finished before the sink
    /// is dropped or replaced. No-op for [`Stdout`](Self::Stdout) and
    /// [`File`](Self::File). `Child::wait` already drops `stdin` itself
    /// before waiting, so there's no need to do it here first.
    pub fn finish(&mut self) -> io::Result<()> {
        if let OutputSink::Command(child) = self {
            child.wait()?;
        }
        Ok(())
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
    fn write_result_streams_into_the_same_sink() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let mut sink = OutputSink::open_file(&path).unwrap();
        sink.write_result(|w| {
            w.write_all(b"a,b\n")?;
            w.write_all(b"1,2\n")
        })
        .unwrap();
        sink.write_result(|w| w.write_all(b"tail\n")).unwrap();
        drop(sink);
        assert_eq!(fs::read_to_string(&path).unwrap(), "a,b\n1,2\ntail\n");
    }

    #[test]
    fn open_file_errors_on_an_unwritable_path() {
        let dir = tempdir().unwrap();
        let missing_parent = dir.path().join("no-such-dir").join("out.txt");
        assert!(OutputSink::open_file(&missing_parent).is_err());
    }

    #[test]
    fn command_sink_pipes_blocks_through_the_shell() {
        let dir = tempdir().unwrap();
        let out = dir.path().join("piped.txt");
        // `cat > file` via sh -c: blocks written to the child's stdin land in the file.
        let mut sink = OutputSink::open_command(&format!("cat > {}", out.display())).unwrap();
        sink.write_block("alpha").unwrap();
        sink.write_block("beta").unwrap();
        sink.finish().unwrap(); // closes stdin, waits for the child
        assert_eq!(fs::read_to_string(&out).unwrap(), "alpha\nbeta\n");
    }
}
