//! Matching files and input directories to existing destination directories by name.
//!
//! `DirectoryMatcher` ranks destination directories, matches file names to directory names with
//! and without ignored prefixes, and applies custom mappings.
//! `NormalizedFilename` maps a normalized name match back to the byte range in the original file name,
//! so prompts can highlight the matched part.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use anyhow::Result;

use cli_tools_common::{MatchRange, path_to_filename_string};

use crate::config::{Config, CustomMapping};
use crate::roots::Roots;
use crate::types::{DirectoryInfo, FileInfo};
use crate::utils;

/// A file index and the filename range that matched a destination.
#[derive(Debug, Clone, Copy)]
pub struct FileMatch {
    /// Index into the matched file list.
    pub index: usize,
    /// Byte range of the match in the file name.
    pub range: MatchRange,
}

/// A file listed for a move prompt and its already computed highlight range.
#[derive(Debug, Clone)]
pub struct ListedFile {
    /// Path of the file.
    pub path: PathBuf,
    /// Byte range of the match in the file name.
    pub range: MatchRange,
}

/// A normalized filename with byte mappings back to the original filename.
#[derive(Debug, Clone)]
pub struct NormalizedFilename {
    /// Lowercase filename with dots and spaces removed.
    text: String,
    /// Original byte range of each character in `text`.
    original_ranges: Vec<(usize, usize)>,
}

/// Matches files and directories to destination directories.
#[derive(Debug, Clone, Copy)]
pub struct DirectoryMatcher<'a> {
    /// Roots used to rank destination directories by depth.
    roots: &'a Roots,
    /// Options holding the name filter rules and custom mappings.
    config: &'a Config,
}

/// Precomputed destination directory name variants used while matching files.
struct DestinationMatchInfo<'a> {
    /// Index into the original destination directory list.
    index: usize,
    /// Lowercase directory name with dots converted to spaces.
    spaced_name: &'a str,
    /// Lowercase directory name with spaces and dots removed.
    normalized_name: String,
    /// Prefix-stripped spaced name, present only when stripping changed the name.
    stripped_spaced_name: Option<String>,
    /// Prefix-stripped normalized name, present only when stripping changed the name.
    stripped_normalized_name: Option<String>,
}

impl NormalizedFilename {
    /// Build a normalized filename while retaining the original byte range for each character.
    #[must_use]
    pub fn new(filename: &str) -> Self {
        let mut text = String::with_capacity(filename.len());
        let mut original_ranges = Vec::with_capacity(filename.chars().count());
        for (start, character) in filename.char_indices() {
            let end = start + character.len_utf8();
            for lowercase_character in character.to_lowercase() {
                if !matches!(lowercase_character, ' ' | '.') {
                    text.push(lowercase_character);
                    original_ranges.push((start, end));
                }
            }
        }

        Self { text, original_ranges }
    }

    /// Map a normalized substring match back to its original filename byte range.
    #[must_use]
    pub fn find_match(&self, normalized_name: &str) -> Option<MatchRange> {
        if normalized_name.is_empty() {
            return None;
        }

        let normalized_start = self.text.find(normalized_name)?;
        let normalized_end = normalized_start + normalized_name.len();
        let start_index = self.text.get(..normalized_start)?.chars().count();
        let end_index = self.text.get(..normalized_end)?.chars().count();

        Some(MatchRange {
            start: self.original_ranges.get(start_index)?.0,
            end: self.original_ranges.get(end_index.checked_sub(1)?)?.1,
        })
    }
}

impl<'a> DirectoryMatcher<'a> {
    /// Create a matcher for destination directories under the given roots.
    #[must_use]
    pub const fn new(roots: &'a Roots, config: &'a Config) -> Self {
        Self { roots, config }
    }

