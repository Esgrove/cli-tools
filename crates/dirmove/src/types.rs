//! Destination directory information shared by scanning, filtering, and matching.

use std::path::PathBuf;

use cli_tools_common::path_to_filename_string;

/// Information about a directory used for matching files to move.
#[derive(Debug)]
pub struct DirectoryInfo {
    /// Absolute path to the directory.
    pub path: PathBuf,
    /// Normalized directory name (lowercase, dots replaced with spaces).
    pub name: String,
}

impl DirectoryInfo {
    /// Create a new `DirectoryInfo` from a directory path.
    /// Normalizes the directory name to lowercase with dots replaced by spaces.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        let name = path_to_filename_string(&path).to_lowercase().replace('.', " ");
        Self { path, name }
    }
}
