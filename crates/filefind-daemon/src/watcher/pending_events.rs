//! Debouncing of `notify` file system events into index changes, including rename pairing.
//!
//! Renames arrive differently per platform.
//! Linux reports one event with both paths, Windows reports the old name and the new name as two events,
//! and macOS reports each affected path without saying which side it is.
//! This module turns all of them into [`FileChangeEvent::Renamed`] when both sides are known,
//! and into a delete or a create of the existing side otherwise,
//! so watched network and non-NTFS drives follow renames like the USN journal does for NTFS drives.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use notify::event::{ModifyKind, RenameMode};
use notify::{Event, EventKind};

use filefind_common::IndexScope;
use filefind_common::scope::path_to_utf8;
use filefind_common::types::FileChangeEvent;
use tracing::warn;

/// A pending debounced file change paired with when it was first seen.
#[derive(Debug)]
struct PendingChange {
    /// Latest coalesced change event for the path.
    event: FileChangeEvent,
    /// Timestamp when this path first entered the debounce window.
    first_seen: Instant,
}

/// Old name of an unpaired rename waiting for its destination event.
#[derive(Debug)]
struct PendingRenameSource {
    /// Original path before the rename.
    path: PathBuf,
    /// Timestamp when the old-name event was observed.
    seen_at: Instant,
}

/// Changes waiting for their debounce period, keyed by the path they apply to.
#[derive(Debug, Default)]
pub(super) struct PendingEvents {
    /// The latest change for each path, with the time it was first seen.
    changes: HashMap<PathBuf, PendingChange>,
    /// Old names of renames whose new name has not arrived yet, oldest first.
    rename_sources: VecDeque<PendingRenameSource>,
}

