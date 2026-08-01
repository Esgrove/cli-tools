//! Integration tests for duplicate discovery through public hashing APIs.

use cli_tools::dupe_find::DupeFileInfo;
use cli_tools::dupe_find::hash::{calculate_file_hash, collect_hash_candidates, group_hash_matches};

fn file_info(path: std::path::PathBuf) -> DupeFileInfo {
    DupeFileInfo::new(path, "mp4".to_string())
}

#[test]
fn groups_identical_contents_with_different_filenames() -> anyhow::Result<()> {
    let temp_directory = tempfile::TempDir::new()?;
    let first_path = temp_directory.path().join("first-name.mp4");
    let second_path = temp_directory.path().join("unrelated-name.mp4");
    let different_path = temp_directory.path().join("different.mp4");
    std::fs::write(&first_path, b"same bytes")?;
    std::fs::write(&second_path, b"same bytes")?;
    std::fs::write(&different_path, b"other data")?;
    let files = vec![file_info(first_path), file_info(second_path), file_info(different_path)];

    let collection = collect_hash_candidates(&files);
    let hashes = collection
        .candidates
        .iter()
        .map(calculate_file_hash)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let indexed_hashes = hashes
        .iter()
        .map(cli_tools::dupe_find::hash::CalculatedFileHash::indexed_hash)
        .collect::<Vec<_>>();

    assert!(collection.errors.is_empty());
    assert_eq!(group_hash_matches(&indexed_hashes), vec![vec![0, 1]]);
    Ok(())
}

#[test]
fn keeps_valid_candidates_when_another_path_is_missing() -> anyhow::Result<()> {
    let temp_directory = tempfile::TempDir::new()?;
    let first_path = temp_directory.path().join("first.mp4");
    let missing_path = temp_directory.path().join("missing.mp4");
    let second_path = temp_directory.path().join("second.mp4");
    std::fs::write(&first_path, b"same bytes")?;
    std::fs::write(&second_path, b"same bytes")?;
    let files = vec![
        file_info(first_path),
        file_info(missing_path.clone()),
        file_info(second_path),
    ];

    let collection = collect_hash_candidates(&files);
    let candidate_indices = collection
        .candidates
        .iter()
        .map(|candidate| candidate.index)
        .collect::<Vec<_>>();

    assert_eq!(candidate_indices, vec![0, 2]);
    assert_eq!(collection.errors.len(), 1);
    assert_eq!(collection.errors[0].0, missing_path);
    Ok(())
}

#[test]
fn rejects_a_candidate_changed_after_collection() -> anyhow::Result<()> {
    let temp_directory = tempfile::TempDir::new()?;
    let first_path = temp_directory.path().join("first.mp4");
    let second_path = temp_directory.path().join("second.mp4");
    std::fs::write(&first_path, b"same bytes")?;
    std::fs::write(&second_path, b"same bytes")?;
    let files = vec![file_info(first_path.clone()), file_info(second_path)];
    let collection = collect_hash_candidates(&files);
    let first_candidate = collection
        .candidates
        .iter()
        .find(|candidate| candidate.index == 0)
        .expect("first file should be a hash candidate");
    std::fs::write(&first_path, b"changed and longer")?;

    let error = calculate_file_hash(first_candidate).expect_err("changed candidate should be rejected");

    assert!(error.to_string().contains("changed while it was being hashed"));
    Ok(())
}
