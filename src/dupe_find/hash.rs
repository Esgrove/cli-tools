//! Hash candidate discovery, calculation, and exact content grouping.

use std::collections::HashMap;
use std::path::PathBuf;

use itertools::Itertools;

use crate::file_hash::{FileFingerprint, fingerprint_file, hash_file_if_unchanged};

use super::DupeFileInfo;

/// A file that needs a BLAKE3 hash for exact content comparison.
#[derive(Debug, Clone)]
pub struct HashCandidate {
    /// Index of the file in the caller's input collection.
    pub index: usize,
    /// Path to the file.
    pub path: PathBuf,
    /// Metadata recorded before hashing.
    pub fingerprint: FileFingerprint,
}

/// A successfully calculated file hash.
#[derive(Debug)]
pub struct CalculatedFileHash {
    /// Index of the file in the caller's input collection.
    pub index: usize,
    /// Path to the file.
    pub path: PathBuf,
    /// Metadata that identifies the hashed file version.
    pub fingerprint: FileFingerprint,
    /// BLAKE3 hash of the file contents.
    pub hash: blake3::Hash,
}

/// Hash value used when grouping files with identical contents.
#[derive(Debug, Clone)]
pub struct IndexedFileHash {
    /// Index of the file in the caller's input collection.
    pub index: usize,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Lowercase hexadecimal BLAKE3 hash.
    pub blake3_hash: String,
}

/// Candidate collection results, including files that could not be inspected.
#[derive(Debug, Default)]
pub struct HashCandidateCollection {
    /// Files in size groups containing at least two candidates.
    pub candidates: Vec<HashCandidate>,
    /// Metadata errors paired with their file paths.
    pub errors: Vec<(PathBuf, anyhow::Error)>,
}

impl CalculatedFileHash {
    /// Convert this calculated hash into a value suitable for duplicate grouping.
    #[must_use]
    pub fn indexed_hash(&self) -> IndexedFileHash {
        IndexedFileHash {
            index: self.index,
            size_bytes: self.fingerprint.size_bytes,
            blake3_hash: self.hash.to_string(),
        }
    }
}

/// Find files that need hashing, skipping sizes represented by only one file.
#[must_use]
pub fn collect_hash_candidates(files: &[DupeFileInfo]) -> HashCandidateCollection {
    let mut candidates_by_size: HashMap<u64, Vec<HashCandidate>> = HashMap::new();
    let mut errors = Vec::new();

    for (index, file) in files.iter().enumerate() {
        match fingerprint_file(&file.path) {
            Ok(fingerprint) => candidates_by_size
                .entry(fingerprint.size_bytes)
                .or_default()
                .push(HashCandidate {
                    index,
                    path: file.path.clone(),
                    fingerprint,
                }),
            Err(error) => errors.push((file.path.clone(), error)),
        }
    }

    let candidates = candidates_by_size
        .into_values()
        .filter(|same_size| same_size.len() > 1)
        .flatten()
        .collect();

    HashCandidateCollection { candidates, errors }
}

/// Calculate a candidate's hash and reject files changed during the read.
///
/// # Errors
/// Returns an error when the file cannot be read or changes while being hashed.
pub fn calculate_file_hash(candidate: &HashCandidate) -> anyhow::Result<CalculatedFileHash> {
    let hash = hash_file_if_unchanged(&candidate.path, candidate.fingerprint)?;
    Ok(CalculatedFileHash {
        index: candidate.index,
        path: candidate.path.clone(),
        fingerprint: candidate.fingerprint,
        hash,
    })
}

/// Group file indices by equal size and BLAKE3 hash.
#[must_use]
pub fn group_hash_matches(hash_values: &[IndexedFileHash]) -> Vec<Vec<usize>> {
    hash_values
        .iter()
        .map(|value| ((value.size_bytes, value.blake3_hash.as_str()), value.index))
        .into_group_map()
        .into_values()
        .filter(|indices| indices.len() > 1)
        .map(|indices| indices.into_iter().sorted_unstable().collect())
        .collect()
}

#[cfg(test)]
mod test_hash_candidates {
    use super::*;

    #[test]
    fn only_returns_candidates_that_share_a_size() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        let first_path = temp_directory.path().join("first.mp4");
        let second_path = temp_directory.path().join("second.mp4");
        let unique_path = temp_directory.path().join("unique.mp4");
        std::fs::write(&first_path, b"same bytes").expect("should write first file");
        std::fs::write(&second_path, b"same bytes").expect("should write second file");
        std::fs::write(&unique_path, b"unique").expect("should write unique file");
        let files = vec![
            DupeFileInfo::new(first_path, "mp4".to_string()),
            DupeFileInfo::new(second_path, "mp4".to_string()),
            DupeFileInfo::new(unique_path, "mp4".to_string()),
        ];

        let collection = collect_hash_candidates(&files);

        assert!(collection.errors.is_empty());
        assert_eq!(collection.candidates.len(), 2);
        assert_eq!(collection.candidates[0].index, 0);
        assert_eq!(collection.candidates[1].index, 1);
    }

    #[test]
    fn calculates_and_groups_identical_hashes_in_file_order() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        let first_path = temp_directory.path().join("first.mp4");
        let second_path = temp_directory.path().join("second.mp4");
        std::fs::write(&first_path, b"same bytes").expect("should write first file");
        std::fs::write(&second_path, b"same bytes").expect("should write second file");
        let files = vec![
            DupeFileInfo::new(first_path, "mp4".to_string()),
            DupeFileInfo::new(second_path, "mp4".to_string()),
        ];
        let candidates = collect_hash_candidates(&files).candidates;
        let hashes: Vec<IndexedFileHash> = candidates
            .iter()
            .map(|candidate| calculate_file_hash(candidate).expect("should hash file").indexed_hash())
            .collect();

        let reversed_hashes = vec![hashes[1].clone(), hashes[0].clone()];

        assert_eq!(group_hash_matches(&reversed_hashes), vec![vec![0, 1]]);
    }
}