impl PendingEvents {
    /// Create an empty set of pending events.
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Merge one `notify` event into the pending changes, skipping excluded or non-UTF-8 paths.
    pub(super) fn ingest_with_scope(&mut self, event: Event, scope: &IndexScope, now: Instant) {
        let is_excluded = |path: &PathBuf| {
            let Ok(path_str) = path_to_utf8(path) else {
                warn!("Skipping non-UTF-8 watcher path: {}", path.display());
                return true;
            };
            scope.is_excluded(path_str, scope.most_specific_root(path_str))
        };

        match event.kind {
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
                let mut paths = event.paths.into_iter();
                if let (Some(from), Some(to)) = (paths.next(), paths.next()) {
                    match (is_excluded(&from), is_excluded(&to)) {
                        (false, false) => self.rename(from, to, now),
                        (false, true) => self.insert(from.clone(), FileChangeEvent::Deleted(from), now),
                        (true, false) => self.insert(to.clone(), FileChangeEvent::Created(to), now),
                        (true, true) => {}
                    }
                }
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                for path in event.paths {
                    if !is_excluded(&path) {
                        self.rename_sources
                            .push_back(PendingRenameSource { path, seen_at: now });
                    }
                }
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                for path in event.paths {
                    let source = self.rename_sources.pop_front().map(|pending| pending.path);
                    match (source, is_excluded(&path)) {
                        (Some(source), false) => self.rename(source, path, now),
                        (Some(source), true) => self.insert(source.clone(), FileChangeEvent::Deleted(source), now),
                        (None, false) => self.insert(path.clone(), FileChangeEvent::Created(path), now),
                        (None, true) => {}
                    }
                }
            }
            EventKind::Modify(ModifyKind::Name(_)) => {
                // The platform does not say which side of the rename a path is,
                // so the path that still exists is the new name and a missing one is the old name.
                for path in event.paths {
                    if is_excluded(&path) {
                        continue;
                    }
                    let change = if path.exists() {
                        FileChangeEvent::Created(path.clone())
                    } else {
                        FileChangeEvent::Deleted(path.clone())
                    };
                    self.insert(path, change, now);
                }
            }
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {
                for path in event.paths {
                    if is_excluded(&path) {
                        continue;
                    }
                    let change = match event.kind {
                        EventKind::Create(_) => FileChangeEvent::Created(path.clone()),
                        EventKind::Remove(_) => FileChangeEvent::Deleted(path.clone()),
                        _ => FileChangeEvent::Modified(path.clone()),
                    };
                    self.insert(path, change, now);
                }
            }
            _ => {}
        }
    }

    /// Test helper that ingests an event using only directory exclusion rules.
    #[cfg(test)]
    pub(super) fn ingest(&mut self, event: Event, exclude_patterns: &[String], now: Instant) {
        let scope = IndexScope::new(Vec::new(), exclude_patterns.to_vec(), Vec::new());
        self.ingest_with_scope(event, &scope, now);
    }

    /// Remove and return the changes that have waited at least `debounce`.
    ///
    /// A rename whose new name never arrived within the debounce period becomes a delete of the old name.
    pub(super) fn take_due(&mut self, now: Instant, debounce: Duration) -> Vec<FileChangeEvent> {
        let mut due = Vec::new();
        while let Some(pending) = self.rename_sources.front() {
            if now.duration_since(pending.seen_at) < debounce {
                break;
            }
            due.push(FileChangeEvent::Deleted(pending.path.clone()));
            self.rename_sources.pop_front();
        }
        self.changes.retain(|_path, pending| {
            if now.duration_since(pending.first_seen) >= debounce {
                due.push(pending.event.clone());
                false
            } else {
                true
            }
        });
        due
    }

    /// Remove and return every pending change, for example when the watcher stops.
    pub(super) fn take_all(&mut self) -> Vec<FileChangeEvent> {
        let mut all: Vec<FileChangeEvent> = self
            .rename_sources
            .drain(..)
            .map(|pending| FileChangeEvent::Deleted(pending.path))
            .collect();
        all.extend(self.changes.drain().map(|(_, pending)| pending.event));
        all
    }

    /// Record a rename, folding it into any pending change of the old path.
    fn rename(&mut self, from: PathBuf, to: PathBuf, now: Instant) {
        let event = match self.changes.remove(&from).map(|pending| pending.event) {
            // A file created and renamed within the debounce period is simply created at its new name.
            Some(FileChangeEvent::Created(_)) => FileChangeEvent::Created(to.clone()),
            // A chain of renames keeps the original old name, which is the one in the index.
            Some(FileChangeEvent::Renamed { from: original, .. }) => FileChangeEvent::Renamed {
                from: original,
                to: to.clone(),
            },
            _ => FileChangeEvent::Renamed { from, to: to.clone() },
        };
        self.changes.insert(to, PendingChange { event, first_seen: now });
    }

    /// Record a change for `path`, keeping a pending create, rename, or delete when a stale modify arrives afterwards.
    fn insert(&mut self, path: PathBuf, change: FileChangeEvent, now: Instant) {
        self.changes
            .entry(path)
            .and_modify(|pending| {
                let keeps_existing = match (&pending.event, &change) {
                    (FileChangeEvent::Created(_) | FileChangeEvent::Renamed { .. }, FileChangeEvent::Modified(_)) => {
                        true
                    }
                    (FileChangeEvent::Deleted(_), FileChangeEvent::Modified(modified_path)) => {
                        std::fs::symlink_metadata(modified_path).is_err()
                    }
                    _ => false,
                };
                if !keeps_existing {
                    pending.event = change.clone();
                }
            })
            .or_insert(PendingChange {
                event: change,
                first_seen: now,
            });
    }
}

#[cfg(test)]
mod test_pending_events {
    use std::path::Path;

    use notify::event::{CreateKind, DataChange, RemoveKind};

    use super::*;

    const DEBOUNCE: Duration = Duration::from_millis(100);

    fn event(kind: EventKind, paths: &[&str]) -> Event {
        paths
            .iter()
            .fold(Event::new(kind), |event, path| event.add_path(PathBuf::from(path)))
    }

    fn rename(mode: RenameMode, paths: &[&str]) -> Event {
        event(EventKind::Modify(ModifyKind::Name(mode)), paths)
    }

    fn renamed(from: &str, to: &str) -> FileChangeEvent {
        FileChangeEvent::Renamed {
            from: PathBuf::from(from),
            to: PathBuf::from(to),
        }
    }

    fn describe(changes: &[FileChangeEvent]) -> Vec<String> {
        let mut described: Vec<String> = changes.iter().map(ToString::to_string).collect();
        described.sort();
        described
    }

