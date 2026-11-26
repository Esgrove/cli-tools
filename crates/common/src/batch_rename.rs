//! Two-phase batch renaming that handles chains and swap cycles safely.
//!
//! When a batch of renames contains chains (`a -> b`, `b -> c`) or cycles (`a <-> b`),
//! naive sequential renaming fails with false collisions or overwrites inputs before they move.
//! `BatchRenamer` stages colliding sources into temporary sibling paths first,
//! moves them into their final targets in a second phase, and rolls back staged files if phase one fails.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::file_handling::{OverwritePolicy, RenameOutcome, safe_rename, unique_sibling_path};
use crate::into_nfc;

/// Outcome status of one entry processed by [`BatchRenamer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchRenameStatus {
    /// Entry was moved to its target path.
    Renamed,
    /// Target already existed outside the batch and policy was [`OverwritePolicy::Skip`].
    SkippedExisting,
    /// Source and target referred to the same path and spelling.
    Unchanged,
}

/// One planned rename operation in a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchRenameEntry {
    /// Original path on disk.
    pub source: PathBuf,
    /// Desired destination path.
    pub target: PathBuf,
}

/// Result for one entry processed by [`BatchRenamer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchRenameOutcome {
    /// Original path.
    pub source: PathBuf,
    /// Requested destination path.
    pub target: PathBuf,
    /// Whether the entry was renamed, skipped, or unchanged.
    pub status: BatchRenameStatus,
}

/// Internal tracking for a staged entry during two-phase execution.
struct StagedEntry {
    index: usize,
    original_source: PathBuf,
    temporary_path: PathBuf,
    target: PathBuf,
}

/// Executes a batch of renames safely across chains, cycles, and case-only changes.
#[derive(Debug, Clone, Copy)]
pub struct BatchRenamer {
    policy: OverwritePolicy,
}

impl BatchRenameEntry {
    /// Construct a new batch rename entry.
    #[must_use]
    pub const fn new(source: PathBuf, target: PathBuf) -> Self {
        Self { source, target }
    }
}

impl BatchRenamer {
    /// Create a batch renamer with the given overwrite policy for pre-existing external targets.
    #[must_use]
    pub const fn new(policy: OverwritePolicy) -> Self {
        Self { policy }
    }

    /// Execute all planned renames in two phases so chains and swap cycles succeed without clobbering.
    ///
    /// # Errors
    /// Returns an error if a required source is missing, an external target conflicts under
    /// [`OverwritePolicy::Error`], or a filesystem rename fails.
    pub fn execute(&self, entries: &[BatchRenameEntry]) -> io::Result<Vec<BatchRenameOutcome>> {
        let mut outcomes: Vec<Option<BatchRenameOutcome>> = vec![None; entries.len()];
        let mut active_indices: Vec<usize> = Vec::with_capacity(entries.len());

        // Classify identical paths and collect normalized source keys for active entries.
        let mut source_keys: HashSet<String> = HashSet::with_capacity(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            if entry.source == entry.target {
                let _ = fs::symlink_metadata(&entry.source)?;
                if let Some(slot) = outcomes.get_mut(index) {
                    *slot = Some(BatchRenameOutcome {
                        source: entry.source.clone(),
                        target: entry.target.clone(),
                        status: BatchRenameStatus::Unchanged,
                    });
                }
                continue;
            }
            source_keys.insert(path_key(&entry.source));
            active_indices.push(index);
        }

        // Filter out entries whose external targets already exist when policy is Skip or Error.
        let mut planned_indices: Vec<usize> = Vec::with_capacity(active_indices.len());
        for index in active_indices {
            let Some(entry) = entries.get(index) else {
                continue;
            };
            let target_is_batch_source = source_keys.contains(&path_key(&entry.target));
            if !target_is_batch_source && fs::symlink_metadata(&entry.target).is_ok() {
                match self.policy {
                    OverwritePolicy::Skip => {
                        source_keys.remove(&path_key(&entry.source));
                        if let Some(slot) = outcomes.get_mut(index) {
                            *slot = Some(BatchRenameOutcome {
                                source: entry.source.clone(),
                                target: entry.target.clone(),
                                status: BatchRenameStatus::SkippedExisting,
                            });
                        }
                        continue;
                    }
                    OverwritePolicy::Error => {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            format!("Target already exists: {}", entry.target.display()),
                        ));
                    }
                    OverwritePolicy::Overwrite => {}
                }
            }
            planned_indices.push(index);
        }

        let staged = Self::stage_entries(entries, &planned_indices)?;
        self.commit_staged_entries(&staged, &mut outcomes)?;

        Ok(outcomes.into_iter().flatten().collect())
    }

    /// Stage every planned entry into a unique temporary sibling path, rolling back on any failure.
    fn stage_entries(entries: &[BatchRenameEntry], planned_indices: &[usize]) -> io::Result<Vec<StagedEntry>> {
        let mut staged: Vec<StagedEntry> = Vec::with_capacity(planned_indices.len());
        for &index in planned_indices {
            let Some(entry) = entries.get(index) else {
                continue;
            };
            let temporary_path = unique_sibling_path(&entry.source, "batch-rename-tmp");
            if let Err(error) = fs::rename(&entry.source, &temporary_path) {
                rollback_staged(&staged);
                return Err(error);
            }
            staged.push(StagedEntry {
                index,
                original_source: entry.source.clone(),
                temporary_path,
                target: entry.target.clone(),
            });
        }
        Ok(staged)
    }

    /// Move each staged temporary file to its final destination, rolling back remaining entries on error.
    fn commit_staged_entries(
        self,
        staged: &[StagedEntry],
        outcomes: &mut [Option<BatchRenameOutcome>],
    ) -> io::Result<()> {
        for (position, item) in staged.iter().enumerate() {
            match safe_rename(&item.temporary_path, &item.target, self.policy) {
                Ok(RenameOutcome::Renamed | RenameOutcome::CaseRenamed) => {
                    if let Some(slot) = outcomes.get_mut(item.index) {
                        *slot = Some(BatchRenameOutcome {
                            source: item.original_source.clone(),
                            target: item.target.clone(),
                            status: BatchRenameStatus::Renamed,
                        });
                    }
                }
                Ok(RenameOutcome::SkippedExisting) => {
                    // Restore original source when a concurrent file appeared at target.
                    let _ = fs::rename(&item.temporary_path, &item.original_source);
                    if let Some(slot) = outcomes.get_mut(item.index) {
                        *slot = Some(BatchRenameOutcome {
                            source: item.original_source.clone(),
                            target: item.target.clone(),
                            status: BatchRenameStatus::SkippedExisting,
                        });
                    }
                }
                Ok(RenameOutcome::Unchanged) => {
                    if let Some(slot) = outcomes.get_mut(item.index) {
                        *slot = Some(BatchRenameOutcome {
                            source: item.original_source.clone(),
                            target: item.target.clone(),
                            status: BatchRenameStatus::Unchanged,
                        });
                    }
                }
                Err(error) => {
                    if let Some(remaining) = staged.get(position..) {
                        rollback_staged(remaining);
                    }
                    return Err(error);
                }
            }
        }
        Ok(())
    }
}

