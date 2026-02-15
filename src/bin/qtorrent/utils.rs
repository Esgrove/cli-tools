use std::borrow::Cow;
use std::path::Path;

use cli_tools::dot_rename::DotFormat;

use crate::torrent::Torrent;

// List of known media file extensions
const KNOWN_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "mpg", "mpeg", "ts", "mp3", "flac", "wav", "aac", "ogg",
    "wma", "m4a", "opus", "alac", "rar", "zip", "7z", "tar", "gz", "bz2", "xz", "srt", "sub", "jpg", "jpeg", "png",
    "gif", "bmp", "webp", "tiff", "tif", "pdf", "epub", "mobi",
];

/// Information about a torrent file to be added.
pub struct TorrentInfo {
    /// Path to the torrent file.
    pub(crate) path: std::path::PathBuf,
    /// Parsed torrent data.
    pub(crate) torrent: Torrent,
    /// Raw torrent file bytes.
    pub(crate) bytes: Vec<u8>,
    /// Info hash calculated from raw bytes (lowercase hex).
    pub(crate) info_hash: String,
    /// Whether the original torrent has multiple files.
    pub(crate) original_is_multi_file: bool,
    /// Whether to treat this as multi-file after filtering (determines subdirectory creation).
    /// This is true only if more than one file will be included after filtering.
    pub(crate) effective_is_multi_file: bool,
    /// Custom name to rename to (None = use torrent's internal name).
    pub(crate) rename_to: Option<String>,
    /// Size of the included files.
    pub(crate) included_size: u64,
    /// Indices of files to exclude (for setting priority to 0).
    pub(crate) excluded_indices: Vec<usize>,
    /// For originally multi-file torrents that become effectively single-file,
    /// store the single included file's name to get the correct extension.
    pub(crate) single_included_file: Option<String>,
    /// Original name from torrent metadata (for file/folder renaming on disk).
    /// For single-file torrents, this is the filename.
    /// For multi-file torrents, this is the root folder name.
    pub(crate) original_name: Option<String>,
    /// Resolved tags for this torrent (from tag overwrite prefixes or default config tags).
    pub(crate) tags: Option<String>,
}

/// Summary of files skipped due to directory matching.
#[derive(Debug, Default)]
pub struct SkippedDirectorySummary {
    /// Number of files in the skipped directory.
    pub(crate) file_count: usize,
    /// Total size of all files in the skipped directory.
    pub(crate) total_size: u64,
}

impl SkippedDirectorySummary {
    /// Add a file to this summary.
    pub(crate) const fn add_file(&mut self, size: u64) {
        self.file_count += 1;
        self.total_size += size;
    }

    /// Returns "file" or "files" based on the count.
    pub(crate) const fn files_word(&self) -> &'static str {
        if self.file_count == 1 { "file" } else { "files" }
    }
}

impl TorrentInfo {
    /// Check if all files in this torrent were excluded by filters.
    ///
    /// Returns `true` for multi-file torrents where every file was filtered out,
    /// meaning there are no files left to download.
    pub(crate) fn all_files_excluded(&self) -> bool {
        self.original_is_multi_file
            && !self.excluded_indices.is_empty()
            && self.excluded_indices.len() == self.torrent.files().len()
    }

