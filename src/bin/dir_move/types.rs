use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};

use cli_tools::path_to_filename_string;

/// Information about a directory used for matching files to move.
#[derive(Debug)]
pub struct DirectoryInfo {
    /// Absolute path to the directory.
    pub(crate) path: PathBuf,
    /// Normalized directory name (lowercase, dots replaced with spaces).
    pub(crate) name: String,
}

/// Information about a file for grouping purposes.
/// Uses `Cow` for efficient string handling - avoids cloning when possible.
pub struct FileInfo<'a> {
    /// Path to the file.
    pub(crate) path: Cow<'a, Path>,
    /// Original filename after stripping ignored prefixes (used for contiguity checks).
    pub(crate) original_name: Cow<'a, str>,
    /// Filtered filename with numeric/resolution/glue parts removed (used for prefix matching).
    pub(crate) filtered_name: Cow<'a, str>,
}

#[derive(Debug)]
pub struct MoveInfo {
    pub(crate) source: PathBuf,
    pub(crate) target: PathBuf,
}

/// A candidate prefix for grouping files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixCandidate<'a> {
    /// The prefix string (e.g., "Studio.Name" or "`StudioName`").
    pub(crate) prefix: Cow<'a, str>,
    /// Number of files matching this prefix.
    pub(crate) match_count: usize,
    /// Number of dot-separated parts in the prefix (1, 2, or 3).
    pub(crate) part_count: usize,
    /// Position in the filename where this prefix starts (0 = beginning).
    /// Lower values indicate prefixes closer to the start of the filename.
    pub(crate) start_position: usize,
}

/// Information about what needs to be moved during an unpack operation.
#[derive(Debug, Default)]
pub struct UnpackInfo {
    /// Files to move.
    pub(crate) file_moves: Vec<MoveInfo>,
    /// Directories to move directly.
    pub(crate) directory_moves: Vec<MoveInfo>,
}

impl FileInfo<'_> {
    /// Create a new `FileInfo` with owned strings.
    pub(crate) const fn new(path: PathBuf, original_name: String, filtered_name: String) -> Self {
        Self {
            path: Cow::Owned(path),
            original_name: Cow::Owned(original_name),
            filtered_name: Cow::Owned(filtered_name),
        }
    }

    /// Get the path as a `PathBuf`.
    pub(crate) fn path_buf(&self) -> PathBuf {
        self.path.to_path_buf()
    }
}

impl fmt::Debug for FileInfo<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileInfo")
            .field("path", &self.path)
            .field("original_name", &self.original_name)
            .field("filtered_name", &self.filtered_name)
            .finish()
    }
}

impl DirectoryInfo {
    pub(crate) fn new(path: PathBuf) -> Self {
        let name = path_to_filename_string(&path).to_lowercase().replace('.', " ");
        Self { path, name }
    }
}

impl MoveInfo {
    pub(crate) const fn new(source: PathBuf, target: PathBuf) -> Self {
        Self { source, target }
    }
}

impl<'a> PrefixCandidate<'a> {
    /// Create a new `PrefixCandidate`.
    pub(crate) const fn new(
        prefix: Cow<'a, str>,
        match_count: usize,
        part_count: usize,
        start_position: usize,
    ) -> Self {
        Self {
            prefix,
            match_count,
            part_count,
            start_position,
        }
    }
}

/// A group of files that share a common prefix.
/// Used for organizing files into directories based on their name prefixes.
#[derive(Debug, Clone)]
pub struct PrefixGroup {
    /// Files belonging to this prefix group.
    pub(crate) files: Vec<PathBuf>,
    /// Number of dot-separated parts in the prefix (1, 2, or 3).
    /// Higher values indicate more specific prefixes.
    pub(crate) part_count: usize,
    /// Minimum start position where this prefix appears across all files.
    /// Lower values indicate prefixes closer to the start of filenames.
    pub(crate) min_start_position: usize,
}

impl PrefixGroup {
    /// Create a new `PrefixGroup`.
    pub(crate) const fn new(files: Vec<PathBuf>, part_count: usize, min_start_position: usize) -> Self {
        Self {
            files,
            part_count,
            min_start_position,
        }
    }
}

/// Intermediate data structure for building prefix groups.
/// Used during the collection phase before converting to final `PrefixGroup`.
#[derive(Debug)]
pub struct PrefixGroupBuilder {
    /// The original prefix string (may contain dots).
    pub(crate) original_prefix: String,
    /// Files belonging to this prefix group.
    pub(crate) files: Vec<PathBuf>,
    /// Number of dot-separated parts in the prefix.
    pub(crate) part_count: usize,
    /// Whether the prefix has a concatenated (no-dot) form.
    pub(crate) has_concatenated_form: bool,
    /// Minimum start position where this prefix appears across all files.
    pub(crate) min_start_position: usize,
}

impl PrefixGroupBuilder {
    /// Create a new `PrefixGroupBuilder`.
    pub(crate) fn new(
        original_prefix: String,
        file: PathBuf,
        part_count: usize,
        has_concatenated_form: bool,
        start_position: usize,
    ) -> Self {
        Self {
            original_prefix,
            files: vec![file],
            part_count,
            has_concatenated_form,
            min_start_position: start_position,
        }
    }

    /// Add a file to this group and update metadata.
    /// If this is the first concatenated form encountered, also update the original prefix.
    pub(crate) fn add_file(
        &mut self,
        file: PathBuf,
        part_count: usize,
        is_concatenated: bool,
        start_position: usize,
        prefix: String,
    ) {
        self.files.push(file);
        self.part_count = self.part_count.max(part_count);
        self.min_start_position = self.min_start_position.min(start_position);
        // Prefer concatenated (no-dot) form for directory names
        if is_concatenated && !self.has_concatenated_form {
            self.original_prefix = prefix;
            self.has_concatenated_form = true;
        }
    }

    /// Convert this builder into a final `PrefixGroup`.
    pub(crate) fn into_prefix_group(self) -> (String, PrefixGroup) {
        (
            self.original_prefix,
            PrefixGroup::new(self.files, self.part_count, self.min_start_position),
        )
    }
}
