//! Records of completed file system moves and removals.
//!
//! Tools collect a [`PathChange`] for every rename, move, delete, or trash they perform,
//! so indexes of the file system, such as the filefind database, can be updated without rescanning.

use std::path::{Path, PathBuf};

/// A completed change to a file or directory path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathChange {
    /// The entry at `from` now lives at `to`, after a rename or a move.
    Moved {
        /// Path before the move.
        from: PathBuf,
        /// Path after the move.
        to: PathBuf,
        /// Whether the moved entry is a directory, which moves everything under it too.
        is_directory: bool,
    },
    /// The entry at `path` was deleted or moved to the trash.
    Removed {
        /// Path of the removed entry.
        path: PathBuf,
        /// Whether the removed entry is a directory, which removes everything under it too.
        is_directory: bool,
    },
}

impl PathChange {
    /// A file moved or renamed from `from` to `to`.
    #[must_use]
    pub fn moved_file(from: impl Into<PathBuf>, to: impl Into<PathBuf>) -> Self {
        Self::Moved {
            from: from.into(),
            to: to.into(),
            is_directory: false,
        }
    }

    /// A directory moved or renamed from `from` to `to`, with everything under it.
    #[must_use]
    pub fn moved_directory(from: impl Into<PathBuf>, to: impl Into<PathBuf>) -> Self {
        Self::Moved {
            from: from.into(),
            to: to.into(),
            is_directory: true,
        }
    }

    /// A file deleted or moved to the trash.
    #[must_use]
    pub fn removed_file(path: impl Into<PathBuf>) -> Self {
        Self::Removed {
            path: path.into(),
            is_directory: false,
        }
    }

    /// A directory deleted or moved to the trash, with everything under it.
    #[must_use]
    pub fn removed_directory(path: impl Into<PathBuf>) -> Self {
        Self::Removed {
            path: path.into(),
            is_directory: true,
        }
    }

    /// The path the change starts from: the old path of a move, or the removed path.
    #[must_use]
    pub fn source(&self) -> &Path {
        match self {
            Self::Moved { from, .. } => from,
            Self::Removed { path, .. } => path,
        }
    }

    /// Whether the change applies to a directory and everything under it.
    #[must_use]
    pub const fn is_directory(&self) -> bool {
        match self {
            Self::Moved { is_directory, .. } | Self::Removed { is_directory, .. } => *is_directory,
        }
    }
}

#[cfg(test)]
mod test_path_change {
    use super::*;

    #[test]
    fn constructors_set_the_kind_and_paths() {
        assert_eq!(
            PathChange::moved_file("a.txt", "b.txt"),
            PathChange::Moved {
                from: PathBuf::from("a.txt"),
                to: PathBuf::from("b.txt"),
                is_directory: false,
            }
        );
        assert_eq!(
            PathChange::moved_directory("old", "new"),
            PathChange::Moved {
                from: PathBuf::from("old"),
                to: PathBuf::from("new"),
                is_directory: true,
            }
        );
        assert_eq!(
            PathChange::removed_file("gone.txt"),
            PathChange::Removed {
                path: PathBuf::from("gone.txt"),
                is_directory: false,
            }
        );
        assert_eq!(
            PathChange::removed_directory("gone"),
            PathChange::Removed {
                path: PathBuf::from("gone"),
                is_directory: true,
            }
        );
    }

    #[test]
    fn source_is_the_old_or_removed_path() {
        assert_eq!(PathChange::moved_file("a.txt", "b.txt").source(), Path::new("a.txt"));
        assert_eq!(PathChange::removed_directory("gone").source(), Path::new("gone"));
    }

    #[test]
    fn is_directory_follows_the_constructor() {
        assert!(PathChange::moved_directory("old", "new").is_directory());
        assert!(PathChange::removed_directory("gone").is_directory());
        assert!(!PathChange::moved_file("a.txt", "b.txt").is_directory());
        assert!(!PathChange::removed_file("gone.txt").is_directory());
    }
}