    /// Get the display name for this torrent (`rename_to` or internal name).
    #[allow(clippy::option_if_let_else)]
    pub(crate) fn display_name(&self) -> Cow<'_, str> {
        if let Some(ref name) = self.rename_to {
            Cow::Borrowed(name.as_str())
        } else if let Some(name) = self.torrent.name() {
            Cow::Borrowed(name)
        } else {
            Cow::Borrowed("unknown")
        }
    }

    /// Get the suggested name derived from the torrent filename.
    ///
    /// This returns the raw name without any filtering applied.
    /// Use `clean_suggested_name` to apply `remove_from_name` filtering.
    ///
    /// If `ignore_filename_patterns` is provided and the torrent filename contains any of these
    /// strings, the filename is ignored and the internal name is used instead.
    #[allow(clippy::option_if_let_else)]
    pub(crate) fn suggested_name_raw(&self, ignore_filename_patterns: &[String]) -> Cow<'_, str> {
        // Try to get name from torrent filename first, unless it matches ignore patterns
        let torrent_filename = self.path.file_stem().and_then(|stem| stem.to_str()).filter(|filename| {
            // Skip filename if it contains any of the ignore patterns
            !ignore_filename_patterns
                .iter()
                .any(|pattern| filename.contains(pattern))
        });

        // Get the internal name from the torrent
        let internal_name = self.torrent.name();

        // For effective multi-file torrents (after filtering), this becomes the folder name
        if self.effective_is_multi_file {
            // Prefer torrent filename over internal name
            return if let Some(name) = torrent_filename {
                Cow::Borrowed(name)
            } else if let Some(name) = internal_name {
                Cow::Borrowed(name)
            } else {
                Cow::Borrowed("unknown")
            };
        }

        // For single-file torrents (or originally multi-file that became single after filtering),
        // preserve the file extension
        if let Some(filename) = torrent_filename {
            // For originally multi-file torrents that became single-file after filtering,
            // get the extension from the single included file
            let extension_source = if self.original_is_multi_file {
                self.single_included_file.as_deref()
            } else {
                internal_name
            };

            if let Some(source) = extension_source
                && let Some(extension) = Path::new(source).extension()
            {
                let extension_str = extension.to_string_lossy();
                // Check if the filename already has this extension
                if !filename
                    .to_lowercase()
                    .ends_with(&format!(".{}", extension_str.to_lowercase()))
                {
                    return Cow::Owned(format!("{filename}.{extension_str}"));
                }
            }
            return Cow::Borrowed(filename);
        }

        // Fall back to the internal name or single included file
        if let Some(ref file) = self.single_included_file {
            Cow::Borrowed(file.as_str())
        } else if let Some(name) = internal_name {
            Cow::Borrowed(name)
        } else {
            Cow::Borrowed("unknown")
        }
    }
}

/// Extract a file extension if it looks like a real media file extension.
///
/// Only recognises known media extensions to avoid treating names like "Show.Name" as having extension "Name".
/// Also filters out purely numeric extensions (like `.15` from dates).
pub fn extract_file_extension(name: &str) -> Option<String> {
    let ext = Path::new(name).extension()?.to_string_lossy().to_lowercase();

    // If the extension is purely numeric, it's likely part of a date, not a real extension
    if ext.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    if KNOWN_EXTENSIONS.contains(&ext.as_str()) {
        Some(ext)
    } else {
        None
    }
}

/// Insert a date before the file extension, or at the end if no extension.
pub fn insert_date_before_extension(name: &str, date: &str) -> String {
    let path = Path::new(name);
    path.extension().map_or_else(
        || format!("{name}.{date}"),
        |ext| {
            let stem = path.file_stem().map_or(name, |s| s.to_str().unwrap_or(name));
            format!("{stem}.{date}.{}", ext.to_string_lossy())
        },
    )
}

/// Format a single file name, stripping extension before formatting and restoring it after.
pub fn format_single_file_name(dot_rename: &DotFormat, name: &str) -> String {
    // For single files, strip the extension before formatting and restore it after.
    // DotRename expects names without extensions.
    if let Ok((stem, extension)) = cli_tools::get_normalized_file_name_and_extension(Path::new(name)) {
        let formatted_stem = dot_rename.format_name(&stem);
        if extension.is_empty() {
            formatted_stem
        } else {
            format!("{formatted_stem}.{extension}")
        }
    } else {
        dot_rename.format_name(name)
    }
}

#[cfg(test)]
mod test_insert_date_before_extension {
    use super::*;

    #[test]
    fn inserts_date_before_extension() {
        let result = insert_date_before_extension("Name.mp4", "2024.01.15");
        assert_eq!(result, "Name.2024.01.15.mp4");
    }

    #[test]
    fn appends_date_when_no_extension() {
        let result = insert_date_before_extension("Name", "2024.01.15");
        assert_eq!(result, "Name.2024.01.15");
    }

    #[test]
    fn handles_multiple_dots_in_name() {
        let result = insert_date_before_extension("Some.Name.Here.mp4", "2024.01.15");
        assert_eq!(result, "Some.Name.Here.2024.01.15.mp4");
    }
}

#[cfg(test)]
mod test_extract_file_extension {
    use super::*;

    #[test]
    fn extracts_known_video_extension() {
        assert_eq!(extract_file_extension("movie.mp4"), Some("mp4".to_string()));
        assert_eq!(extract_file_extension("video.mkv"), Some("mkv".to_string()));
        assert_eq!(extract_file_extension("clip.avi"), Some("avi".to_string()));
    }

    #[test]
    fn extracts_known_audio_extension() {
        assert_eq!(extract_file_extension("song.mp3"), Some("mp3".to_string()));
        assert_eq!(extract_file_extension("track.flac"), Some("flac".to_string()));
        assert_eq!(extract_file_extension("audio.ogg"), Some("ogg".to_string()));
    }

