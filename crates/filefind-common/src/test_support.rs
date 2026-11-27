//! Test fixture for tools that update the filefind index.
//!
//! [`IndexFixture`] creates an index database covering one temporary root, indexes existing paths under it,
//! and lists what the index holds afterwards, so tool tests can check the index followed their changes.
//! Run the tool with [`IndexFixture::environment`] to point it at the fixture instead of the user's index.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::config::{CONFIG_PATH_ENV, DATABASE_PATH_ENV};
use crate::database::Database;
use crate::types::{FileEntry, IndexedVolume, VolumeType};

/// A temporary filefind index whose single volume is a test root directory.
pub struct IndexFixture {
    /// Path of the index database file.
    database_path: PathBuf,
    /// Directory the index volume covers.
    root: PathBuf,
}

impl IndexFixture {
    /// Create an index at `database_path` with a volume covering `root`, and index the existing `entries`.
    ///
    /// # Panics
    /// Panics if the database cannot be created or an entry does not exist.
    #[must_use]
    pub fn create(database_path: &Path, root: &Path, entries: &[PathBuf]) -> Self {
        let fixture = Self {
            database_path: database_path.to_path_buf(),
            root: root.to_path_buf(),
        };
        let database = Database::open(database_path).expect("the fixture index should be created");
        let volume_id = database
            .upsert_volume(&IndexedVolume {
                id: None,
                serial_number: "TEST-FIXTURE".into(),
                label: None,
                mount_point: root.to_string_lossy().to_string(),
                volume_type: VolumeType::Local,
                is_online: true,
                last_scan_time: None,
                last_usn: None,
            })
            .expect("the fixture volume should be inserted");
        for entry in entries {
            // The index stores normalized absolute paths, so mixed separators from test helpers are normalized too.
            let entry = std::path::absolute(entry).expect("a fixture entry should have an absolute path");
            let metadata = std::fs::metadata(&entry).expect("an indexed fixture entry should exist");
            database
                .insert_file(&FileEntry::from_metadata(volume_id, &entry, &metadata))
                .expect("the fixture entry should be indexed");
        }
        fixture
    }

    /// Create an index at `database_path` covering multiple roots, each stored as its own volume.
    ///
    /// # Panics
    /// Panics if the database cannot be created or an entry does not exist.
    #[must_use]
    pub fn create_with_roots(database_path: &Path, roots: &[(&Path, &[PathBuf])]) -> Self {
        let primary_root = roots
            .first()
            .map_or_else(PathBuf::new, |(root, _)| (*root).to_path_buf());
        let fixture = Self {
            database_path: database_path.to_path_buf(),
            root: primary_root,
        };
        let database = Database::open(database_path).expect("the fixture index should be created");
        for (index, (root, entries)) in roots.iter().enumerate() {
            let volume_id = database
                .upsert_volume(&IndexedVolume {
                    id: None,
                    serial_number: format!("TEST-FIXTURE-{index}"),
                    label: None,
                    mount_point: root.to_string_lossy().to_string(),
                    volume_type: VolumeType::Local,
                    is_online: true,
                    last_scan_time: None,
                    last_usn: None,
                })
                .expect("the fixture volume should be inserted");
            for entry in *entries {
                let entry = std::path::absolute(entry).expect("a fixture entry should have an absolute path");
                let metadata = std::fs::metadata(&entry).expect("an indexed fixture entry should exist");
                database
                    .insert_file(&FileEntry::from_metadata(volume_id, &entry, &metadata))
                    .expect("the fixture entry should be indexed");
            }
        }
        fixture
    }

    /// Environment variables that point a tool at this index and at no filefind config file.
    #[must_use]
    pub fn environment(&self) -> [(&'static str, OsString); 2] {
        [
            (DATABASE_PATH_ENV, self.database_path.clone().into_os_string()),
            (
                CONFIG_PATH_ENV,
                self.root.join("missing-filefind.toml").into_os_string(),
            ),
        ]
    }

    /// The indexed paths relative to the root, with forward slashes, sorted.
    ///
    /// # Panics
    /// Panics if the index cannot be read.
    #[must_use]
    pub fn indexed_paths(&self) -> Vec<String> {
        let root = self.root.to_string_lossy().to_string();
        let mut paths: Vec<String> = Database::open(&self.database_path)
            .expect("the fixture index should open")
            .search_by_path("", usize::MAX)
            .expect("the fixture index should be listed")
            .into_iter()
            .map(|entry| {
                entry
                    .full_path
                    .strip_prefix(&root)
                    .unwrap_or(&entry.full_path)
                    .trim_start_matches(['/', '\\'])
                    .replace('\\', "/")
            })
            .collect();
        paths.sort();
        paths
    }
}

#[cfg(test)]
mod test_index_fixture {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn indexes_the_given_entries_relative_to_the_root() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(root.join("dir")).unwrap();
        std::fs::write(root.join("dir").join("file.txt"), "contents").unwrap();
        std::fs::write(root.join("unindexed.txt"), "contents").unwrap();

        let fixture = IndexFixture::create(
            &temp.path().join("filefind.db"),
            &root,
            &[root.join("dir"), root.join("dir").join("file.txt")],
        );

        assert_eq!(fixture.indexed_paths(), vec!["dir", "dir/file.txt"]);
        let environment = fixture.environment();
        assert_eq!(environment[0].0, "FILEFIND_DB");
        assert_eq!(environment[0].1, temp.path().join("filefind.db").into_os_string());
        assert_eq!(environment[1].0, "FILEFIND_CONFIG");
    }
}
