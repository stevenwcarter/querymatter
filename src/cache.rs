//! The `.querymatter/` on-disk cache data model.
//!
//! This module owns the *shape* of the persistent cache and its bincode
//! (de)serialization — a `CachedDir` blob per scanned filesystem directory,
//! plus a versioned `manifest.bin` header that lets a future format change
//! be detected and safely discarded rather than mis-decoded. Nothing here
//! touches the filesystem; reading/writing the actual files, freshness
//! checks, and vault discovery are later phases of the cache-vault feature.

use indexmap::IndexMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use std::{fs, io};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::model::Value;

/// The on-disk directory (relative to a vault root) holding `manifest.bin`
/// and one blob file per cached directory.
const CACHE_DIR_NAME: &str = ".querymatter";

/// Filename of the manifest within [`CACHE_DIR_NAME`].
const MANIFEST_FILE_NAME: &str = "manifest.bin";

/// Magic bytes identifying a `manifest.bin` file, checked before any bincode
/// decode is attempted.
pub const MAGIC: [u8; 4] = *b"QMDB";

/// Bumped whenever any cached struct's shape changes. Together with `MAGIC`
/// this is the only safe "format changed → discard the cache" mechanism,
/// since bincode itself has no schema versioning.
pub const SCHEMA_VERSION: u32 = 1;

/// One cached Markdown file: its frontmatter fields plus enough metadata
/// (`mtime`, `size`) to detect that it has changed on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedFile {
    pub rel_path: String,
    pub mtime: SystemTime,
    pub size: u64,
    pub fields: IndexMap<String, Value>,
}

/// One cached filesystem directory: every matched file directly under it,
/// plus the directory's own `mtime` (used by the `--fast` freshness hybrid)
/// and when it was scanned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedDir {
    pub dir: PathBuf,
    pub scanned_at: SystemTime,
    pub dir_mtime: SystemTime,
    pub files: Vec<CachedFile>,
}

/// A `manifest.bin` entry pointing at one `CachedDir` blob.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub dir: PathBuf,
    pub scanned_at: SystemTime,
    pub dir_mtime: SystemTime,
    pub blob: String,
}

/// The bincode-encoded body of `manifest.bin`, i.e. everything after the
/// `MAGIC ++ SCHEMA_VERSION` header. Kept separate from the header fields so
/// the header can be validated from raw bytes before any decode is attempted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestBody {
    pub crate_version: String,
    pub ttl_secs: u64,
    pub dirs: Vec<ManifestEntry>,
}

/// Encodes `value` with bincode's serde bridge (`bincode::config::standard()`).
///
/// # Panics
/// If `value` contains a non-UTF-8 `PathBuf`/`OsString` (serde's `Path` impl
/// errors in that case) — a documented, accepted limitation of the cache
/// (design spec §9). **Also** if `value` contains a [`SystemTime`] earlier
/// than [`std::time::UNIX_EPOCH`] (serde's `SystemTime` impl errors before
/// encoding a negative duration). Real filesystem mtimes can be pre-epoch
/// (clock skew, `touch -d`, archive extraction), so callers that encode
/// values built from live filesystem metadata — e.g. `save_cache` — must use
/// [`try_encode`] instead of calling this directly.
pub fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    try_encode(value).expect("value not encodable (non-UTF-8 path or pre-epoch SystemTime)")
}

/// Fallible sibling of [`encode`]: `Err` instead of a panic on any bincode
/// encode error (see [`encode`]'s panic note). Used wherever the encoded
/// value may hold data pulled from the live filesystem rather than
/// constructed in-process, so one anomalous value can't crash a whole batch.
fn try_encode<T: Serialize>(value: &T) -> Result<Vec<u8>, bincode::error::EncodeError> {
    bincode::serde::encode_to_vec(value, bincode::config::standard())
}

/// Decodes a `T` previously produced by [`encode`], or `None` on any error
/// (truncated bytes, corrupt data, shape mismatch).
pub fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Option<T> {
    bincode::serde::decode_from_slice(bytes, bincode::config::standard())
        .map(|(value, _)| value)
        .ok()
}