    #[test]
    fn extracts_known_archive_extension() {
        assert_eq!(extract_file_extension("archive.rar"), Some("rar".to_string()));
        assert_eq!(extract_file_extension("files.zip"), Some("zip".to_string()));
        assert_eq!(extract_file_extension("backup.7z"), Some("7z".to_string()));
    }

    #[test]
    fn extracts_known_image_extension() {
        assert_eq!(extract_file_extension("photo.jpg"), Some("jpg".to_string()));
        assert_eq!(extract_file_extension("image.png"), Some("png".to_string()));
        assert_eq!(extract_file_extension("pic.webp"), Some("webp".to_string()));
    }

    #[test]
    fn extracts_known_document_extension() {
        assert_eq!(extract_file_extension("doc.pdf"), Some("pdf".to_string()));
        assert_eq!(extract_file_extension("book.epub"), Some("epub".to_string()));
        assert_eq!(extract_file_extension("ebook.mobi"), Some("mobi".to_string()));
    }

    #[test]
    fn returns_none_for_unknown_extension() {
        assert_eq!(extract_file_extension("Show.Name"), None);
        assert_eq!(extract_file_extension("file.xyz"), None);
        assert_eq!(extract_file_extension("document.unknown"), None);
    }

    #[test]
    fn returns_none_for_numeric_extension() {
        assert_eq!(extract_file_extension("Show.2024.01.15"), None);
        assert_eq!(extract_file_extension("File.123"), None);
        assert_eq!(extract_file_extension("Name.99"), None);
    }

    #[test]
    fn returns_none_for_no_extension() {
        assert_eq!(extract_file_extension("filename"), None);
        assert_eq!(extract_file_extension(""), None);
    }

    #[test]
    fn handles_case_insensitive_extensions() {
        assert_eq!(extract_file_extension("video.MP4"), Some("mp4".to_string()));
        assert_eq!(extract_file_extension("audio.FLAC"), Some("flac".to_string()));
        assert_eq!(extract_file_extension("image.PNG"), Some("png".to_string()));
    }

    #[test]
    fn handles_multiple_dots() {
        assert_eq!(extract_file_extension("Show.Name.S01E01.mp4"), Some("mp4".to_string()));
        assert_eq!(
            extract_file_extension("Some.File.With.Many.Dots.mkv"),
            Some("mkv".to_string())
        );
    }
}

#[cfg(test)]
mod test_skipped_directory_summary {
    use super::*;

    #[test]
    fn default_values() {
        let summary = SkippedDirectorySummary::default();
        assert_eq!(summary.file_count, 0);
        assert_eq!(summary.total_size, 0);
    }

    #[test]
    fn add_file_increments_count_and_size() {
        let mut summary = SkippedDirectorySummary::default();
        summary.add_file(100);
        assert_eq!(summary.file_count, 1);
        assert_eq!(summary.total_size, 100);
    }

    #[test]
    fn add_multiple_files() {
        let mut summary = SkippedDirectorySummary::default();
        summary.add_file(100);
        summary.add_file(200);
        summary.add_file(300);
        assert_eq!(summary.file_count, 3);
        assert_eq!(summary.total_size, 600);
    }

    #[test]
    fn files_word_singular() {
        let mut summary = SkippedDirectorySummary::default();
        summary.add_file(100);
        assert_eq!(summary.files_word(), "file");
    }

    #[test]
    fn files_word_plural_zero() {
        let summary = SkippedDirectorySummary::default();
        assert_eq!(summary.files_word(), "files");
    }

    #[test]
    fn files_word_plural_multiple() {
        let mut summary = SkippedDirectorySummary::default();
        summary.add_file(100);
        summary.add_file(200);
        assert_eq!(summary.files_word(), "files");
    }
}

#[cfg(test)]
mod test_format_single_file_name {
    use super::*;
    use cli_tools::dot_rename::DotRenameConfig;

    #[test]
    fn formats_name_with_extension() {
        let config = DotRenameConfig::default();
        let dot_format = DotFormat::new(&config);
        let result = format_single_file_name(&dot_format, "Some Name Here.mp4");
        assert_eq!(result, "Some.Name.Here.mp4");
    }

    #[test]
    fn formats_name_without_extension() {
        let config = DotRenameConfig::default();
        let dot_format = DotFormat::new(&config);
        let result = format_single_file_name(&dot_format, "Some Name Here");
        assert_eq!(result, "Some.Name.Here");
    }

    #[test]
    fn preserves_extension_case() {
        let config = DotRenameConfig::default();
        let dot_format = DotFormat::new(&config);
        let result = format_single_file_name(&dot_format, "File Name.MKV");
        assert_eq!(result, "File.Name.MKV");
    }

