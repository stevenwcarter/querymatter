//! The `.querymatter/` on-disk cache data model.
//!
//! This module owns the *shape* of the persistent cache and its bincode
//! (de)serialization — a `CachedDir` blob per scanned filesystem directory,
//! plus a versioned `manifest.bin` header that lets a future format change
//! be detected and safely discarded rather than mis-decoded. Nothing here
//! touches the filesystem; reading/writing the actual files, freshness
//! checks, and vault discovery are later phases of the cache-vault feature.

use indexmap::IndexMap;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::model::Value;

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
/// Only if `value` contains a non-UTF-8 `PathBuf`/`OsString` (serde's `Path`
/// impl errors in that case) — a documented, accepted limitation of the
/// cache (design spec §9). Every other field type this crate stores
/// (numbers, `String`, `SystemTime`, collections) encodes infallibly.
pub fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    bincode::serde::encode_to_vec(value, bincode::config::standard())
        .expect("non-UTF-8 paths are unsupported in the cache (design spec §9)")
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
    let mut bytes = Vec::with_capacity(8);
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
    bytes.extend_from_slice(&encode(body));
    bytes
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Value;
    use indexmap::IndexMap;
    use std::path::PathBuf;
    use std::time::{Duration, UNIX_EPOCH};

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
}