    /// Return directory indices in match priority order.
    ///
    /// Deeper output directories are preferred first
    /// so recursive scans route to the most nested matching destination.
    /// Directory name length breaks same-depth ties, favoring more specific names before shorter broad matches.
    #[must_use]
    pub fn sorted_directory_indices(&self, directories: &[DirectoryInfo]) -> Vec<usize> {
        let mut indexed_directories: Vec<_> = directories.iter().enumerate().collect();
        indexed_directories.sort_by(|(_, left), (_, right)| {
            self.roots
                .output_depth(&right.path)
                .cmp(&self.roots.output_depth(&left.path))
                .then_with(|| right.name.chars().count().cmp(&left.name.chars().count()))
                .then_with(|| left.name.cmp(&right.name))
        });
        indexed_directories.into_iter().map(|(index, _)| index).collect()
    }

    /// Match files to directories based on normalized name matching.
    ///
    /// Returns a map from directory index into `directories` to the matching indices in `files`.
    /// A file can match several directories.
    /// Directory priority is controlled by `sorted_directory_indices`,
    /// so recursive scans prefer deeper destinations before broader shallow matches.
    #[must_use]
    pub fn match_files(&self, files: &[PathBuf], directories: &[DirectoryInfo]) -> HashMap<usize, Vec<FileMatch>> {
        let name_filter = self.config.name_filter();
        let mut matches: HashMap<usize, Vec<FileMatch>> = HashMap::new();
        let destination_match_info = self.collect_destination_match_info(directories);

        for (file_index, file_path) in files.iter().enumerate() {
            let Some(file_name) = file_path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };

            // Build the lightweight matching forms used before highlight ranges are needed.
            let file_name_spaced = file_name.replace('.', " ").to_lowercase();
            let file_name_concat = file_name.replace('.', "").to_lowercase();
            let file_name_spaced_stripped = name_filter.strip_ignored_prefixes(&file_name_spaced);
            let mut normalized_filename = None;

            for destination_info in &destination_match_info {
                let Some(normalized_name) = destination_info.matched_normalized_name(
                    &file_name_spaced,
                    &file_name_spaced_stripped,
                    &file_name_concat,
                ) else {
                    continue;
                };

                let normalized_filename = normalized_filename.get_or_insert_with(|| NormalizedFilename::new(file_name));
                if let Some(range) = normalized_filename.find_match(normalized_name) {
                    matches.entry(destination_info.index).or_default().push(FileMatch {
                        index: file_index,
                        range,
                    });
                }
            }
        }

