//! Collecting `.torrent` files from the input paths.
//!
//! Searches the input files and directories, optionally recursively,
//! and keeps the torrent files whose names pass the include and exclude globs.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use regex::Regex;
use walkdir::WalkDir;

/// Finds the torrent files to process from the input paths.
#[derive(Debug, Clone, Default)]
pub struct TorrentPathCollector {
    /// Input files and directories. The current directory is used when empty.
    pub input_paths: Vec<PathBuf>,
    /// Recurse into subdirectories when searching for torrent files.
    pub recurse: bool,
    /// Torrent file name patterns to include, any match selects the file.
    include_patterns: Vec<Regex>,
    /// Torrent file name patterns to exclude.
    exclude_patterns: Vec<Regex>,
    /// The first malformed filter, if any.
    invalid_pattern: Option<String>,
}

impl TorrentPathCollector {
    /// Create a collector for the input paths with include and exclude file name globs.
    ///
    /// The globs match case-insensitively,
    /// and a pattern without glob characters matches as a substring.
    /// Malformed filters are reported by `collect`.
    #[must_use]
    pub fn new(input_paths: Vec<PathBuf>, recurse: bool, include: &[String], exclude: &[String]) -> Self {
        let mut collector = Self {
            input_paths,
            recurse,
            ..Self::default()
        };
        match compile_name_patterns(include, "include")
            .and_then(|include| compile_name_patterns(exclude, "exclude").map(|exclude| (include, exclude)))
        {
            Ok((include, exclude)) => {
                collector.include_patterns = include;
                collector.exclude_patterns = exclude;
            }
            Err(error) => collector.invalid_pattern = Some(error),
        }
        collector
    }

    /// Collect torrent file paths from the input paths.
    ///
    /// If no paths are provided, the current working directory is used.
    /// If a path is a directory, it is searched for `.torrent` files.
    /// If a path is a `.torrent` file, it is used directly.
    /// If `recurse` is true, directories are searched recursively.
    /// The result is sorted and has no duplicates.
    ///
    /// # Errors
    /// Returns an error for a malformed filter, an unresolved path, or an unreadable directory.
    pub fn collect(&self) -> Result<Vec<PathBuf>> {
        if let Some(error) = &self.invalid_pattern {
            anyhow::bail!("{error}");
        }

        let mut torrent_paths = Vec::new();

        if self.input_paths.is_empty() {
            let current_directory = cli_tools_common::resolve_input_path(None)?;
            collect_torrents_from_directory(&current_directory, &mut torrent_paths, self.recurse)?;
        } else {
            for path in &self.input_paths {
                let resolved = cli_tools_common::resolve_required_input_path(path)?;
                if resolved.is_dir() {
                    collect_torrents_from_directory(&resolved, &mut torrent_paths, self.recurse)?;
                } else if is_torrent_file(&resolved) {
                    torrent_paths.push(resolved);
                }
            }
        }

        torrent_paths.retain(|path| self.matches_name_filters(path));
        torrent_paths.sort_unstable();
        torrent_paths.dedup();

        Ok(torrent_paths)
    }

    /// Check whether the file name passes the include and exclude patterns.
    #[must_use]
    pub fn matches_name_filters(&self, path: &Path) -> bool {
        if self.invalid_pattern.is_some() {
            return false;
        }

        let file_name = path
            .file_name()
            .map_or_default(|name| name.to_string_lossy().to_lowercase());
        let included = self.include_patterns.is_empty()
            || self.include_patterns.iter().any(|pattern| pattern.is_match(&file_name));
        included && !self.exclude_patterns.iter().any(|pattern| pattern.is_match(&file_name))
    }
}

/// Collect all `.torrent` files from the given directory.
///
/// If `recurse` is true, subdirectories are searched recursively, skipping hidden entries
/// and the `downloaded` archive directory.
fn collect_torrents_from_directory(directory: &Path, torrent_paths: &mut Vec<PathBuf>, recurse: bool) -> Result<()> {
    if recurse {
        for entry in WalkDir::new(directory)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                if entry.depth() == 0 {
                    return true;
                }
                if cli_tools_common::should_skip_entry(entry) {
                    return false;
                }
                !(entry.file_type().is_dir()
                    && entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.eq_ignore_ascii_case(crate::utils::DOWNLOADED_DIRECTORY_NAME)))
            })
            .filter_map(Result::ok)
        {
            if entry.file_type().is_file() && is_torrent_file(entry.path()) {
                torrent_paths.push(entry.into_path());
            }
        }
    } else {
        for entry in
            fs::read_dir(directory).with_context(|| format!("Failed to read directory: {}", directory.display()))?
        {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let path = entry.path();
            if file_type.is_file() && is_torrent_file(&path) {
                torrent_paths.push(path);
            }
        }
    }
    Ok(())
}

