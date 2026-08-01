//! Shared BLAKE3 hashing and file fingerprint helpers.
//!
//! Fingerprints use file size and modification time to identify unchanged files.
//! Hash helpers verify that a file's fingerprint remains stable while its contents are read.

use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::Context;

/// File metadata used to determine whether cached analysis is still valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFingerprint {
    /// File size in bytes.
    pub size_bytes: u64,
    /// Modification time as nanoseconds since the Unix epoch.
    pub modified_time_ns: i64,
}

/// Read the metadata fingerprint for a file.
///
/// # Errors
/// Returns an error when file metadata or its modification time cannot be read.
pub fn fingerprint_file(path: &Path) -> anyhow::Result<FileFingerprint> {
    let metadata =
        std::fs::metadata(path).with_context(|| format!("Failed to read metadata for {}", path.display()))?;
    let modified = metadata
        .modified()
        .with_context(|| format!("Failed to read modification time for {}", path.display()))?;
    let modified_time_ns = modified
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX));

    Ok(FileFingerprint {
        size_bytes: metadata.len(),
        modified_time_ns,
    })
}

/// Calculate the BLAKE3 hash for a file.
///
/// # Errors
/// Returns an error when the file cannot be read.
pub fn hash_file(path: &Path) -> anyhow::Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    hasher
        .update_mmap_rayon(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    Ok(hasher.finalize())
}

/// Calculate a BLAKE3 hash and reject it if the file changed while being read.
///
/// # Errors
/// Returns an error when the file cannot be read or its fingerprint changes during hashing.
pub fn hash_file_if_unchanged(path: &Path, expected: FileFingerprint) -> anyhow::Result<blake3::Hash> {
    let hash = hash_file(path)?;
    let current = fingerprint_file(path)?;
    if current != expected {
        anyhow::bail!("{} changed while it was being hashed", path.display());
    }
    Ok(hash)
}

#[cfg(test)]
mod test_file_hash {
    use super::*;

    #[test]
    fn fingerprints_and_hashes_file_contents() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        let path = temp_directory.path().join("sample.bin");
        std::fs::write(&path, b"sample contents").expect("should write sample file");

        let fingerprint = fingerprint_file(&path).expect("should fingerprint file");
        let hash = hash_file_if_unchanged(&path, fingerprint).expect("should hash unchanged file");

        assert_eq!(fingerprint.size_bytes, 15);
        assert_eq!(hash, blake3::hash(b"sample contents"));
    }

    #[test]
    fn rejects_stale_expected_fingerprint() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        let path = temp_directory.path().join("sample.bin");
        std::fs::write(&path, b"sample contents").expect("should write sample file");
        let mut stale_fingerprint = fingerprint_file(&path).expect("should fingerprint file");
        stale_fingerprint.size_bytes += 1;

        let result = hash_file_if_unchanged(&path, stale_fingerprint);

        assert!(result.is_err());
    }

    #[test]
    fn different_contents_have_different_hashes() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        let first_path = temp_directory.path().join("first.bin");
        let second_path = temp_directory.path().join("second.bin");
        std::fs::write(&first_path, b"first").expect("should write first file");
        std::fs::write(&second_path, b"other").expect("should write second file");

        assert_ne!(
            hash_file(&first_path).expect("should hash first file"),
            hash_file(&second_path).expect("should hash second file")
        );
    }
}