    #[test]
    fn handles_multiple_spaces() {
        let config = DotRenameConfig::default();
        let dot_format = DotFormat::new(&config);
        let result = format_single_file_name(&dot_format, "File   With   Spaces.mp4");
        assert_eq!(result, "File.With.Spaces.mp4");
    }

    #[test]
    fn handles_underscores() {
        let config = DotRenameConfig::default();
        let dot_format = DotFormat::new(&config);
        let result = format_single_file_name(&dot_format, "File_With_Underscores.mp4");
        assert_eq!(result, "File.With.Underscores.mp4");
    }
}

#[cfg(test)]
mod test_torrent_info_helpers {
    //! Helper module to create test `TorrentInfo` instances.

    use super::*;
    use crate::torrent::{File, Torrent};
    use std::path::PathBuf;

    /// Creates a minimal single-file torrent for testing.
    pub fn create_single_file_torrent(name: &str) -> Torrent {
        let mut torrent = Torrent::default();
        torrent.info.name = Some(name.to_string());
        torrent.info.length = Some(1000);
        torrent
    }

    /// Creates a minimal multi-file torrent for testing.
    ///
    /// Returns a torrent with the given file names, each 500 bytes.
    pub fn create_multi_file_torrent(name: &str, file_names: &[&str]) -> Torrent {
        let mut torrent = Torrent::default();
        torrent.info.name = Some(name.to_string());
        torrent.info.files = Some(
            file_names
                .iter()
                .map(|file_name| File {
                    length: 500,
                    path: vec![(*file_name).to_string()],
                    md5sum: None,
                })
                .collect(),
        );
        torrent
    }

    /// Creates a minimal torrent without a name.
    pub fn create_torrent_without_name() -> Torrent {
        let mut torrent = Torrent::default();
        torrent.info.length = Some(1000);
        torrent
    }

    /// Creates a `TorrentInfo` for testing.
    pub fn create_torrent_info(
        path: &str,
        torrent_name: Option<&str>,
        rename_to: Option<&str>,
        effective_is_multi_file: bool,
        original_is_multi_file: bool,
        single_included_file: Option<&str>,
    ) -> TorrentInfo {
        let torrent = torrent_name.map_or_else(create_torrent_without_name, create_single_file_torrent);
        TorrentInfo {
            path: PathBuf::from(path),
            torrent,
            bytes: vec![],
            info_hash: "abc123".to_string(),
            original_is_multi_file,
            effective_is_multi_file,
            rename_to: rename_to.map(String::from),
            included_size: 1000,
            excluded_indices: vec![],
            single_included_file: single_included_file.map(String::from),
            original_name: None,
            tags: None,
        }
    }

    /// Creates a `TorrentInfo` with specified excluded file indices for testing.
    pub fn create_torrent_info_with_exclusions(
        path: &str,
        torrent_name: &str,
        file_names: &[&str],
        excluded_indices: Vec<usize>,
        included_size: u64,
    ) -> TorrentInfo {
        let torrent = create_multi_file_torrent(torrent_name, file_names);
        TorrentInfo {
            path: PathBuf::from(path),
            torrent,
            bytes: vec![],
            info_hash: "abc123".to_string(),
            original_is_multi_file: true,
            effective_is_multi_file: excluded_indices.len() < file_names.len()
                && (file_names.len() - excluded_indices.len()) > 1,
            rename_to: None,
            included_size,
            excluded_indices,
            single_included_file: None,
            original_name: Some(torrent_name.to_string()),
            tags: None,
        }
    }
}

#[cfg(test)]
mod test_all_files_excluded {
    use crate::utils::test_torrent_info_helpers::*;

    #[test]
    fn returns_true_when_all_files_excluded() {
        let info = create_torrent_info_with_exclusions(
            "test.torrent",
            "test_folder",
            &["file1.txt", "file2.txt"],
            vec![0, 1],
            0,
        );
        assert!(info.all_files_excluded());
    }

    #[test]
    fn returns_false_when_some_files_remain() {
        let info = create_torrent_info_with_exclusions(
            "test.torrent",
            "test_folder",
            &["file1.txt", "video.mp4"],
            vec![0],
            500,
        );
        assert!(!info.all_files_excluded());
    }

    #[test]
    fn returns_false_when_no_files_excluded() {
        let info = create_torrent_info_with_exclusions(
            "test.torrent",
            "test_folder",
            &["file1.txt", "file2.txt"],
            vec![],
            1000,
        );
        assert!(!info.all_files_excluded());
    }

    #[test]
    fn returns_false_for_single_file_torrent() {
        let info = create_torrent_info("test.torrent", Some("file.txt"), None, false, false, None);
        assert!(!info.all_files_excluded());
    }
}