/// Check if the given path is a `.torrent` file.
fn is_torrent_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("torrent"))
}

/// Compile case-insensitive file name globs.
/// A pattern without glob characters matches as a substring.
fn compile_name_patterns(patterns: &[String], kind: &str) -> Result<Vec<Regex>, String> {
    patterns
        .iter()
        .map(|pattern| {
            let lowercase = pattern.to_lowercase();
            let glob = if lowercase.contains(['*', '?', '[', '{']) {
                lowercase
            } else {
                format!("*{lowercase}*")
            };
            cli_tools_common::glob_to_regex(&glob).ok_or_else(|| format!("Invalid {kind} glob: {pattern:?}"))
        })
        .collect()
}

#[cfg(test)]
mod test_torrent_path_collector {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn name_filters_are_additive_globs() {
        let collector = TorrentPathCollector::new(
            Vec::new(),
            false,
            &["show.*".to_string(), "Movie".to_string()],
            &["*sample*".to_string()],
        );

        assert!(collector.matches_name_filters(Path::new("dir/Show.S01.torrent")));
        assert!(collector.matches_name_filters(Path::new("The.Movie.2024.torrent")));
        assert!(!collector.matches_name_filters(Path::new("Other.torrent")));
        assert!(!collector.matches_name_filters(Path::new("Show.Sample.torrent")));
    }

    #[test]
    fn malformed_include_is_reported_instead_of_selecting_everything() -> Result<()> {
        let directory = TempDir::new()?;
        fs::write(directory.path().join("Friendly.torrent"), b"")?;
        let inputs = vec![directory.path().to_path_buf()];
        let nonmatching = TorrentPathCollector::new(inputs.clone(), false, &["Other*".to_owned()], &[]);
        assert_eq!(nonmatching.collect()?, Vec::<PathBuf>::new());

        let invalid = TorrentPathCollector::new(inputs, false, &["[abc".to_owned()], &[]);
        let error = invalid
            .collect()
            .expect_err("an invalid include must not become unrestricted");
        assert!(error.to_string().contains("include"));
        assert!(error.to_string().contains("[abc"));
        assert!(!invalid.matches_name_filters(Path::new("Friendly.torrent")));
        Ok(())
    }

    #[test]
    fn collects_torrent_files_recursively_only_when_asked() -> Result<()> {
        let temp_directory = TempDir::new()?;
        let root = temp_directory.path();
        fs::create_dir_all(root.join("nested"))?;
        fs::write(root.join("b.torrent"), b"")?;
        fs::write(root.join("a.TORRENT"), b"")?;
        fs::write(root.join("notes.txt"), b"")?;
        fs::write(root.join("nested").join("c.torrent"), b"")?;

        let flat = TorrentPathCollector::new(vec![root.to_path_buf()], false, &[], &[]).collect()?;
        let recursive = TorrentPathCollector::new(vec![root.to_path_buf()], true, &[], &[]).collect()?;

        let names = |paths: &[PathBuf]| {
            paths
                .iter()
                .map(|path| cli_tools_common::path_to_filename_string(path))
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&flat), vec!["a.TORRENT", "b.torrent"]);
        assert_eq!(names(&recursive), vec!["a.TORRENT", "b.torrent", "c.torrent"]);
        Ok(())
    }

    #[test]
    fn recursive_collect_skips_downloaded_and_hidden_directories() -> Result<()> {
        let temp_directory = TempDir::new()?;
        let root = temp_directory.path();
        fs::create_dir_all(root.join("downloaded"))?;
        fs::create_dir_all(root.join(".hidden"))?;
        fs::create_dir_all(root.join("nested"))?;
        fs::write(root.join("downloaded").join("old.torrent"), b"")?;
        fs::write(root.join(".hidden").join("secret.torrent"), b"")?;
        fs::write(root.join("nested").join("new.torrent"), b"")?;

        let found = TorrentPathCollector::new(vec![root.to_path_buf()], true, &[], &[]).collect()?;
        let names: Vec<_> = found
            .iter()
            .map(|path| cli_tools_common::path_to_filename_string(path))
            .collect();
        assert_eq!(names, vec!["new.torrent"]);
        Ok(())
    }
}