/// Restore staged temporary files back to their original source paths in reverse order.
fn rollback_staged(staged: &[StagedEntry]) {
    for item in staged.iter().rev() {
        let _ = fs::rename(&item.temporary_path, &item.original_source);
    }
}

/// Normalized case-folded comparison key for matching source and target paths within one batch.
fn path_key(path: &Path) -> String {
    into_nfc(path.to_string_lossy().into_owned()).to_lowercase()
}

#[cfg(test)]
mod test_batch_renamer {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn swaps_two_files_in_a_cycle_without_data_loss() {
        let dir = TempDir::new().unwrap();
        let first = dir.path().join("a.txt");
        let second = dir.path().join("b.txt");
        fs::write(&first, "from-a").unwrap();
        fs::write(&second, "from-b").unwrap();

        let entries = vec![
            BatchRenameEntry::new(first.clone(), second.clone()),
            BatchRenameEntry::new(second.clone(), first.clone()),
        ];
        let outcomes = BatchRenamer::new(OverwritePolicy::Skip).execute(&entries).unwrap();

        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().all(|item| item.status == BatchRenameStatus::Renamed));
        assert_eq!(fs::read_to_string(&first).unwrap(), "from-b");
        assert_eq!(fs::read_to_string(&second).unwrap(), "from-a");
    }

    #[test]
    fn executes_rename_chain_in_any_order() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        let c = dir.path().join("c.txt");
        fs::write(&a, "one").unwrap();
        fs::write(&b, "two").unwrap();

        // Listed forward (a -> b before b -> c), which would fail without staging.
        let entries = vec![
            BatchRenameEntry::new(a.clone(), b.clone()),
            BatchRenameEntry::new(b.clone(), c.clone()),
        ];
        let outcomes = BatchRenamer::new(OverwritePolicy::Skip).execute(&entries).unwrap();

        assert!(outcomes.iter().all(|item| item.status == BatchRenameStatus::Renamed));
        assert!(!a.exists());
        assert_eq!(fs::read_to_string(&b).unwrap(), "one");
        assert_eq!(fs::read_to_string(&c).unwrap(), "two");
    }

    #[test]
    fn rolls_back_earlier_staged_entries_when_a_later_source_is_missing() {
        let dir = TempDir::new().unwrap();
        let existing = dir.path().join("a.txt");
        let missing = dir.path().join("missing.txt");
        let target_a = dir.path().join("a_new.txt");
        let target_missing = dir.path().join("m_new.txt");
        fs::write(&existing, "intact").unwrap();

        let entries = vec![
            BatchRenameEntry::new(existing.clone(), target_a),
            BatchRenameEntry::new(missing, target_missing),
        ];
        let err = BatchRenamer::new(OverwritePolicy::Skip)
            .execute(&entries)
            .expect_err("missing source should fail");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(fs::read_to_string(&existing).unwrap(), "intact");
    }
}