/// Writes a `manifest.bin` payload: `MAGIC ++ SCHEMA_VERSION (LE u32) ++
/// bincode(body)`.
pub fn write_manifest_bytes(body: &ManifestBody) -> Vec<u8> {
    try_write_manifest_bytes(body)
        .expect("value not encodable (non-UTF-8 path or pre-epoch SystemTime)")
}

/// Fallible sibling of [`write_manifest_bytes`]; see [`try_encode`].
fn try_write_manifest_bytes(body: &ManifestBody) -> Result<Vec<u8>, bincode::error::EncodeError> {
    let mut bytes = Vec::with_capacity(8);
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
    bytes.extend_from_slice(&try_encode(body)?);
    Ok(bytes)
}

/// Validates the `manifest.bin` header (`MAGIC` then `SCHEMA_VERSION`)
/// *before* attempting to decode the body, so a future struct-shape change
/// (signaled by a version bump) can never be mis-decoded as the current
/// shape. Returns `None` on any mismatch, truncation, or decode failure.
pub fn read_manifest_bytes(bytes: &[u8]) -> Option<ManifestBody> {
    if bytes.len() < 8 || bytes[0..4] != MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    if version != SCHEMA_VERSION {
        return None;
    }
    decode(&bytes[8..])
}

/// A deterministic, filesystem-safe hash of `dir`'s absolute path, used to
/// derive its blob filename. Stable only within one build (`DefaultHasher`
/// makes no cross-version guarantee), which is fine: any format change that
/// would matter also bumps [`SCHEMA_VERSION`], discarding the whole cache.
fn stable_hash(dir: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();
    dir.hash(&mut hasher);
    hasher.finish()
}

/// The blob filename for `dir`, e.g. `"3f2a9c1d8b0e4f5a.bin"`.
fn blob_file_name(dir: &Path) -> String {
    format!("{:016x}.bin", stable_hash(dir))
}

