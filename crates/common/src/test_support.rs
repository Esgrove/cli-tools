//! Temporary directory trees for tests, behind the `test-support` feature.
//!
//! `TempTree` builds nested files, directories, symlinks, and non-UTF-8 names under a temporary root,
//! and snapshots the tree as sorted relative paths so a test can assert the whole result in one comparison.
//! The root has a visible name,
//! because the default `tempfile` prefix starts with a dot and the walkers skip hidden entries.

#![allow(
    clippy::missing_panics_doc,
    reason = "test helpers panic with a message when the filesystem setup fails"
)]
#![allow(
    clippy::must_use_candidate,
    reason = "the builders return the created path for convenience, and most callers only need the side effect"
)]

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use walkdir::WalkDir;

/// Temporary directory tree that is removed when dropped.
#[derive(Debug)]
pub struct TempTree {
    directory: TempDir,
}

impl TempTree {
    /// Create an empty tree with a visible root directory name.
    #[must_use]
    pub fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("cli_tools_test_")
            .tempdir()
            .expect("temporary directory should be created");
        Self { directory }
    }

    /// Create a tree holding empty files at the given relative paths.
    ///
    /// A path ending in "/" creates a directory instead.
    #[must_use]
    pub fn with_entries(entries: &[&str]) -> Self {
        let tree = Self::new();
        for entry in entries {
            if let Some(directory) = entry.strip_suffix('/') {
                tree.dir(directory);
            } else {
                tree.file(entry);
            }
        }
        tree
    }

    /// Root directory of the tree.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.directory.path()
    }

    /// Absolute path of a relative path inside the tree.
    #[must_use]
    pub fn join(&self, relative: &str) -> PathBuf {
        self.path().join(relative)
    }

    /// Create an empty file, creating its parent directories as needed.
    pub fn file(&self, relative: &str) -> PathBuf {
        self.file_with(relative, "")
    }

    /// Create a file with the given contents, creating its parent directories as needed.
    pub fn file_with(&self, relative: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent directories should be created");
        }
        fs::write(&path, contents).expect("file should be written");
        path
    }

    /// Create a directory and its parents.
    pub fn dir(&self, relative: &str) -> PathBuf {
        let path = self.join(relative);
        fs::create_dir_all(&path).expect("directory should be created");
        path
    }

    /// Create a symlink at `link` pointing to `target`, both relative to the root.
    ///
    /// The link stores `target` as given, so it stays valid if the whole tree moves.
    #[cfg(unix)]
    pub fn symlink(&self, target: &str, link: &str) -> PathBuf {
        let link_path = self.join(link);
        if let Some(parent) = link_path.parent() {
            fs::create_dir_all(parent).expect("parent directories should be created");
        }
        let link_depth = Path::new(link).components().count().saturating_sub(1);
        let relative_target: PathBuf = std::iter::repeat_n(Path::new(".."), link_depth)
            .collect::<PathBuf>()
            .join(target);
        std::os::unix::fs::symlink(relative_target, &link_path).expect("symlink should be created");
        link_path
    }

    /// Create a symlink at `link` pointing to `target` like [`Self::symlink`], on every platform.
    ///
    /// Returns `None` on Windows when the process may not create symlinks,
    /// which needs Developer Mode or an elevated process, so the calling test can skip itself.
    /// Windows links to a directory are created as directory symlinks, so `target` should exist first.
    pub fn try_symlink(&self, target: &str, link: &str) -> Option<PathBuf> {
        let link_path = self.join(link);
        if let Some(parent) = link_path.parent() {
            fs::create_dir_all(parent).expect("parent directories should be created");
        }
        let link_depth = Path::new(link).components().count().saturating_sub(1);
        let relative_target: PathBuf = std::iter::repeat_n(Path::new(".."), link_depth)
            .collect::<PathBuf>()
            .join(target);
        create_symlink(&relative_target, &link_path).then_some(link_path)
    }

    /// Create an empty file whose name holds an invalid UTF-8 byte between `stem` and `extension`.
    ///
    /// Returns `None` when the filesystem rejects non-UTF-8 names, as APFS does.
    #[cfg(unix)]
    pub fn non_utf8_file(&self, directory: &str, stem: &str, extension: &str) -> Option<PathBuf> {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let mut name = stem.as_bytes().to_vec();
        name.push(0xFF);
        name.push(b'.');
        name.extend_from_slice(extension.as_bytes());
        let parent = self.dir(directory);
        let path = parent.join(OsStr::from_bytes(&name));
        fs::write(&path, "").ok().map(|()| path)
    }

    /// Read a file inside the tree as text.
    #[must_use]
    pub fn read(&self, relative: &str) -> String {
        fs::read_to_string(self.join(relative)).expect("file should be readable")
    }

    /// Whether a relative path exists, without following a final symlink.
    #[must_use]
    pub fn exists(&self, relative: &str) -> bool {
        self.join(relative).symlink_metadata().is_ok()
    }

    /// Relative paths of every file and symlink in the tree, sorted, using "/" as the separator.
    #[must_use]
    pub fn files(&self) -> Vec<String> {
        self.collect(|file_type| !file_type.is_dir())
    }

    /// Relative paths of every directory in the tree, sorted, using "/" as the separator.
    #[must_use]
    pub fn dirs(&self) -> Vec<String> {
        self.collect(fs::FileType::is_dir)
    }

    /// Every entry in the tree, sorted.
    ///
    /// Directories end in "/", symlinks show as "link -> target",
    /// and files with contents show as "file: contents".
    /// Two snapshots compare equal only when names, kinds, link targets, and contents all match.
    #[must_use]
    pub fn snapshot(&self) -> Vec<String> {
        let mut entries: Vec<String> = self
            .entries()
            .map(|entry| {
                let relative = self.relative(entry.path());
                let file_type = entry.file_type();
                if file_type.is_dir() {
                    format!("{relative}/")
                } else if file_type.is_symlink() {
                    let target = fs::read_link(entry.path()).expect("symlink should be readable");
                    format!("{relative} -> {}", target.to_string_lossy())
                } else {
                    let contents = fs::read(entry.path()).expect("file should be readable");
                    if contents.is_empty() {
                        relative
                    } else {
                        format!("{relative}: {}", String::from_utf8_lossy(&contents))
                    }
                }
            })
            .collect();
        entries.sort();
        entries
    }

    /// Walk every entry below the root without following symlinks.
    fn entries(&self) -> impl Iterator<Item = walkdir::DirEntry> {
        WalkDir::new(self.path())
            .min_depth(1)
            .follow_links(false)
            .into_iter()
            .map(|entry| entry.expect("tree entry should be readable"))
    }

    /// Sorted relative paths of the entries whose type passes the filter.
    fn collect(&self, keep: impl Fn(&fs::FileType) -> bool) -> Vec<String> {
        let mut paths: Vec<String> = self
            .entries()
            .filter(|entry| keep(&entry.file_type()))
            .map(|entry| self.relative(entry.path()))
            .collect();
        paths.sort();
        paths
    }

    /// Path relative to the root as a lossy string with "/" separators.
    fn relative(&self, path: &Path) -> String {
        let relative = path.strip_prefix(self.path()).expect("entry should be inside the tree");
        relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/")
    }
}