    fn take_after_debounce(pending: &mut PendingEvents, start: Instant) -> Vec<String> {
        describe(&pending.take_due(start + DEBOUNCE, DEBOUNCE))
    }

    #[test]
    fn rename_with_both_paths_becomes_a_rename() {
        let mut pending = PendingEvents::new();
        let start = Instant::now();

        pending.ingest(rename(RenameMode::Both, &["old.txt", "new.txt"]), &[], start);

        assert_eq!(
            take_after_debounce(&mut pending, start),
            describe(&[renamed("old.txt", "new.txt")])
        );
    }

    #[test]
    fn separate_old_and_new_names_pair_into_a_rename() {
        let mut pending = PendingEvents::new();
        let start = Instant::now();

        pending.ingest(rename(RenameMode::From, &["old.txt"]), &[], start);
        pending.ingest(rename(RenameMode::To, &["new.txt"]), &[], start);

        assert_eq!(
            take_after_debounce(&mut pending, start),
            describe(&[renamed("old.txt", "new.txt")])
        );
    }

    #[test]
    fn consecutive_renames_pair_in_order() {
        let mut pending = PendingEvents::new();
        let start = Instant::now();

        pending.ingest(rename(RenameMode::From, &["first.txt"]), &[], start);
        pending.ingest(rename(RenameMode::To, &["first renamed.txt"]), &[], start);
        pending.ingest(rename(RenameMode::From, &["second.txt"]), &[], start);
        pending.ingest(rename(RenameMode::To, &["second renamed.txt"]), &[], start);

        assert_eq!(
            take_after_debounce(&mut pending, start),
            describe(&[
                renamed("first.txt", "first renamed.txt"),
                renamed("second.txt", "second renamed.txt"),
            ])
        );
    }

    #[test]
    fn old_name_without_a_new_name_becomes_a_delete() {
        let mut pending = PendingEvents::new();
        let start = Instant::now();

        pending.ingest(rename(RenameMode::From, &["moved away.txt"]), &[], start);

        assert_eq!(
            take_after_debounce(&mut pending, start),
            describe(&[FileChangeEvent::Deleted(PathBuf::from("moved away.txt"))])
        );
    }

    #[test]
    fn new_name_without_an_old_name_becomes_a_create() {
        let mut pending = PendingEvents::new();
        let start = Instant::now();

        pending.ingest(rename(RenameMode::To, &["moved in.txt"]), &[], start);

        assert_eq!(
            take_after_debounce(&mut pending, start),
            describe(&[FileChangeEvent::Created(PathBuf::from("moved in.txt"))])
        );
    }

    #[test]
    fn created_then_renamed_file_is_created_at_the_new_name() {
        let mut pending = PendingEvents::new();
        let start = Instant::now();

        pending.ingest(
            event(EventKind::Create(CreateKind::File), &["download.part"]),
            &[],
            start,
        );
        pending.ingest(rename(RenameMode::Both, &["download.part", "video.mkv"]), &[], start);

        assert_eq!(
            take_after_debounce(&mut pending, start),
            describe(&[FileChangeEvent::Created(PathBuf::from("video.mkv"))])
        );
    }

    #[test]
    fn chained_renames_keep_the_original_old_name() {
        let mut pending = PendingEvents::new();
        let start = Instant::now();

        pending.ingest(rename(RenameMode::Both, &["a.txt", "b.txt"]), &[], start);
        pending.ingest(rename(RenameMode::Both, &["b.txt", "c.txt"]), &[], start);

        assert_eq!(
            take_after_debounce(&mut pending, start),
            describe(&[renamed("a.txt", "c.txt")])
        );
    }

    #[test]
    fn modification_after_a_rename_keeps_the_rename() {
        let mut pending = PendingEvents::new();
        let start = Instant::now();

        pending.ingest(rename(RenameMode::Both, &["old.txt", "new.txt"]), &[], start);
        pending.ingest(
            event(EventKind::Modify(ModifyKind::Data(DataChange::Content)), &["new.txt"]),
            &[],
            start,
        );

        assert_eq!(
            take_after_debounce(&mut pending, start),
            describe(&[renamed("old.txt", "new.txt")])
        );
    }