#[cfg(test)]
mod test_display_name {
    use crate::utils::test_torrent_info_helpers::*;

    #[test]
    fn returns_rename_to_when_set() {
        let info = create_torrent_info(
            "file.torrent",
            Some("Internal.Name.mp4"),
            Some("Custom.Name.mp4"),
            false,
            false,
            None,
        );
        assert_eq!(info.display_name(), "Custom.Name.mp4");
    }

    #[test]
    fn returns_internal_name_when_no_rename() {
        let info = create_torrent_info("file.torrent", Some("Internal.Name.mp4"), None, false, false, None);
        assert_eq!(info.display_name(), "Internal.Name.mp4");
    }

    #[test]
    fn returns_unknown_when_no_name() {
        let info = create_torrent_info("file.torrent", None, None, false, false, None);
        assert_eq!(info.display_name(), "unknown");
    }
}

#[cfg(test)]
mod test_suggested_name_raw_multi_file {
    use crate::utils::test_torrent_info_helpers::*;
    use std::path::PathBuf;

    #[test]
    fn prefers_filename_over_internal_name() {
        let info = create_torrent_info(
            "/path/to/Custom.Filename.torrent",
            Some("Internal.Name"),
            None,
            true,
            true,
            None,
        );
        assert_eq!(info.suggested_name_raw(&[]), "Custom.Filename");
    }

    #[test]
    fn falls_back_to_internal_name_when_no_filename() {
        let mut info = create_torrent_info("", Some("Internal.Name"), None, true, true, None);
        info.path = PathBuf::new();
        assert_eq!(info.suggested_name_raw(&[]), "Internal.Name");
    }

    #[test]
    fn returns_unknown_when_no_names_available() {
        let mut info = create_torrent_info("", None, None, true, true, None);
        info.path = PathBuf::new();
        assert_eq!(info.suggested_name_raw(&[]), "unknown");
    }

    #[test]
    fn ignores_filename_matching_pattern() {
        let info = create_torrent_info(
            "/path/to/[abc123].torrent",
            Some("Internal.Name"),
            None,
            true,
            true,
            None,
        );
        let patterns = vec!["[abc".to_string()];
        assert_eq!(info.suggested_name_raw(&patterns), "Internal.Name");
    }
}

#[cfg(test)]
mod test_suggested_name_raw_single_file {
    use crate::utils::test_torrent_info_helpers::*;
    use std::path::PathBuf;

    #[test]
    fn adds_extension_from_internal_name() {
        let info = create_torrent_info(
            "/path/to/Show.Name.torrent",
            Some("internal.mp4"),
            None,
            false,
            false,
            None,
        );
        assert_eq!(info.suggested_name_raw(&[]), "Show.Name.mp4");
    }

    #[test]
    fn does_not_duplicate_extension() {
        let info = create_torrent_info(
            "/path/to/Show.Name.mp4.torrent",
            Some("internal.mp4"),
            None,
            false,
            false,
            None,
        );
        assert_eq!(info.suggested_name_raw(&[]), "Show.Name.mp4");
    }

    #[test]
    fn uses_single_included_file_extension_for_filtered_multi() {
        let info = create_torrent_info(
            "/path/to/Show.Name.torrent",
            Some("FolderName"),
            None,
            false,
            true,
            Some("video.mkv"),
        );
        assert_eq!(info.suggested_name_raw(&[]), "Show.Name.mkv");
    }

    #[test]
    fn falls_back_to_internal_name() {
        let mut info = create_torrent_info("", Some("Internal.Name.mp4"), None, false, false, None);
        info.path = PathBuf::new();
        assert_eq!(info.suggested_name_raw(&[]), "Internal.Name.mp4");
    }

    #[test]
    fn falls_back_to_single_included_file() {
        let mut info = create_torrent_info("", None, None, false, true, Some("video.mkv"));
        info.path = PathBuf::new();
        assert_eq!(info.suggested_name_raw(&[]), "video.mkv");
    }

    #[test]
    fn returns_unknown_when_no_names_available() {
        let mut info = create_torrent_info("", None, None, false, false, None);
        info.path = PathBuf::new();
        assert_eq!(info.suggested_name_raw(&[]), "unknown");
    }

    #[test]
    fn case_insensitive_extension_check() {
        let info = create_torrent_info(
            "/path/to/Show.Name.MP4.torrent",
            Some("internal.mp4"),
            None,
            false,
            false,
            None,
        );
        assert_eq!(info.suggested_name_raw(&[]), "Show.Name.MP4");
    }
}