/// Writes `bytes` to `path` atomically: encode to a temp file in the same
/// directory, then `rename` over `path`. On a POSIX filesystem `rename` is
/// atomic, so a reader never observes a partially written `path` and a crash
/// mid-write leaves only the untouched previous file (or nothing) at `path`.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "write_atomic: path has no file name",
        )
    })?;
    // `process::id()` keeps concurrent runs of the binary from colliding on
    // the same temp name; a single run never writes the same target path
    // twice concurrently, so no further uniquing is needed.
    let tmp_path = dir.join(format!(
        "{}.tmp-{}",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    fs::write(&tmp_path, bytes)?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Persists `dirs` to `<vault_dir>/.querymatter/`: one blob file per
/// [`CachedDir`], then `manifest.bin` last, all via [`write_atomic`] so a
/// reader never sees a manifest that outraces its blobs.
///
/// A `CachedDir` that fails to encode (see [`encode`]'s panic note — in
/// practice a pre-1970 mtime from clock skew or archive extraction) is
/// skipped with a warning on stderr and omitted from the manifest, so the
/// caller simply re-scans that one directory next time rather than losing
/// the whole cache save.
pub fn save_cache(vault_dir: &Path, dirs: &[CachedDir], ttl_secs: u64) -> anyhow::Result<()> {
    let cache_dir = vault_dir.join(CACHE_DIR_NAME);
    fs::create_dir_all(&cache_dir).with_context(|| format!("creating {}", cache_dir.display()))?;

    let mut entries = Vec::with_capacity(dirs.len());
    for dir in dirs {
        let bytes = match try_encode(dir) {
            Ok(bytes) => bytes,
            Err(err) => {
                eprintln!(
                    "querymatter: cache: skipping {} (won't encode: {err})",
                    dir.dir.display()
                );
                continue;
            }
        };
        let blob = blob_file_name(&dir.dir);
        let blob_path = cache_dir.join(&blob);
        write_atomic(&blob_path, &bytes)
            .with_context(|| format!("writing {}", blob_path.display()))?;
        entries.push(ManifestEntry {
            dir: dir.dir.clone(),
            scanned_at: dir.scanned_at,
            dir_mtime: dir.dir_mtime,
            blob,
        });
    }

    let body = ManifestBody {
        crate_version: env!("CARGO_PKG_VERSION").to_string(),
        ttl_secs,
        dirs: entries,
    };
    let manifest_bytes = try_write_manifest_bytes(&body).context("encoding manifest.bin")?;
    write_atomic(&cache_dir.join(MANIFEST_FILE_NAME), &manifest_bytes)
        .context("writing manifest.bin")?;
    Ok(())
}

/// Loads the cache saved by [`save_cache`], or `None` if `vault_dir` has no
/// usable one: no `manifest.bin`, or one whose header doesn't match
/// [`MAGIC`]/[`SCHEMA_VERSION`] (see [`read_manifest_bytes`]).
///
/// Each manifest entry's blob is loaded independently; a blob that's
/// missing or fails to decode is skipped — its directory is simply absent
/// from the returned `Vec`, and the caller re-scans it — rather than
/// discarding the whole cache over one corrupt file.
pub fn load_cache(vault_dir: &Path) -> Option<(ManifestBody, Vec<CachedDir>)> {
    let cache_dir = vault_dir.join(CACHE_DIR_NAME);
    let manifest_bytes = fs::read(cache_dir.join(MANIFEST_FILE_NAME)).ok()?;
    let body = read_manifest_bytes(&manifest_bytes)?;

    let loaded = body
        .dirs
        .iter()
        .filter_map(|entry| {
            let bytes = fs::read(cache_dir.join(&entry.blob)).ok()?;
            decode::<CachedDir>(&bytes)
        })
        .collect();
    Some((body, loaded))
}

/// Walks `start` upward through its ancestors, returning the canonicalized
/// path of the first one containing a `.querymatter/manifest.bin` — i.e. the
/// vault root a query rooted at `start` should read/write its cache from.
pub fn find_vault(start: &Path) -> Option<PathBuf> {
    let mut dir = start.canonicalize().ok()?;
    loop {
        if dir.join(CACHE_DIR_NAME).join(MANIFEST_FILE_NAME).is_file() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Value;
    use indexmap::IndexMap;
    use std::path::PathBuf;
    use std::time::{Duration, UNIX_EPOCH};
    use tempfile::TempDir;

    fn sample_dir() -> CachedDir {
        let mut f = IndexMap::new();
        f.insert("status".to_string(), Value::Str("draft".into()));
        f.insert(
            "tags".to_string(),
            Value::List(vec![Value::Str("a".into())]),
        );
        CachedDir {
            dir: PathBuf::from("/v/plans"),
            scanned_at: UNIX_EPOCH + Duration::from_secs(1000),
            dir_mtime: UNIX_EPOCH + Duration::from_secs(900),
            files: vec![CachedFile {
                rel_path: "a.md".into(),
                mtime: UNIX_EPOCH + Duration::from_secs(800),
                size: 42,
                fields: f,
            }],
        }
    }

    /// [`sample_dir`] relocated to `dir`, for tests that need real,
    /// distinct filesystem paths (blob names are derived from the path).
    fn sample_dir_at(dir: PathBuf) -> CachedDir {
        CachedDir {
            dir,
            ..sample_dir()
        }
    }
    #[test]
    fn cacheddir_roundtrips_through_bincode() {
        let d = sample_dir();
        let bytes = encode(&d);
        assert_eq!(decode::<CachedDir>(&bytes), Some(d));
    }
    #[test]
    fn manifest_header_roundtrips() {
        let body = ManifestBody {
            crate_version: "0.1.0".into(),
            ttl_secs: 300,
            dirs: vec![],
        };
        let bytes = write_manifest_bytes(&body);
        assert_eq!(&bytes[0..4], &MAGIC);
        assert_eq!(read_manifest_bytes(&bytes), Some(body));
    }
    #[test]
    fn wrong_magic_rejected() {
        let mut bytes = write_manifest_bytes(&ManifestBody {
            crate_version: "x".into(),
            ttl_secs: 1,
            dirs: vec![],
        });
        bytes[0] = b'Z';
        assert_eq!(read_manifest_bytes(&bytes), None);
    }
    #[test]
    fn wrong_version_rejected() {
        let body = ManifestBody {
            crate_version: "x".into(),
            ttl_secs: 1,
            dirs: vec![],
        };
        let mut bytes = write_manifest_bytes(&body);
        bytes[4..8].copy_from_slice(&(SCHEMA_VERSION + 1).to_le_bytes());
        assert_eq!(read_manifest_bytes(&bytes), None);
    }
    #[test]
    fn garbage_rejected() {
        assert_eq!(read_manifest_bytes(b"xx"), None);
        assert_eq!(read_manifest_bytes(&[]), None);
    }

    #[test]
    fn write_atomic_writes_bytes_and_leaves_no_temp_file() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("out.bin");
        write_atomic(&path, b"hello").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"hello");
        let stray_files: Vec<_> = fs::read_dir(td.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name() != path.file_name().unwrap())
            .collect();
        assert!(
            stray_files.is_empty(),
            "leftover temp file: {stray_files:?}"
        );
    }

    #[test]
    fn save_then_load_roundtrips() {
        let td = TempDir::new().unwrap();
        let dirs = vec![sample_dir_at(td.path().join("plans"))];
        save_cache(td.path(), &dirs, 300).unwrap();
        assert!(td.path().join(".querymatter/manifest.bin").is_file());
        let (body, loaded) = load_cache(td.path()).unwrap();
        assert_eq!(body.ttl_secs, 300);
        assert_eq!(loaded, dirs);
    }

    #[test]
    fn missing_manifest_is_none() {
        let td = TempDir::new().unwrap();
        assert!(load_cache(td.path()).is_none());
    }

    #[test]
    fn incompatible_manifest_is_none() {
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join(".querymatter")).unwrap();
        fs::write(
            td.path().join(".querymatter/manifest.bin"),
            b"NOPEnotaversion",
        )
        .unwrap();
        assert!(load_cache(td.path()).is_none());
    }

    #[test]
    fn corrupt_blob_skips_only_that_dir() {
        // Save two dirs, then clobber one blob with garbage; load returns
        // only the other, not an error.
        let td = TempDir::new().unwrap();
        let good = sample_dir_at(td.path().join("plans"));
        let bad = sample_dir_at(td.path().join("notes"));
        save_cache(td.path(), &[good.clone(), bad.clone()], 300).unwrap();

        let (body, _) = load_cache(td.path()).unwrap();
        let bad_entry = body.dirs.iter().find(|e| e.dir == bad.dir).unwrap();
        fs::write(
            td.path().join(".querymatter").join(&bad_entry.blob),
            b"not a valid bincode blob",
        )
        .unwrap();

        let (_, loaded) = load_cache(td.path()).unwrap();
        assert_eq!(loaded, vec![good]);
    }

    #[test]
    fn pre_epoch_mtime_is_skipped_not_panicked() {
        // A `CachedDir` whose file mtime predates the Unix epoch (clock
        // skew, `touch -d`, archive extraction) must be skipped, not panic
        // the whole save (carry-forward fix from Task 1's review).
        let td = TempDir::new().unwrap();
        let good = sample_dir_at(td.path().join("plans"));
        let mut ancient = sample_dir_at(td.path().join("ancient"));
        ancient.files[0].mtime = UNIX_EPOCH.checked_sub(Duration::from_secs(1)).unwrap();

        save_cache(td.path(), &[good.clone(), ancient], 300).unwrap();

        let (_, loaded) = load_cache(td.path()).unwrap();
        assert_eq!(loaded, vec![good]);
    }

    #[test]
    fn find_vault_finds_ancestor() {
        let td = TempDir::new().unwrap();
        save_cache(td.path(), &[], 300).unwrap();
        let deep = td.path().join("a/b/c");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(
            find_vault(&deep),
            Some(fs::canonicalize(td.path()).unwrap())
        );

        let other = TempDir::new().unwrap();
        assert_eq!(find_vault(other.path()), None);
    }
}