    #[test]
    fn created_then_modified_file_stays_created() {
        let mut pending = PendingEvents::new();
        let start = Instant::now();

        pending.ingest(event(EventKind::Create(CreateKind::File), &["new.txt"]), &[], start);
        pending.ingest(
            event(EventKind::Modify(ModifyKind::Data(DataChange::Content)), &["new.txt"]),
            &[],
            start,
        );

        assert_eq!(
            take_after_debounce(&mut pending, start),
            describe(&[FileChangeEvent::Created(PathBuf::from("new.txt"))])
        );
    }

    #[test]
    fn removed_file_becomes_a_delete() {
        let mut pending = PendingEvents::new();
        let start = Instant::now();

        pending.ingest(event(EventKind::Remove(RemoveKind::File), &["gone.txt"]), &[], start);

        assert_eq!(
            take_after_debounce(&mut pending, start),
            describe(&[FileChangeEvent::Deleted(PathBuf::from("gone.txt"))])
        );
    }

    #[test]
    fn rename_into_an_excluded_name_becomes_a_delete() {
        let mut pending = PendingEvents::new();
        let start = Instant::now();
        let exclude = vec!["*.tmp".to_string()];

        pending.ingest(rename(RenameMode::Both, &["data.txt", "data.tmp"]), &exclude, start);

        assert_eq!(
            take_after_debounce(&mut pending, start),
            describe(&[FileChangeEvent::Deleted(PathBuf::from("data.txt"))])
        );
    }

    #[test]
    fn rename_out_of_an_excluded_name_becomes_a_create() {
        let mut pending = PendingEvents::new();
        let start = Instant::now();
        let exclude = vec!["*.tmp".to_string()];

        pending.ingest(rename(RenameMode::From, &["data.tmp"]), &exclude, start);
        pending.ingest(rename(RenameMode::To, &["data.txt"]), &exclude, start);

        assert_eq!(
            take_after_debounce(&mut pending, start),
            describe(&[FileChangeEvent::Created(PathBuf::from("data.txt"))])
        );
    }

    #[test]
    fn rename_with_unknown_side_uses_whether_the_path_exists() {
        let temp = tempfile::tempdir().unwrap();
        let existing = temp.path().join("new.txt");
        std::fs::write(&existing, "contents").unwrap();
        let missing = temp.path().join("old.txt");
        let mut pending = PendingEvents::new();
        let start = Instant::now();

        pending.ingest(
            rename(
                RenameMode::Any,
                &[&missing.to_string_lossy(), &existing.to_string_lossy()],
            ),
            &[],
            start,
        );

        assert_eq!(
            take_after_debounce(&mut pending, start),
            describe(&[
                FileChangeEvent::Created(existing.clone()),
                FileChangeEvent::Deleted(missing),
            ])
        );
        assert!(Path::new(&existing).exists());
    }

    #[test]
    fn changes_wait_for_the_debounce_period() {
        let mut pending = PendingEvents::new();
        let start = Instant::now();

        pending.ingest(rename(RenameMode::From, &["old.txt"]), &[], start);
        pending.ingest(event(EventKind::Create(CreateKind::File), &["new.txt"]), &[], start);

        assert!(pending.take_due(start + DEBOUNCE / 2, DEBOUNCE).is_empty());
        assert_eq!(
            take_after_debounce(&mut pending, start),
            describe(&[
                FileChangeEvent::Created(PathBuf::from("new.txt")),
                FileChangeEvent::Deleted(PathBuf::from("old.txt")),
            ])
        );
        assert!(pending.take_due(start + DEBOUNCE * 2, DEBOUNCE).is_empty());
    }

    #[test]
    fn take_all_returns_unpaired_old_names_as_deletes() {
        let mut pending = PendingEvents::new();
        let start = Instant::now();

        pending.ingest(rename(RenameMode::From, &["old.txt"]), &[], start);
        pending.ingest(rename(RenameMode::Both, &["a.txt", "b.txt"]), &[], start);

        assert_eq!(
            describe(&pending.take_all()),
            describe(&[
                FileChangeEvent::Deleted(PathBuf::from("old.txt")),
                renamed("a.txt", "b.txt"),
            ])
        );
        assert!(pending.take_all().is_empty());
    }
}