        matches
    }

    /// Match files to the existing directories named by the configured custom mappings.
    ///
    /// Each file uses the first mapping whose pattern it contains and whose directory exists.
    /// Returns a map from directory index into `directories` to the matching indices in `files`.
    #[must_use]
    pub fn match_custom_mappings(
        &self,
        files: &[PathBuf],
        directories: &[DirectoryInfo],
    ) -> HashMap<usize, Vec<FileMatch>> {
        let mut custom_matches: HashMap<usize, Vec<FileMatch>> = HashMap::new();

        for (file_index, file_path) in files.iter().enumerate() {
            let Some(file_name) = file_path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };

            let file_name_normalized = utils::normalize_name(file_name);
            for mapping in &self.config.custom_mappings {
                if file_name_normalized.contains(&mapping.pattern)
                    && let Some(directory_index) = self.find_directory_for_custom_mapping(mapping, directories)
                    && let Some(range) = NormalizedFilename::new(file_name).find_match(&mapping.pattern)
                {
                    custom_matches.entry(directory_index).or_default().push(FileMatch {
                        index: file_index,
                        range,
                    });
                    break;
                }
            }
        }

        custom_matches
    }

    /// Find the highest-priority output directory for a custom mapping target name.
    ///
    /// When the same mapped directory exists under multiple output roots,
    /// this uses the same directory priority order as normal file matching.
    #[must_use]
    pub fn find_directory_for_custom_mapping(
        &self,
        mapping: &CustomMapping,
        directories: &[DirectoryInfo],
    ) -> Option<usize> {
        let target_normalized = utils::normalize_name(&mapping.directory);

        self.sorted_directory_indices(directories)
            .into_iter()
            .find(|&directory_index| {
                directories
                    .get(directory_index)
                    .is_some_and(|directory| utils::normalize_name(&directory.name) == target_normalized)
            })
    }

    /// Match input directories to destination directories with the same normalized name.
    ///
    /// When several destination directories match, the same priority as file matching is used:
    /// deeper nested output directories first, then longer directory names.
    /// A source directory never matches itself.
    /// Returns each matched destination with its source directories, sorted by destination name.
    ///
    /// # Errors
    /// Returns an error if a directory index is out of bounds.
    pub fn match_input_directories<'directories>(
        &self,
        source_directories: Vec<PathBuf>,
        directories: &'directories [DirectoryInfo],
    ) -> Result<Vec<(&'directories DirectoryInfo, Vec<PathBuf>)>> {
        let directory_indices = self.sorted_directory_indices(directories);
        let mut matches: BTreeMap<usize, Vec<PathBuf>> = BTreeMap::new();

        for source_directory in source_directories {
            let source_name = DirectoryInfo::new(source_directory.clone()).name;
            for &directory_index in &directory_indices {
                let target_directory = directories
                    .get(directory_index)
                    .ok_or_else(|| anyhow::anyhow!("Directory match index {directory_index} is out of bounds"))?;
                if source_name == target_directory.name
                    && !cli_tools_common::paths_refer_to_same_file(&source_directory, &target_directory.path)
                {
                    matches.entry(directory_index).or_default().push(source_directory);
                    break;
                }
            }
        }

        let mut groups = matches
            .into_iter()
            .map(|(index, source_directories)| {
                let target_directory = directories
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("Directory match index {index} is out of bounds"))?;
                Ok((target_directory, source_directories))
            })
            .collect::<Result<Vec<_>>>()?;
        groups.sort_by(|left, right| left.0.name.cmp(&right.0.name));
        Ok(groups)
    }

    /// Precompute destination directory name variants used by file matching.
    fn collect_destination_match_info<'directories>(
        &self,
        directories: &'directories [DirectoryInfo],
    ) -> Vec<DestinationMatchInfo<'directories>> {
        let name_filter = self.config.name_filter();
        self.sorted_directory_indices(directories)
            .into_iter()
            .filter_map(|directory_index| {
                let directory_name = directories.get(directory_index)?.name.as_str();
                if name_filter.is_ignored_prefix(directory_name) {
                    return None;
                }

                let normalized_name = utils::normalize_name(directory_name);
                if name_filter.should_skip_destination_directory(directory_name, &normalized_name) {
                    return None;
                }
                let stripped_name = name_filter.strip_ignored_prefixes(directory_name);
                let (stripped_spaced_name, stripped_normalized_name) = if stripped_name.as_ref() == directory_name {
                    (None, None)
                } else {
                    let stripped_name = stripped_name.into_owned();
                    let stripped_normalized_name = utils::normalize_name(&stripped_name);
                    (Some(stripped_name), Some(stripped_normalized_name))
                };

                Some(DestinationMatchInfo {
                    index: directory_index,
                    spaced_name: directory_name,
                    normalized_name,
                    stripped_spaced_name,
                    stripped_normalized_name,
                })
            })
            .collect()
    }
}

impl DestinationMatchInfo<'_> {
    /// Return the normalized directory-name variant responsible for a filename match.
    fn matched_normalized_name<'a>(
        &'a self,
        file_name_spaced: &str,
        file_name_spaced_stripped: &str,
        file_name_concat: &str,
    ) -> Option<&'a str> {
        if file_name_spaced_stripped.contains(self.spaced_name) || file_name_concat.contains(&self.normalized_name) {
            return Some(&self.normalized_name);
        }

        let stripped_name_matches = self.stripped_spaced_name.as_deref().is_some_and(|stripped_name| {
            file_name_spaced_stripped.contains(stripped_name) || file_name_spaced.contains(stripped_name)
        }) || self
            .stripped_normalized_name
            .as_ref()
            .is_some_and(|stripped_name| file_name_concat.contains(stripped_name));

        stripped_name_matches
            .then_some(self.stripped_normalized_name.as_deref())
            .flatten()
    }
}