impl Default for TempTree {
    fn default() -> Self {
        Self::new()
    }
}

/// Create a symlink at `link` pointing to `target`, which is resolved relative to the parent of `link`.
///
/// Returns `false` on Windows when the process may not create symlinks,
/// which needs Developer Mode or an elevated process, so the calling test can skip itself.
/// Windows links to a directory are created as directory symlinks, so `target` should exist first.
pub fn create_symlink(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).expect("symlink should be created");
        true
    }
    #[cfg(windows)]
    {
        /// Windows error code for a missing symlink privilege.
        const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;

        // The stored target is not normalized, and Windows does not resolve "/" inside a symlink target.
        let target: PathBuf = target.components().collect();
        let resolved = link
            .parent()
            .map_or_else(|| target.clone(), |parent| parent.join(&target));
        let result = if resolved.is_dir() {
            std::os::windows::fs::symlink_dir(&target, link)
        } else {
            std::os::windows::fs::symlink_file(&target, link)
        };
        result.map_or_else(
            |error| {
                assert_eq!(
                    error.raw_os_error(),
                    Some(ERROR_PRIVILEGE_NOT_HELD),
                    "symlink should be created: {error}"
                );
                false
            },
            |()| true,
        )
    }
}

#[cfg(test)]
mod test_temp_tree {
    use super::*;

    #[test]
    fn root_name_is_not_hidden() {
        let tree = TempTree::new();
        let name = tree
            .path()
            .file_name()
            .expect("root should have a name")
            .to_string_lossy();
        assert!(!name.starts_with('.'));
    }

    #[test]
    fn with_entries_builds_nested_files_and_directories() {
        let tree = TempTree::with_entries(&["a/b/c.txt", "a/d.txt", "empty/", "top.txt"]);
        assert_eq!(tree.files(), vec!["a/b/c.txt", "a/d.txt", "top.txt"]);
        assert_eq!(tree.dirs(), vec!["a", "a/b", "empty"]);
    }

    #[test]
    fn snapshot_shows_kinds_and_contents() {
        let tree = TempTree::new();
        tree.file_with("one/two.txt", "hello");
        tree.file("one/three.txt");
        tree.dir("four");
        assert_eq!(
            tree.snapshot(),
            vec!["four/", "one/", "one/three.txt", "one/two.txt: hello"]
        );
        assert_eq!(tree.read("one/two.txt"), "hello");
        assert!(tree.exists("four"));
        assert!(!tree.exists("five"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_listed_but_not_followed() {
        let tree = TempTree::new();
        tree.file("real/file.txt");
        tree.symlink("real", "nested/link");
        assert_eq!(tree.files(), vec!["nested/link", "real/file.txt"]);
        assert_eq!(tree.dirs(), vec!["nested", "real"]);
        assert!(tree.snapshot().contains(&"nested/link -> ../real".to_string()));
        assert!(tree.join("nested/link/file.txt").is_file());
    }

    #[test]
    fn try_symlink_creates_directory_and_file_links_on_every_platform() {
        let tree = TempTree::new();
        tree.file_with("real/file.txt", "hello");
        let Some(directory_link) = tree.try_symlink("real", "nested/link") else {
            return;
        };
        let file_link = tree
            .try_symlink("real/file.txt", "file-link.txt")
            .expect("file symlink");
        assert!(fs::symlink_metadata(&directory_link).unwrap().file_type().is_symlink());
        assert!(fs::symlink_metadata(&file_link).unwrap().file_type().is_symlink());
        assert_eq!(tree.files(), vec!["file-link.txt", "nested/link", "real/file.txt"]);
        assert_eq!(tree.dirs(), vec!["nested", "real"]);
        assert_eq!(fs::read_to_string(tree.join("nested/link/file.txt")).unwrap(), "hello");
        assert_eq!(fs::read_to_string(file_link).unwrap(), "hello");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_file_is_listed_lossily_when_supported() {
        let tree = TempTree::new();
        if let Some(path) = tree.non_utf8_file("sub", "name", "txt") {
            assert!(path.to_str().is_none());
            assert_eq!(tree.files(), vec!["sub/name\u{FFFD}.txt"]);
        }
    }
}