/// Group files by the first custom mapping whose pattern their original name contains.
///
/// Used in create mode, where the mapped directory is created when it does not exist.
/// Returns the mapped directory names in sorted order with their files and highlight ranges.
#[must_use]
pub fn group_files_by_custom_mapping(
    files_with_names: &[FileInfo<'_>],
    custom_mappings: &[CustomMapping],
) -> BTreeMap<String, Vec<ListedFile>> {
    let mut mapping_groups: BTreeMap<String, Vec<ListedFile>> = BTreeMap::new();

    for file_info in files_with_names {
        let file_name_normalized = file_info.original_name.to_lowercase().replace(['.', ' '], "");
        let Some(mapping) = custom_mappings
            .iter()
            .find(|mapping| file_name_normalized.contains(&mapping.pattern))
        else {
            continue;
        };

        let path = file_info.path_buf();
        let filename = path_to_filename_string(&path);
        if let Some(range) = NormalizedFilename::new(&filename).find_match(&mapping.pattern) {
            mapping_groups
                .entry(mapping.directory.clone())
                .or_default()
                .push(ListedFile { path, range });
        }
    }

    mapping_groups
}

#[cfg(test)]
mod test_directory_matching {
    use super::*;
    use crate::test_helpers::*;

    #[test]
    fn numeric_and_short_directory_names_are_not_offered() {
        let fixture = make_test_fixture(Vec::new());
        let dirs = make_test_dirs(&[
            "2024", "12345", "0", "A", "AB", "ABC", "A B C", "Show", "A1BC", "A B C D", "ÄÖÜ",
        ]);
        let files = make_file_paths(&["2024.12345.0.A.AB.ABC.Show.A1BC.ABCD.ÄÖÜ.mp4"]);

        let result = fixture.matcher().match_files(&files, &dirs);

        assert_eq!(result.len(), 3);
        for index in [7, 8, 9] {
            assert_eq!(result.get(&index).expect("valid directory should match").len(), 1);
        }
        let filtered = fixture.name_filter().partition_destination_directories(dirs).0;
        assert_eq!(filtered.len(), 3);
    }

    #[test]
    fn numeric_and_short_prefix_stripped_directory_names_are_not_offered() {
        let fixture = make_test_fixture_with_ignores(Vec::new(), vec!["dl"]);
        let dirs = make_test_dirs(&["DL 2024", "DL 0", "DL A", "DL ABC", "DL Show"]);
        let files = make_file_paths(&["2024.0.A.ABC.Show.mp4"]);

        let result = fixture.matcher().match_files(&files, &dirs);

        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get(&4).expect("four-character stripped name should match").len(),
            1
        );
        let filtered = fixture.name_filter().partition_destination_directories(dirs).0;
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn basic_match() {
        let fixture = make_test_fixture(Vec::new());
        let dirs = make_test_dirs(&["Certain Name"]);
        let files = make_file_paths(&[
            "Something.else.Certain.Name.video.1.mp4",
            "Certain.Name.Example.video.2.mp4",
            "Another.Certain.Name.Example.video.3.mp4",
            "Another.Name.Example.video.3.mp4",
            "Cert.Name.Example.video.3.mp4",
        ]);
        let result = fixture.matcher().match_files(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&0).unwrap().len(), 3);
    }

    #[test]
    fn no_matches() {
        let fixture = make_test_fixture(Vec::new());
        let dirs = make_test_dirs(&["Unknown Dir"]);
        let files = make_file_paths(&["Some.File.mp4", "Other.File.mp4"]);
        let result = fixture.matcher().match_files(&files, &dirs);
        assert!(result.is_empty());
    }

    #[test]
    fn ignored_group_name_directory_not_offered() {
        let fixture = Fixture::new(
            PathBuf::from("."),
            Config::test_with_ignored_group_names(vec!["episode"]),
        );
        let dirs = fixture
            .name_filter()
            .partition_destination_directories(make_test_dirs(&["Episode"]))
            .0;
        let files = make_file_paths(&["Studio.Episode.01.mp4"]);

        let result = fixture.matcher().match_files(&files, &dirs);

        assert!(result.is_empty());
    }

    #[test]
    fn prefix_stripped_ignored_group_name_directory_not_offered() {
        let fixture = Fixture::new(
            PathBuf::from("."),
            Config::test_with_prefix_ignores_and_ignored_group_names(vec!["dl"], vec!["studio"]),
        );
        let dirs = fixture
            .name_filter()
            .partition_destination_directories(make_test_dirs(&["DL Studio"]))
            .0;
        let files = make_file_paths(&["DL.Studio.Episode.01.mp4"]);

        let result = fixture.matcher().match_files(&files, &dirs);

        assert!(result.is_empty());
    }

    #[test]
    fn multiple_dirs() {
        let fixture = make_test_fixture(Vec::new());
        let dirs = make_test_dirs(&["Show A", "Show B"]);
        let files = make_file_paths(&["Show.A.ep1.mp4", "Show.B.ep1.mp4", "Show.A.ep2.mp4"]);
        let result = fixture.matcher().match_files(&files, &dirs);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn case_insensitive() {
        let fixture = make_test_fixture(Vec::new());
        let dirs = make_test_dirs(&["Movie Name"]);
        let files = make_file_paths(&["MOVIE.NAME.part1.mp4", "movie.name.part2.mp4", "Movie.Name.part3.mp4"]);
        let result = fixture.matcher().match_files(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&0).unwrap().len(), 3);
    }

    #[test]
    fn partial_match() {
        let fixture = make_test_fixture(Vec::new());
        let dirs = make_test_dirs(&["Show Name", "Show"]);
        let files = make_file_paths(&["Show.Name.ep1.mp4", "Show.Other.ep1.mp4"]);
        let result = fixture.matcher().match_files(&files, &dirs);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn file_can_match_multiple_directories() {
        let fixture = make_test_fixture(Vec::new());
        let dirs = make_test_dirs(&["Show", "Show Name"]);
        let files = make_file_paths(&["Show.Name.Episode.mp4"]);

        let result = fixture.matcher().match_files(&files, &dirs);

        assert_eq!(result.len(), 2);
        assert_eq!(result.get(&0).unwrap().len(), 1);
        assert_eq!(result.get(&1).unwrap().len(), 1);
    }

    #[test]
    fn empty_files() {
        let fixture = make_test_fixture(Vec::new());
        let dirs = make_test_dirs(&["Show Name"]);
        let files: Vec<PathBuf> = Vec::new();
        let result = fixture.matcher().match_files(&files, &dirs);
        assert!(result.is_empty());
    }

    #[test]
    fn empty_dirs() {
        let fixture = make_test_fixture(Vec::new());
        let dirs: Vec<DirectoryInfo> = Vec::new();
        let files = make_file_paths(&["Show.Name.ep1.mp4"]);
        let result = fixture.matcher().match_files(&files, &dirs);
        assert!(result.is_empty());
    }

    #[test]
    fn dots_replaced_with_spaces() {
        let fixture = make_test_fixture(Vec::new());
        let dirs = make_test_dirs(&["my show name"]);
        let files = make_file_paths(&["my.show.name.ep1.mp4", "my.show.name.ep2.mp4"]);
        let result = fixture.matcher().match_files(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&0).unwrap().len(), 2);
    }

    #[test]
    fn mixed_separators() {
        let fixture = make_test_fixture(Vec::new());
        let dirs = make_test_dirs(&["show name"]);
        let files = make_file_paths(&["Show.Name.ep1.mp4", "show_name_ep2.mp4", "Show-Name-ep3.mp4"]);
        let result = fixture.matcher().match_files(&files, &dirs);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn with_prefix_ignore() {
        let fixture = make_test_fixture_with_ignores(Vec::new(), vec!["www"]);
        let dirs = make_test_dirs(&["example"]);
        let files = make_file_paths(&["www.example.file.mp4"]);
        let result = fixture.matcher().match_files(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&0).unwrap().len(), 1);
    }

    #[test]
    fn with_repeated_prefix_ignore() {
        let fixture = make_test_fixture_with_ignores(Vec::new(), vec!["prefix"]);
        let dirs = make_test_dirs(&["name"]);
        let files = make_file_paths(&["prefix.prefix.name.file.mp4"]);
        let result = fixture.matcher().match_files(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&0).unwrap().len(), 1);
    }

    #[test]
    fn with_multiple_prefix_ignores() {
        let fixture = make_test_fixture_with_ignores(Vec::new(), vec!["www", "ftp"]);
        let dirs = make_test_dirs(&["example"]);
        let files = make_file_paths(&["www.ftp.example.file.mp4", "ftp.example.other.mp4"]);
        let result = fixture.matcher().match_files(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&0).unwrap().len(), 2);
    }

    #[test]
    fn prefix_ignore_applied_to_both() {
        let fixture = make_test_fixture_with_ignores(Vec::new(), vec!["release"]);
        let dirs = make_test_dirs(&["release show name"]);
        let files = make_file_paths(&["release.show.name.ep1.mp4"]);
        let result = fixture.matcher().match_files(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&0).unwrap().len(), 1);
    }

    #[test]
    fn file_has_prefix_dir_does_not() {
        let fixture = make_test_fixture_with_ignores(Vec::new(), vec!["www"]);
        let dirs = make_test_dirs(&["example"]);
        let files = make_file_paths(&["www.example.file.mp4"]);
        let result = fixture.matcher().match_files(&files, &dirs);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn dir_has_prefix_file_does_not() {
        let fixture = make_test_fixture_with_ignores(Vec::new(), vec!["www"]);
        let dirs = make_test_dirs(&["www example"]);
        let files = make_file_paths(&["example.file.mp4"]);
        let result = fixture.matcher().match_files(&files, &dirs);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn special_characters_in_names() {
        let fixture = make_test_fixture(Vec::new());
        let dirs = make_test_dirs(&["show's name"]);
        let files = make_file_paths(&["show's.name.ep1.mp4"]);
        let result = fixture.matcher().match_files(&files, &dirs);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn concatenated_filename_matches_spaced_directory() {
        // Directory "Some Name" should match both "Some.Name." and "SomeName" files
        let fixture = make_test_fixture(Vec::new());
        let dirs = make_test_dirs(&["Some Name"]);
        let files = make_file_paths(&[
            "Some.Name.Episode.01.mp4",
            "SomeName.Episode.02.mp4",
            "SOMENAME.Episode.03.mp4",
            "somename.episode.04.mp4",
        ]);
        let result = fixture.matcher().match_files(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get(&0).unwrap().len(),
            4,
            "All 4 files should match directory 'Some Name'"
        );
    }

    #[test]
    fn mixed_concatenated_and_dotted_filenames() {
        // Mix of naming styles should all match the same directory
        let fixture = make_test_fixture(Vec::new());
        let dirs = make_test_dirs(&["Photo Lab"]);
        let files = make_file_paths(&[
            "Photo.Lab.Image.01.jpg",
            "PhotoLab.Image.02.jpg",
            "Photolab.Image.03.jpg",
            "PHOTOLAB.Image.04.jpg",
            "photo.lab.image.05.jpg",
        ]);
        let result = fixture.matcher().match_files(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get(&0).unwrap().len(),
            5,
            "All 5 files should match directory 'Photo Lab'"
        );
    }

    #[test]
    fn concatenated_directory_matches_dotted_files() {
        // Directory without spaces should match dot-separated files
        let fixture = make_test_fixture(Vec::new());
        let dirs = make_test_dirs(&["SomeName"]);
        let files = make_file_paths(&["Some.Name.Episode.01.mp4", "SomeName.Episode.02.mp4"]);
        let result = fixture.matcher().match_files(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get(&0).unwrap().len(),
            2,
            "Both files should match directory 'SomeName'"
        );
    }

    #[test]
    fn concatenated_directory_matches_dotted_files_without_prefix() {
        // Directory "SomeName" (no spaces) should match files like "some.name.file.mp4"
        let fixture = make_test_fixture(Vec::new());
        let dirs = make_test_dirs(&["SomeName"]);
        let files = make_file_paths(&[
            "some.name.file.01.mp4",
            "Some.Name.file.02.mp4",
            "SOME.NAME.file.03.mp4",
        ]);
        let result = fixture.matcher().match_files(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get(&0).unwrap().len(),
            3,
            "All 3 dot-separated files should match concatenated directory 'SomeName'"
        );
    }

    #[test]
    fn concatenated_directory_with_dotted_files_and_prefix() {
        // Directory "SomeName" should match files like "prefix.some.name.file.mp4"
        let fixture = make_test_fixture_with_ignores(Vec::new(), vec!["prefix"]);
        let dirs = make_test_dirs(&["SomeName"]);
        let files = make_file_paths(&[
            "prefix.some.name.file.01.mp4",
            "prefix.SomeName.file.02.mp4",
            "some.name.file.03.mp4",
            "SomeName.file.04.mp4",
        ]);
        let result = fixture.matcher().match_files(&files, &dirs);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get(&0).unwrap().len(),
            4,
            "All 4 files should match directory 'SomeName'"
        );
    }

    #[test]
    fn numbers_in_directory_name() {
        let fixture = make_test_fixture(Vec::new());
        let dirs = make_test_dirs(&["2024 show", "show 2024"]);
        let files = make_file_paths(&["2024.show.ep1.mp4", "show.2024.ep1.mp4"]);
        let result = fixture.matcher().match_files(&files, &dirs);
        assert_eq!(result.len(), 2);
    }
}

#[cfg(test)]
mod test_filename_directory_highlight {
    use super::*;
    use crate::test_helpers::*;

    #[test]
    fn finds_dot_separated_directory_name() {
        let range = NormalizedFilename::new("Prefix.Certain.Name.Video.mp4")
            .find_match("certainname")
            .expect("directory name should match filename");

        assert_eq!(
            range.extract_from("Prefix.Certain.Name.Video.mp4"),
            Some("Certain.Name")
        );
    }

    #[test]
    fn finds_concatenated_directory_name() {
        let range = NormalizedFilename::new("Prefix.CertainName.Video.mp4")
            .find_match("certainname")
            .expect("directory name should match filename");

        assert_eq!(range.extract_from("Prefix.CertainName.Video.mp4"), Some("CertainName"));
    }

    #[test]
    fn returns_none_when_directory_name_is_absent() {
        assert!(
            NormalizedFilename::new("Some.Other.Video.mp4")
                .find_match("certainname")
                .is_none()
        );
    }

    #[test]
    fn destination_matching_preserves_space_boundaries() {
        let fixture = make_test_fixture(Vec::new());
        let directories = make_test_dirs(&["ab c"]);
        let files = make_file_paths(&["a bc.episode.mp4"]);

        assert!(fixture.matcher().match_files(&files, &directories).is_empty());
    }

    #[test]
    fn directory_matching_carries_the_highlight_range() {
        let fixture = make_test_fixture(Vec::new());
        let directories = make_test_dirs(&["certain name"]);
        let files = make_file_paths(&["Prefix.Certain.Name.Video.mp4"]);

        let matches = fixture.matcher().match_files(&files, &directories);
        let file_match = matches
            .get(&0)
            .and_then(|file_matches| file_matches.first())
            .expect("file should match directory");
        let filename = path_to_filename_string(&files[file_match.index]);

        assert_eq!(file_match.range.extract_from(&filename), Some("Certain.Name"));
    }
}
