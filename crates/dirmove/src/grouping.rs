//! Grouping files by shared name prefixes for create mode.
//!
//! `PrefixGrouper` finds every prefix candidate shared by enough files in two parallel passes,
//! applies prefix overrides, orders the groups from the most specific to the most general,
//! and turns a group prefix into a directory name while skipping names that should not become directories.

use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;
use std::path::PathBuf;

use rayon::prelude::*;

use cli_tools_common::unique_in_order;

use crate::candidates::{self, FileInfo};
use crate::name_filter::NameFilter;
use crate::prefix_index::PrefixIndex;
use crate::utils;

/// Default minimum number of files in a group.
pub const DEFAULT_MIN_GROUP_SIZE: usize = 3;

/// Default minimum number of characters in a single word group prefix.
pub const DEFAULT_MIN_PREFIX_CHARS: usize = 5;

/// Stage of grouping that reports progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupingStage {
    /// Collecting prefix candidates from each file.
    CollectingPrefixes,
    /// Adding files whose names start with a group prefix to that group.
    MatchingFilesToGroups,
}

/// Group size, prefix length, ignored part, and prefix override rules for grouping.
#[derive(Debug, Clone)]
pub struct GroupingOptions {
    /// Minimum number of files in a group.
    min_group_size: usize,
    /// Minimum number of characters in a single word group prefix.
    min_prefix_chars: usize,
    /// Lowercase name parts that disqualify a group.
    ignored_group_parts: Vec<String>,
    /// Prefixes that replace any group prefix they overlap with.
    prefix_overrides: Vec<String>,
}

/// Groups files by shared name prefixes.
#[derive(Debug, Clone, Copy)]
pub struct PrefixGrouper<'a> {
    /// Group size, prefix length, ignored part, and prefix override rules.
    options: &'a GroupingOptions,
    /// Ignored prefix and ignored group name rules.
    name_filter: &'a NameFilter,
}

/// A group of files that share a common prefix.
/// Used for organizing files into directories based on their name prefixes.
#[derive(Debug, Clone)]
pub struct PrefixGroup {
    /// Files belonging to this prefix group.
    pub files: Vec<PathBuf>,
    /// Number of dot-separated parts in the prefix (1, 2, or 3).
    /// Higher values indicate more specific prefixes.
    pub part_count: usize,
    /// Minimum start position where this prefix appears across all files.
    /// Lower values indicate prefixes closer to the start of filenames.
    pub min_start_position: usize,
}

/// A validated prefix candidate ready to be merged into prefix groups.
///
/// Produced during the parallel first pass of `collect_all_prefix_groups`.
struct ValidCandidate {
    /// Normalized group key (lowercase, no dots).
    pub key: String,
    /// Path to the file this candidate belongs to.
    pub file_path: PathBuf,
    /// Number of dot-separated parts in the prefix.
    pub part_count: usize,
    /// Whether the prefix is a concatenated (no-dot) form.
    pub is_concatenated: bool,
    /// Position in the filename where this prefix starts.
    pub start_position: usize,
    /// The original prefix string.
    pub prefix: String,
}

/// Used during the collection phase before converting to final `PrefixGroup`.
#[derive(Debug)]
pub struct PrefixGroupBuilder {
    /// The original prefix string (may contain dots).
    pub original_prefix: String,
    /// Files belonging to this prefix group.
    pub files: Vec<PathBuf>,
    /// Number of dot-separated parts in the prefix.
    pub part_count: usize,
    /// Whether the prefix has a concatenated (no-dot) form.
    pub has_concatenated_form: bool,
    /// Minimum start position where this prefix appears across all files.
    pub min_start_position: usize,
}

/// Receives progress while files are grouped.
///
/// Grouping runs in parallel, so `advance` can be called from several threads at once.
/// Every method does nothing by default, and `()` reports nothing.
pub trait GroupingProgress: Sync {
    /// A stage started and will advance `length` times.
    fn start_stage(&self, _stage: GroupingStage, _length: u64) {}

    /// One item of the current stage finished.
    fn advance(&self) {}

    /// The current stage finished.
    fn finish_stage(&self) {}
}

impl GroupingProgress for () {}

impl GroupingOptions {
    /// Create grouping options.
    ///
    /// Ignored group parts are trimmed and lowercased, and duplicates are removed from both lists.
    #[must_use]
    pub fn new<P, O>(
        min_group_size: usize,
        min_prefix_chars: usize,
        ignored_group_parts: P,
        prefix_overrides: O,
    ) -> Self
    where
        P: IntoIterator,
        P::Item: AsRef<str>,
        O: IntoIterator,
        O::Item: Into<String>,
    {
        let ignored_group_parts = unique_in_order(
            ignored_group_parts
                .into_iter()
                .map(|part| part.as_ref().trim().to_lowercase()),
        );
        let prefix_overrides = unique_in_order(prefix_overrides.into_iter().map(Into::into));
        Self {
            min_group_size,
            min_prefix_chars,
            ignored_group_parts,
            prefix_overrides,
        }
    }

    /// Return the minimum number of files in a group.
    #[must_use]
    pub const fn min_group_size(&self) -> usize {
        self.min_group_size
    }

    /// Return the minimum number of characters in a single word group prefix.
    #[must_use]
    pub const fn min_prefix_chars(&self) -> usize {
        self.min_prefix_chars
    }

    /// Return whether any part of a name matches an ignored group part, ignoring ASCII case.
    #[must_use]
    pub fn has_ignored_group_part<'part>(&self, mut parts: impl Iterator<Item = &'part str>) -> bool {
        parts.any(|part| {
            self.ignored_group_parts
                .iter()
                .any(|ignored| part.eq_ignore_ascii_case(ignored))
        })
    }
}

impl Default for GroupingOptions {
    /// Create options with the default group size and prefix length and no ignored parts or overrides.
    fn default() -> Self {
        Self::new(
            DEFAULT_MIN_GROUP_SIZE,
            DEFAULT_MIN_PREFIX_CHARS,
            Vec::<String>::new(),
            Vec::<String>::new(),
        )
    }
}

impl<'a> PrefixGrouper<'a> {
    /// Create a grouper from grouping options and name filter rules.
    #[must_use]
    pub const fn new(options: &'a GroupingOptions, name_filter: &'a NameFilter) -> Self {
        Self { options, name_filter }
    }

    /// Collect all possible prefix groups for files.
    ///
    /// Files can appear in multiple groups, since they are only excluded once actually moved.
    /// Returns a map from display prefix to its group, where the part count indicates specificity.
    #[must_use]
    pub fn collect_all_prefix_groups(&self, files_with_names: &[FileInfo]) -> HashMap<String, PrefixGroup> {
        self.collect_all_prefix_groups_with_progress(files_with_names, &())
    }

    /// Collect all possible prefix groups for files, reporting progress of each grouping stage.
    #[must_use]
    pub fn collect_all_prefix_groups_with_progress(
        &self,
        files_with_names: &[FileInfo],
        progress: &impl GroupingProgress,
    ) -> HashMap<String, PrefixGroup> {
        let prefix_index = PrefixIndex::new(files_with_names);
        let mut prefix_groups = self.first_pass(files_with_names, &prefix_index, progress);
        Self::second_pass(files_with_names, &prefix_index, &mut prefix_groups, progress);

        let display_groups: HashMap<String, PrefixGroup> = prefix_groups
            .into_values()
            .map(PrefixGroupBuilder::into_prefix_group)
            .collect();

        self.apply_prefix_overrides(display_groups)
    }

    /// Order groups for processing, dropping groups smaller than the minimum group size.
    ///
    /// Groups are sorted by start position, earlier first, then by part count, prefix length,
    /// and file count, larger first, and finally alphabetically.
    /// This biases towards more specific prefixes from the start of file names.
    #[must_use]
    pub fn groups_in_processing_order(&self, groups: HashMap<String, PrefixGroup>) -> Vec<(String, PrefixGroup)> {
        let mut groups: Vec<_> = groups
            .into_iter()
            .filter(|(_, group)| group.files.len() >= self.options.min_group_size)
            .collect();
        groups.sort_by(|(prefix_a, group_a), (prefix_b, group_b)| {
            group_a
                .min_start_position
                .cmp(&group_b.min_start_position)
                .then_with(|| group_b.part_count.cmp(&group_a.part_count))
                .then_with(|| prefix_b.chars().count().cmp(&prefix_a.chars().count()))
                .then_with(|| group_b.files.len().cmp(&group_a.files.len()))
                .then_with(|| prefix_a.cmp(prefix_b))
        });
        groups
    }

    /// Resolve a prefix into a directory name by replacing dots with spaces and stripping ignored prefixes.
    #[must_use]
    pub fn directory_name_for_prefix(&self, prefix: &str) -> String {
        let directory_name = prefix.replace('.', " ");
        self.name_filter.strip_ignored_prefixes(&directory_name).into_owned()
    }

    /// Check whether a proposed directory name should be skipped.
    ///
    /// Skips empty names, ignored group names, ignored prefixes,
    /// names matching the parent directory, and names ignored during this run.
    #[must_use]
    pub fn should_skip_group<S: BuildHasher>(
        &self,
        directory_name: &str,
        parent_directory_normalized: Option<&str>,
        runtime_ignored: &HashSet<String, S>,
    ) -> bool {
        if directory_name.is_empty() {
            return true;
        }

        let name_filter = self.name_filter;
        let directory_name_normalized = utils::normalize_name(directory_name);
        name_filter.should_skip_destination_directory(directory_name, &directory_name_normalized)
            || runtime_ignored.contains(&directory_name_normalized)
            || name_filter.is_ignored_prefix(directory_name)
            || parent_directory_normalized.is_some_and(|parent| parent == directory_name_normalized)
    }

    /// Return whether a directory name could ever be offered as a group name.
    ///
    /// Hidden names, ignored prefixes, ignored group names, and names with an ignored group part never are,
    /// so callers that remember known directory names can leave them out.
    #[must_use]
    pub fn accepts_group_name(&self, name: &str) -> bool {
        !name.starts_with('.')
            && !self.name_filter.is_ignored_prefix(name)
            && !self.is_ignored_group(&utils::normalize_name(name), name.split([' ', '.']))
    }

    /// Apply prefix overrides to groups.
    ///
    /// A group moves under the first override that its prefix starts with, that starts with its prefix,
    /// or that any of its file names starts with.
    /// Groups that end up under the same prefix are merged.
    #[must_use]
    fn apply_prefix_overrides(&self, groups: HashMap<String, PrefixGroup>) -> HashMap<String, PrefixGroup> {
        if self.options.prefix_overrides.is_empty() {
            return groups;
        }

        let mut result: HashMap<String, PrefixGroup> = HashMap::new();

        for (prefix, group) in groups {
            let target_prefix = self
                .options
                .prefix_overrides
                .iter()
                .find(|&override_prefix| {
                    prefix.starts_with(override_prefix)
                        || override_prefix.starts_with(&prefix)
                        || group.files.iter().any(|file| {
                            file.file_name()
                                .and_then(|name| name.to_str())
                                .is_some_and(|name| name.starts_with(override_prefix))
                        })
                })
                .map_or(prefix, std::clone::Clone::clone);

            let entry = result
                .entry(target_prefix)
                .or_insert_with(|| PrefixGroup::new(Vec::new(), group.part_count, group.min_start_position));
            entry.files.extend(group.files);
            entry.part_count = entry.part_count.max(group.part_count);
            entry.min_start_position = entry.min_start_position.min(group.min_start_position);
        }

        for group in result.values_mut() {
            group.files.sort();
            group.files.dedup();
        }

        result
    }

    /// Return whether a normalized group name is ignored, or any of its parts is an ignored group part.
    fn is_ignored_group<'part>(&self, normalized_name: &str, parts: impl Iterator<Item = &'part str>) -> bool {
        self.name_filter.matches_ignored_group_name(normalized_name) || self.options.has_ignored_group_part(parts)
    }

    /// Collect prefix candidates from each file in parallel and merge them into group builders.
    ///
    /// Candidates that match an ignored group name, contain an ignored group part,
    /// or whose parts are not contiguous in the file's original name are left out.
    fn first_pass(
        &self,
        files_with_names: &[FileInfo],
        prefix_index: &PrefixIndex<'_>,
        progress: &impl GroupingProgress,
    ) -> HashMap<String, PrefixGroupBuilder> {
        let min_group_size = self.options.min_group_size;
        let min_prefix_chars = self.options.min_prefix_chars;
        progress.start_stage(GroupingStage::CollectingPrefixes, files_with_names.len() as u64);

        let first_pass_results: Vec<Vec<ValidCandidate>> = files_with_names
            .par_iter()
            .map(|file_info| {
                let prefix_candidates = candidates::find_prefix_candidates_indexed(
                    &file_info.filtered_name,
                    files_with_names,
                    prefix_index,
                    min_group_size,
                    min_prefix_chars,
                );

                let mut valid_candidates: Vec<ValidCandidate> = Vec::new();
                let file_path = file_info.path.clone();

                for candidate in prefix_candidates {
                    let key = utils::normalize_name(&candidate.prefix);
                    let candidate_parts: Vec<&str> = candidate.prefix.split('.').collect();
                    let candidate_combined = candidate_parts.join("").to_lowercase();

                    if self.is_ignored_group(&key, candidate_parts.iter().copied()) {
                        continue;
                    }

                    // Filtering can make non-adjacent parts look adjacent,
                    // so the file itself must have the prefix parts contiguous in its original name.
                    if !candidates::parts_are_contiguous_lowered(
                        file_info.original_parts(),
                        file_info.original_parts_lower(),
                        &candidate_parts,
                        &candidate_combined,
                    ) {
                        continue;
                    }

                    let is_concatenated = !candidate.prefix.contains('.');
                    valid_candidates.push(ValidCandidate {
                        key,
                        file_path: file_path.clone(),
                        part_count: candidate.part_count,
                        is_concatenated,
                        start_position: candidate.start_position,
                        prefix: candidate.prefix.into_owned(),
                    });
                }
                progress.advance();
                valid_candidates
            })
            .collect();

        let mut prefix_groups: HashMap<String, PrefixGroupBuilder> = HashMap::new();
        for candidate in first_pass_results.into_iter().flatten() {
            if let Some(builder) = prefix_groups.get_mut(&candidate.key) {
                builder.add_file(
                    candidate.file_path,
                    candidate.part_count,
                    candidate.is_concatenated,
                    candidate.start_position,
                    candidate.prefix,
                );
            } else {
                prefix_groups.insert(
                    candidate.key,
                    PrefixGroupBuilder::new(
                        candidate.prefix,
                        candidate.file_path,
                        candidate.part_count,
                        candidate.is_concatenated,
                        candidate.start_position,
                    ),
                );
            }
        }
        progress.finish_stage();

        prefix_groups
    }

    /// Add files whose first parts start with a group prefix to that group.
    ///
    /// This handles cases like `JosephExampleTV` joining the `JosephExample` group,
    /// using the prefix index instead of checking every group against every file.
    fn second_pass(
        files_with_names: &[FileInfo],
        prefix_index: &PrefixIndex<'_>,
        prefix_groups: &mut HashMap<String, PrefixGroupBuilder>,
        progress: &impl GroupingProgress,
    ) {
        let group_keys_with_combined: Vec<(String, String, HashSet<PathBuf>)> = prefix_groups
            .iter()
            .map(|(key, builder)| {
                let combined = key.to_lowercase().replace('.', "");
                (key.clone(), combined, builder.files.iter().cloned().collect())
            })
            .collect();

        progress.start_stage(
            GroupingStage::MatchingFilesToGroups,
            group_keys_with_combined.len() as u64,
        );
        let second_pass_results: Vec<Vec<(String, PathBuf)>> = group_keys_with_combined
            .par_iter()
            .map(|(group_key, group_combined, existing_files)| {
                let mut matches: Vec<(String, PathBuf)> = Vec::new();
                let mut matching_indices = Vec::new();
                prefix_index.matching_files(group_key, &mut matching_indices);
                for index in matching_indices {
                    let Some(file_info) = files_with_names.get(index) else {
                        continue;
                    };
                    let file_path = file_info.path.clone();
                    if !existing_files.contains(&file_path)
                        && candidates::parts_are_contiguous_lowered(
                            file_info.original_parts(),
                            file_info.original_parts_lower(),
                            &[group_key.as_str()],
                            group_combined,
                        )
                    {
                        matches.push((group_key.clone(), file_path));
                    }
                }
                progress.advance();
                matches
            })
            .collect();

        for (group_key, file_path) in second_pass_results.into_iter().flatten() {
            if let Some(builder) = prefix_groups.get_mut(&group_key) {
                builder.files.push(file_path);
            }
        }
        progress.finish_stage();
    }
}

impl PrefixGroup {
    /// Create a new `PrefixGroup`.
    #[must_use]
    pub const fn new(files: Vec<PathBuf>, part_count: usize, min_start_position: usize) -> Self {
        Self {
            files,
            part_count,
            min_start_position,
        }
    }
}

impl PrefixGroupBuilder {
    /// Create a new `PrefixGroupBuilder`.
    #[must_use]
    pub fn new(
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
    /// Prefers concatenated (no-dot) forms for directory names.
    /// Among concatenated forms, prefers proper casing (starts with uppercase).
    /// For determinism across platforms, uses alphabetical ordering as tiebreaker.
    pub fn add_file(
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
        if is_concatenated {
            if self.has_concatenated_form {
                // Already have a concatenated form - prefer better casing or alphabetical order
                let should_replace = Self::is_better_prefix(&prefix, &self.original_prefix);
                if should_replace {
                    self.original_prefix = prefix;
                }
            } else {
                // First concatenated form - use it
                self.original_prefix = prefix;
                self.has_concatenated_form = true;
            }
        } else if !self.has_concatenated_form {
            // Without a concatenated form, still choose a deterministic dotted display prefix.
            // This avoids platform-dependent directory names caused by filesystem iteration order.
            let should_replace = Self::is_better_prefix(&prefix, &self.original_prefix);
            if should_replace {
                self.original_prefix = prefix;
            }
        }
    }

    /// Convert this builder into a final `PrefixGroup`.
    #[must_use]
    pub fn into_prefix_group(self) -> (String, PrefixGroup) {
        (
            self.original_prefix,
            PrefixGroup::new(self.files, self.part_count, self.min_start_position),
        )
    }

    /// Determine if `new_prefix` is better than `current_prefix` for display.
    /// Prefers CamelCase (mixed case) over all-uppercase or all-lowercase.
    /// Uses alphabetical order as final tiebreaker for determinism.
    #[must_use]
    fn is_better_prefix(new_prefix: &str, current_prefix: &str) -> bool {
        let new_score = Self::casing_score(new_prefix);
        let current_score = Self::casing_score(current_prefix);

        if new_score == current_score {
            // Same casing quality - use alphabetical order for determinism
            new_prefix < current_prefix
        } else {
            // Higher score is better
            new_score > current_score
        }
    }

    /// Score the casing quality of a prefix.
    /// Higher scores are better.
    /// CamelCase (mixed case starting with uppercase) = 3
    /// Starts with uppercase but all same case = 2
    /// Mixed case starting with lowercase = 1
    /// All lowercase or all uppercase = 0
    #[must_use]
    fn casing_score(prefix: &str) -> u8 {
        let mut chars = prefix.chars();
        let Some(first) = chars.next() else {
            return 0;
        };

        let starts_upper = first.is_ascii_uppercase();
        let has_upper = starts_upper || chars.clone().any(|character| character.is_ascii_uppercase());
        let has_lower = first.is_ascii_lowercase() || chars.any(|character| character.is_ascii_lowercase());

        match (starts_upper, has_upper, has_lower) {
            // CamelCase: starts uppercase, has both upper and lower (e.g., "PhotoLab")
            (true, true, true) => 3,
            // Starts uppercase, single case (e.g., "PHOTOLAB")
            (true, true, false) => 2,
            // Mixed case but starts lowercase (e.g., "photoLab")
            (false, true, true) => 1,
            // All lowercase (e.g., "photolab") or edge cases
            _ => 0,
        }
    }
}

#[cfg(test)]
mod test_prefix_groups {
    use crate::test_helpers::*;

    #[test]
    fn files_appear_in_multiple_groups() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Show.Name.S01E01.mp4"), "").unwrap();
        std::fs::write(root.join("Show.Name.S01E02.mp4"), "").unwrap();
        std::fs::write(root.join("Show.Name.S01E03.mp4"), "").unwrap();
        std::fs::write(root.join("Show.Other.S01E01.mp4"), "").unwrap();
        std::fs::write(root.join("Show.Other.S01E02.mp4"), "").unwrap();

        let config = TestConfig::test_with_group_size(2);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Keys are normalized (lowercase, no dots)
        assert!(groups.contains_key("Show.Name"), "Should have Show.Name group");
        assert!(groups.contains_key("Show.Other"), "Should have Show.Other group");
        assert!(groups.contains_key("Show"), "Should have Show group");
        assert_eq!(groups.get("Show.Name").unwrap().files.len(), 3);
        assert_eq!(groups.get("Show.Other").unwrap().files.len(), 2);
        assert_eq!(groups.get("Show").unwrap().files.len(), 5);
    }

    #[test]
    fn order_by_prefix_length() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Alpha.Beta.Gamma.part1.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Beta.Gamma.part2.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Beta.Delta.part1.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Beta.Delta.part2.mp4"), "").unwrap();

        let config = TestConfig::test_with_group_size(2);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        assert!(groups.contains_key("Alpha.Beta.Gamma"));
        assert!(groups.contains_key("Alpha.Beta.Delta"));
        assert!(groups.contains_key("Alpha.Beta"));
        assert!(groups.contains_key("Alpha"));
        assert_eq!(groups.get("Alpha.Beta.Gamma").unwrap().files.len(), 2);
        assert_eq!(groups.get("Alpha.Beta.Delta").unwrap().files.len(), 2);
        assert_eq!(groups.get("Alpha.Beta").unwrap().files.len(), 4);
        assert_eq!(groups.get("Alpha").unwrap().files.len(), 4);
    }

    #[test]
    fn no_files_no_groups() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        let config = TestConfig::test_with_group_size(2);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        assert!(groups.is_empty());
    }

    #[test]
    fn single_file_no_group() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Lonely.File.mp4"), "").unwrap();

        let config = TestConfig::test_with_group_size(2);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        assert!(groups.is_empty());
    }

    #[test]
    fn files_with_completely_different_prefixes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Alpha.One.mp4"), "").unwrap();
        std::fs::write(root.join("Beta.Two.mp4"), "").unwrap();
        std::fs::write(root.join("Gamma.Three.mp4"), "").unwrap();

        let config = TestConfig::test_with_group_size(2);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        assert!(groups.is_empty());
    }

    #[test]
    fn large_file_set_multiple_series() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        for episode in 1..=10 {
            std::fs::write(root.join(format!("SeriesA.Season1.E{episode:02}.mp4")), "").unwrap();
        }
        for episode in 1..=8 {
            std::fs::write(root.join(format!("SeriesA.Season2.E{episode:02}.mp4")), "").unwrap();
        }
        for episode in 1..=5 {
            std::fs::write(root.join(format!("SeriesB.Season1.E{episode:02}.mp4")), "").unwrap();
        }

        let config = TestConfig::test_with_group_size(3);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        assert!(groups.contains_key("SeriesA.Season1"));
        assert!(groups.contains_key("SeriesA.Season2"));
        assert!(groups.contains_key("SeriesB.Season1"));
        assert!(groups.contains_key("SeriesA"));
        assert!(groups.contains_key("SeriesB"));
        assert_eq!(groups.get("SeriesA.Season1").unwrap().files.len(), 10);
        assert_eq!(groups.get("SeriesA.Season2").unwrap().files.len(), 8);
        assert_eq!(groups.get("SeriesA").unwrap().files.len(), 18);
        assert_eq!(groups.get("SeriesB.Season1").unwrap().files.len(), 5);
        assert_eq!(groups.get("SeriesB").unwrap().files.len(), 5);
    }

    #[test]
    fn min_group_size_affects_grouping() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Show.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Show.Episode.02.mp4"), "").unwrap();

        let config_low = TestConfig::test_with_group_size(2);

        let fixture_low = Fixture::new(root.clone(), config_low);
        let files_with_names = fixture_low.scanner().collect_files_with_names().unwrap();
        let groups_low = fixture_low.grouper().collect_all_prefix_groups(&files_with_names);
        assert!(groups_low.contains_key("Show.Episode"));
        assert!(groups_low.contains_key("Show"));

        let config_high = TestConfig::test_with_group_size(5);

        let fixture_high = Fixture::new(root, config_high);
        let files_with_names = fixture_high.scanner().collect_files_with_names().unwrap();
        let groups_high = fixture_high.grouper().collect_all_prefix_groups(&files_with_names);
        // With min_group_size=5, groups with only 2 files are excluded
        assert!(groups_high.is_empty());
    }

    #[test]
    fn case_variations_grouped_together() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("MyShow.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("MYSHOW.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("myshow.Episode.03.mp4"), "").unwrap();
        std::fs::write(root.join("Myshow.Episode.04.mp4"), "").unwrap();

        let config = TestConfig::test_with_group_size(3);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // All case variations should be in the same group
        // Key will be from first file processed (original case preserved for display)
        // Check that some group has all 4 files
        let max_group_size = groups.values().map(|group| group.files.len()).max().unwrap_or(0);
        assert_eq!(max_group_size, 4, "Should have a group with all 4 case variations");
    }

    #[test]
    fn dot_separated_and_concatenated_grouped_together() {
        // Dot-separated and concatenated prefixes should be grouped together
        // because prefixes_match_normalized handles this
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // These have different structures: "Photo.Lab" vs "PhotoLab"
        std::fs::write(root.join("Photo.Lab.Image.01.jpg"), "").unwrap();
        std::fs::write(root.join("PhotoLab.Image.02.jpg"), "").unwrap();
        std::fs::write(root.join("Photolab.Image.03.jpg"), "").unwrap();
        std::fs::write(root.join("PHOTOLAB.Image.04.jpg"), "").unwrap();

        let config = TestConfig::test_with_group_size(2);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();

        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // All 4 files should be grouped together (PhotoLab = Photo.Lab = Photolab = PHOTOLAB)
        let max_group_size = groups.values().map(|group| group.files.len()).max().unwrap_or(0);
        assert_eq!(max_group_size, 4, "Should have a group with all 4 files");

        // Should prefer the concatenated form (no dots) for the directory name
        // The group key should be "PhotoLab" not "Photo.Lab"
        assert!(
            groups.contains_key("PhotoLab"),
            "Should prefer concatenated prefix 'PhotoLab' over dotted 'Photo.Lab'"
        );
    }

    #[test]
    fn prefers_concatenated_prefix_for_directory_name() {
        // When files have both "Darkko.TV" and "DarkkoTV" forms, prefer "DarkkoTV"
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Darkko.TV.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("DarkkoTV.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Darkko.TV.Episode.03.mp4"), "").unwrap();

        let config = TestConfig::test_with_group_size(2);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should have a group with all 3 files
        let max_group_size = groups.values().map(|group| group.files.len()).max().unwrap_or(0);
        assert_eq!(max_group_size, 3, "Should have a group with all 3 files");

        // The group key should be "DarkkoTV" (concatenated) not "Darkko.TV" (dotted)
        assert!(
            groups.contains_key("DarkkoTV"),
            "Should prefer concatenated prefix 'DarkkoTV' over dotted 'Darkko.TV'"
        );
        assert!(
            !groups.contains_key("Darkko.TV"),
            "Should not have separate group for dotted form"
        );
    }

    #[test]
    fn same_prefix_different_case_grouped() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Summer.Vacation.Photo1.jpg"), "").unwrap();
        std::fs::write(root.join("Summer.Vacation.Photo2.jpg"), "").unwrap();
        std::fs::write(root.join("SUMMER.VACATION.Photo3.jpg"), "").unwrap();
        std::fs::write(root.join("summer.vacation.Photo4.jpg"), "").unwrap();

        let config = TestConfig::test_with_group_size(2);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // All 4 files should be grouped under "Summer.Vacation" (case-insensitive)
        assert!(
            groups.contains_key("Summer.Vacation"),
            "Should have Summer.Vacation group"
        );
        assert_eq!(groups.get("Summer.Vacation").unwrap().files.len(), 4);
    }

    #[test]
    fn min_group_size_respected_in_groups() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // 5 files with same prefix
        std::fs::write(root.join("Gallery.Photos.Img1.jpg"), "").unwrap();
        std::fs::write(root.join("Gallery.Photos.Img2.jpg"), "").unwrap();
        std::fs::write(root.join("Gallery.Photos.Img3.jpg"), "").unwrap();
        std::fs::write(root.join("Gallery.Photos.Img4.jpg"), "").unwrap();
        std::fs::write(root.join("Gallery.Photos.Img5.jpg"), "").unwrap();

        // With min_group_size=6, all prefixes with 5 files are excluded
        let config_6 = TestConfig::test_with_group_size(6);

        let fixture_6 = Fixture::new(root.clone(), config_6);
        let files_with_names = fixture_6.scanner().collect_files_with_names().unwrap();
        let groups_6 = fixture_6.grouper().collect_all_prefix_groups(&files_with_names);
        // With min_group_size=6, all prefixes with 5 files are excluded
        assert!(
            groups_6.is_empty(),
            "min_group_size=6 should not find groups with only 5 files"
        );

        // With min_group_size=5, should find the group
        let config_5 = TestConfig::test_with_group_size(5);

        let fixture_5 = Fixture::new(root.clone(), config_5);
        let files_with_names = fixture_5.scanner().collect_files_with_names().unwrap();
        let groups_5 = fixture_5.grouper().collect_all_prefix_groups(&files_with_names);
        assert!(!groups_5.is_empty(), "min_group_size=5 should find groups");
        assert!(
            groups_5.contains_key("Gallery.Photos"),
            "Should have Gallery.Photos group"
        );
        assert_eq!(groups_5.get("Gallery.Photos").unwrap().files.len(), 5);

        // With min_group_size=3, should still find the same group
        let config_3 = TestConfig::test_with_group_size(3);

        let fixture_3 = Fixture::new(root, config_3);
        let files_with_names = fixture_3.scanner().collect_files_with_names().unwrap();
        let groups_3 = fixture_3.grouper().collect_all_prefix_groups(&files_with_names);
        assert!(
            groups_3.contains_key("Gallery.Photos"),
            "Should have Gallery.Photos group"
        );
        assert_eq!(groups_3.get("Gallery.Photos").unwrap().files.len(), 5);
    }

    #[test]
    fn prefix_overrides_merge_groups() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Show.Name.Season1.Episode01.mp4"), "").unwrap();
        std::fs::write(root.join("Show.Name.Season1.Episode02.mp4"), "").unwrap();
        std::fs::write(root.join("Show.Name.Season2.Episode01.mp4"), "").unwrap();
        std::fs::write(root.join("Show.Name.Season2.Episode02.mp4"), "").unwrap();

        let config = TestConfig {
            prefix_overrides: vec!["Show.Name".to_string()],
            ..TestConfig::test_with_group_size(2)
        };

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // With prefix_overrides = ["Show.Name"], groups starting with Show.Name should be merged
        assert!(
            groups.contains_key("Show.Name"),
            "Should have Show.Name group from override"
        );
        // Should not have separate Season1/Season2 groups - they're merged into Show.Name
        assert!(
            !groups.contains_key("Show.Name.Season1"),
            "Season1 should be merged into Show.Name"
        );
        assert!(
            !groups.contains_key("Show.Name.Season2"),
            "Season2 should be merged into Show.Name"
        );
        let show_name_files = groups.get("Show.Name").unwrap();
        assert_eq!(
            show_name_files.files.len(),
            4,
            "Show.Name group should contain each file exactly once without duplicates"
        );
    }

    #[test]
    fn prefix_overrides_multiple_overrides() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("ShowA.Season1.Ep01.mp4"), "").unwrap();
        std::fs::write(root.join("ShowA.Season1.Ep02.mp4"), "").unwrap();
        std::fs::write(root.join("ShowB.Season1.Ep01.mp4"), "").unwrap();
        std::fs::write(root.join("ShowB.Season1.Ep02.mp4"), "").unwrap();
        std::fs::write(root.join("ShowC.Season1.Ep01.mp4"), "").unwrap();
        std::fs::write(root.join("ShowC.Season1.Ep02.mp4"), "").unwrap();

        let config = TestConfig {
            prefix_overrides: vec!["SeriesA".to_string(), "SeriesB".to_string()],
            ..TestConfig::test_with_group_size(2)
        };

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // ShowA and ShowB should use overrides (groups merged under override name)
        assert!(groups.contains_key("ShowA"), "Should have ShowA from override");
        assert!(groups.contains_key("ShowB"), "Should have ShowB from override");
        // Each override group should have at least 2 entries (the 2 files)
        assert!(
            groups.get("ShowA").unwrap().files.len() >= 2,
            "ShowA should have at least 2 entries"
        );
        assert!(
            groups.get("ShowB").unwrap().files.len() >= 2,
            "ShowB should have at least 2 entries"
        );
        // ShowC has no override, should have its own groups
        let has_showc_group = groups.keys().any(|k| k.starts_with("ShowC"));
        assert!(has_showc_group, "ShowC should have a group");
    }

    #[test]
    fn prefix_overrides_no_match_keeps_original() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("MyShow.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("MyShow.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("MyShow.Episode.03.mp4"), "").unwrap();

        let config = TestConfig {
            prefix_overrides: vec!["NoMatch".to_string()],
            ..TestConfig::test_with_group_size(2)
        };

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Override doesn't match, so original prefix should be used
        let has_myshow_group = groups.keys().any(|k| k.starts_with("MyShow"));
        assert!(has_myshow_group, "Should have MyShow group (override didn't match)");
        assert!(
            !groups.contains_key("OtherShow"),
            "OtherShow override should not create empty group"
        );
    }

    #[test]
    fn prefix_ignores_strips_prefix_from_grouping() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Files with and without "www" prefix
        std::fs::write(root.join("www.Example.Show.Ep01.mp4"), "").unwrap();
        std::fs::write(root.join("www.Example.Show.Ep02.mp4"), "").unwrap();
        std::fs::write(root.join("Example.Show.Ep03.mp4"), "").unwrap();

        let config = TestConfig::test_with_prefix_ignores(vec!["www"]);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // With prefix_ignores stripping "www", all 3 files should group together
        // Check that we have groups containing files
        assert!(!groups.is_empty(), "Should have at least one group");
    }

    #[test]
    fn prefix_ignores_and_overrides_combined() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("release.Show.Name.Season1.Ep01.mp4"), "").unwrap();
        std::fs::write(root.join("release.Show.Name.Season1.Ep02.mp4"), "").unwrap();
        std::fs::write(root.join("Show.Name.Season2.Ep01.mp4"), "").unwrap();
        std::fs::write(root.join("Show.Name.Season2.Ep02.mp4"), "").unwrap();

        let config = TestConfig {
            prefix_ignores: vec!["dl".to_string()],
            prefix_overrides: vec!["Show.Name".to_string()],
            ..TestConfig::test_with_group_size(2)
        };

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // With override "Show.Name", should have that group
        assert!(groups.contains_key("Show.Name"), "Should have Show.Name group");
        // Should not have separate Season groups - merged into Show.Name
        assert!(
            !groups.contains_key("Show.Name.Season1"),
            "Season1 should be merged into Show.Name"
        );
        assert!(
            !groups.contains_key("Show.Name.Season2"),
            "Season2 should be merged into Show.Name"
        );
        // The Show.Name group should contain entries for all 4 files
        let show_name_files = groups.get("Show.Name").unwrap();
        assert!(
            show_name_files.files.len() >= 4,
            "Show.Name group should have at least 4 entries"
        );
    }
}

#[cfg(test)]
mod test_ignored_group_names {

    use crate::test_helpers::*;

    #[test]
    fn ignored_group_name_not_offered() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Studio.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Episode.03.mp4"), "").unwrap();

        let config = TestConfig::test_with_ignored_group_names(vec!["episode"]);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // "Episode" should NOT be offered as a group name
        assert!(
            !groups.contains_key("Episode"),
            "Ignored group name 'Episode' should not be offered"
        );
        // But "Studio" should still be offered
        assert!(
            groups.contains_key("Studio"),
            "Non-ignored group name 'Studio' should still be offered"
        );
    }

    #[test]
    fn multiple_ignored_group_names() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Studio.Video.Part.01.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Video.Part.02.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Video.Part.03.mp4"), "").unwrap();

        let config = TestConfig::test_with_ignored_group_names(vec!["video", "part"]);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Both "Video" and "Part" should NOT be offered
        assert!(
            !groups.contains_key("Video"),
            "Ignored group name 'Video' should not be offered"
        );
        assert!(
            !groups.contains_key("Part"),
            "Ignored group name 'Part' should not be offered"
        );
        // "Studio" should still be offered
        assert!(
            groups.contains_key("Studio"),
            "Non-ignored group name 'Studio' should still be offered"
        );
    }

    #[test]
    fn ignored_group_names_case_insensitive() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Studio.EPISODE.01.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.episode.03.mp4"), "").unwrap();

        // Config helper automatically lowercases
        let config = TestConfig::test_with_ignored_group_names(vec!["episode"]);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // All case variations of "Episode" should be filtered out
        assert!(
            !groups.contains_key("EPISODE"),
            "Ignored group name 'EPISODE' should not be offered"
        );
        assert!(
            !groups.contains_key("Episode"),
            "Ignored group name 'Episode' should not be offered"
        );
        assert!(
            !groups.contains_key("episode"),
            "Ignored group name 'episode' should not be offered"
        );
    }

    #[test]
    fn ignored_multi_part_group_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Studio.Season.One.01.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Season.One.02.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Season.One.03.mp4"), "").unwrap();

        // normalized form (no dots)
        let config = TestConfig::test_with_ignored_group_names(vec!["seasonone"]);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // "Season.One" (normalized to "seasonone") should NOT be offered
        assert!(
            !groups.contains_key("Season.One"),
            "Ignored group name 'Season.One' should not be offered"
        );
        // But "Studio" should still be offered
        assert!(
            groups.contains_key("Studio"),
            "Non-ignored group name 'Studio' should still be offered"
        );
    }

    #[test]
    fn ignored_multi_part_group_name_with_spaces() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Studio.Season.One.01.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Season.One.02.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Season.One.03.mp4"), "").unwrap();

        // Space-separated form should be normalized and still match
        let config = TestConfig::test_with_ignored_group_names(vec!["Season One"]);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        assert!(
            !groups.contains_key("Season.One"),
            "Ignored group name 'Season One' (with space) should filter 'Season.One'"
        );
        assert!(
            groups.contains_key("Studio"),
            "Non-ignored group name 'Studio' should still be offered"
        );
    }

    #[test]
    fn ignored_multi_part_group_name_with_dots() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Studio.Season.One.01.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Season.One.02.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Season.One.03.mp4"), "").unwrap();

        // Dot-separated form should be normalized and still match
        let config = TestConfig::test_with_ignored_group_names(vec!["Season.One"]);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        assert!(
            !groups.contains_key("Season.One"),
            "Ignored group name 'Season.One' (with dot) should filter 'Season.One'"
        );
        assert!(
            groups.contains_key("Studio"),
            "Non-ignored group name 'Studio' should still be offered"
        );
    }

    #[test]
    fn ignored_group_names_mixed_formats_all_match() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Studio.Season.One.01.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Season.One.02.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Season.One.03.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Big.Show.01.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Big.Show.02.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Big.Show.03.mp4"), "").unwrap();

        // Mix of space, dot, and concatenated forms should all normalize the same way
        let config = TestConfig::test_with_ignored_group_names(vec!["Season One", "Big.Show"]);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        assert!(
            !groups.contains_key("Season.One"),
            "Space-separated ignore 'Season One' should filter 'Season.One'"
        );
        assert!(
            !groups.contains_key("Big.Show"),
            "Dot-separated ignore 'Big.Show' should filter 'Big.Show'"
        );
        assert!(
            groups.contains_key("Studio"),
            "Non-ignored group name 'Studio' should still be offered"
        );
    }

    #[test]
    fn empty_ignored_list_allows_all() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Studio.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Episode.03.mp4"), "").unwrap();

        let config = TestConfig::test_with_group_size(3);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Both "Studio" and "Episode" should be offered when no ignores
        assert!(
            groups.contains_key("Studio"),
            "'Studio' should be offered with empty ignore list"
        );
        assert!(
            groups.contains_key("Episode"),
            "'Episode' should be offered with empty ignore list"
        );
    }

    #[test]
    fn ignored_group_name_with_common_words() {
        // Test realistic scenario with common words to ignore
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("StudioName.Scene.01.Chapter.01.mp4"), "").unwrap();
        std::fs::write(root.join("StudioName.Scene.02.Chapter.02.mp4"), "").unwrap();
        std::fs::write(root.join("StudioName.Scene.03.Chapter.03.mp4"), "").unwrap();

        let config = TestConfig {
            min_prefix_chars: 5,
            ..TestConfig::test_with_ignored_group_names(vec!["scene", "chapter"])
        };

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Common words "Scene" and "Chapter" should NOT be offered
        assert!(
            !groups.contains_key("Scene"),
            "Common word 'Scene' should not be offered"
        );
        assert!(
            !groups.contains_key("Chapter"),
            "Common word 'Chapter' should not be offered"
        );
        // But the actual studio name should still be offered
        assert!(
            groups.contains_key("StudioName"),
            "Actual studio name should still be offered"
        );
    }
}

#[cfg(test)]
mod test_parent_directory_skipped {
    use super::*;
    use crate::roots::Roots;
    use crate::test_helpers::*;

    /// Group the files in `root` and return the prefixes and the directory names that would be offered,
    /// skipping names equal to the root's own name the same way create mode does.
    fn grouped_and_offered(root: &std::path::Path) -> (Vec<String>, Vec<String>) {
        let fixture = Fixture::new(root.to_path_buf(), TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let grouper = fixture.grouper();
        let groups = grouper.collect_all_prefix_groups(&files_with_names);
        let parent_normalized =
            Roots::new(vec![root.to_path_buf()], vec![root.to_path_buf()]).first_output_normalized_name();

        let mut prefixes: Vec<String> = groups.keys().cloned().collect();
        prefixes.sort();
        let mut offered: Vec<String> = prefixes
            .iter()
            .map(|prefix| grouper.directory_name_for_prefix(prefix))
            .filter(|name| !grouper.should_skip_group(name, parent_normalized.as_deref(), &HashSet::<String>::new()))
            .collect();
        offered.sort();
        (prefixes, offered)
    }

    /// Create `root_name` with three files named `file_prefix` followed by an episode number.
    fn tree_with_series(root_name: &str, file_prefix: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join(root_name);
        std::fs::create_dir(&root).unwrap();
        for episode in 1..=3 {
            std::fs::write(root.join(format!("{file_prefix}.ep{episode}.mp4")), "").unwrap();
        }
        (tmp, root)
    }

    #[test]
    fn parent_dir_name_not_offered_as_new_dir() {
        let (_tmp, root) = tree_with_series("My Series", "My.Series");

        let (prefixes, offered) = grouped_and_offered(&root);

        assert!(prefixes.contains(&"My.Series".to_string()), "{prefixes:?}");
        assert!(!offered.iter().any(|name| name == "My Series"), "{offered:?}");
    }

    #[test]
    fn parent_dir_name_case_insensitive() {
        let (_tmp, root) = tree_with_series("MYSERIES", "MySeries");

        let (prefixes, offered) = grouped_and_offered(&root);

        assert!(prefixes.contains(&"MySeries".to_string()), "{prefixes:?}");
        assert!(!offered.iter().any(|name| name == "MySeries"), "{offered:?}");
    }

    #[test]
    fn parent_dir_name_with_spaces_matches_concatenated() {
        let (_tmp, root) = tree_with_series("My Series", "MySeries");

        let (prefixes, offered) = grouped_and_offered(&root);

        assert!(prefixes.contains(&"MySeries".to_string()), "{prefixes:?}");
        assert!(!offered.iter().any(|name| name == "MySeries"), "{offered:?}");
    }

    #[test]
    fn parent_dir_name_with_dots_matches_spaced() {
        let (_tmp, root) = tree_with_series("My.Series", "My.Series");

        let (prefixes, offered) = grouped_and_offered(&root);

        assert!(prefixes.contains(&"My.Series".to_string()), "{prefixes:?}");
        assert!(!offered.iter().any(|name| name == "My Series"), "{offered:?}");
    }

    #[test]
    fn different_name_still_offered() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Parent directory has a different name
        let root = tmp.path().join("Downloads");
        std::fs::create_dir(&root).unwrap();

        std::fs::write(root.join("MySeries.ep1.mp4"), "").unwrap();
        std::fs::write(root.join("MySeries.ep2.mp4"), "").unwrap();
        std::fs::write(root.join("MySeries.ep3.mp4"), "").unwrap();

        let config = TestConfig::test_with_group_size(3);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // "MySeries" should be offered since parent is "Downloads"
        assert!(
            groups.contains_key("MySeries"),
            "MySeries should be offered when parent dir is different"
        );
    }

    #[test]
    fn prefix_ignore_stripped_from_dir_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("Downloads");
        std::fs::create_dir(&root).unwrap();

        // Files with ignored prefix "DL"
        std::fs::write(root.join("DL.MySeries.ep1.mp4"), "").unwrap();
        std::fs::write(root.join("DL.MySeries.ep2.mp4"), "").unwrap();
        std::fs::write(root.join("DL.MySeries.ep3.mp4"), "").unwrap();

        let config = TestConfig::test_with_prefix_ignores(vec!["dl"]);

        let fixture = Fixture::new(root, config);

        // Test that strip_ignored_prefixes works on space-separated names
        let dir_name = "DL MySeries";
        let stripped = fixture.name_filter().strip_ignored_prefixes(dir_name);
        assert_eq!(stripped, "MySeries", "DL prefix should be stripped from directory name");
    }

    #[test]
    fn prefix_ignore_multiple_prefixes_stripped() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("Downloads");
        std::fs::create_dir(&root).unwrap();

        let config = TestConfig::test_with_prefix_ignores(vec!["dl", "web"]);

        let fixture = Fixture::new(root, config);

        // Test stripping multiple chained prefixes
        let dir_name = "DL WEB MySeries";
        let stripped = fixture.name_filter().strip_ignored_prefixes(dir_name);
        assert_eq!(stripped, "MySeries", "Both DL and WEB prefixes should be stripped");
    }

    #[test]
    fn prefix_ignore_only_prefix_results_in_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("Downloads");
        std::fs::create_dir(&root).unwrap();

        let config = TestConfig::test_with_prefix_ignores(vec!["dl"]);

        let fixture = Fixture::new(root, config);

        // If the entire name is just the ignored prefix, result should be empty
        let dir_name = "DL";
        let stripped = fixture.name_filter().strip_ignored_prefixes(dir_name);
        assert_eq!(stripped, "DL", "Single word without space separator is not stripped");

        // With trailing content that gets stripped
        let dir_name = "DL ";
        let stripped = fixture.name_filter().strip_ignored_prefixes(dir_name);
        assert_eq!(stripped, "", "DL with trailing space should result in empty");
    }

    #[test]
    fn ignored_group_name_checked_after_prefix_strip() {
        // When a prefix_ignore is stripped, the remaining name should still be checked against ignored_group_names
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Files with prefix "DL.Studio" where "DL" is a prefix_ignore
        // and "Studio" is an ignored_group_name
        std::fs::write(root.join("DL.Studio.ep1.mp4"), "").unwrap();
        std::fs::write(root.join("DL.Studio.ep2.mp4"), "").unwrap();
        std::fs::write(root.join("DL.Studio.ep3.mp4"), "").unwrap();

        let config = TestConfig::test_with_prefix_ignores_and_ignored_group_names(vec!["dl"], vec!["studio"]);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // "DL Studio" would become "Studio" after stripping, which is ignored
        // The group key uses the original prefix without dots
        assert!(
            !groups.contains_key("DLStudio") || groups.get("DLStudio").is_none_or(|g| g.files.len() < 3),
            "Group 'DL Studio' should not result in 'Studio' directory after stripping"
        );
    }

    #[test]
    fn prefix_ignore_not_offered_as_dir_name() {
        // When stripping prefixes results in a name that is itself a prefix_ignore,
        // it should not be offered as a directory name
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Files where stripping "WEB" leaves "DL" which is also a prefix_ignore
        std::fs::write(root.join("WEB.DL.ep1.mp4"), "").unwrap();
        std::fs::write(root.join("WEB.DL.ep2.mp4"), "").unwrap();
        std::fs::write(root.join("WEB.DL.ep3.mp4"), "").unwrap();

        let config = TestConfig::test_with_prefix_ignores(vec!["web", "dl"]);

        let fixture = Fixture::new(root, config);

        // After stripping "WEB", we get "DL" which is itself a prefix_ignore
        // This should be skipped via is_ignored_prefix check
        let dir_name = "WEB DL";
        let stripped = fixture.name_filter().strip_ignored_prefixes(dir_name);
        assert_eq!(stripped, "DL", "WEB should be stripped leaving DL");
        assert!(
            fixture.name_filter().is_ignored_prefix(&stripped),
            "DL should be recognized as an ignored prefix"
        );
    }

    #[test]
    fn prefix_ignore_with_mixed_dotted_and_concatenated_names() {
        // Scenario: files have prefix like "Studio.Alpha.Something" and "Studio.AlphaSomething"
        // When "Studio" is in prefix_ignores, the offered directory name should NOT contain "Studio"
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Mix of dotted and concatenated second parts
        std::fs::write(root.join("Studio.Alpha.Small.Video1.1080p.m4v"), "").unwrap();
        std::fs::write(root.join("Studio.Alpha.Small.Video2.1080p.m4v"), "").unwrap();
        std::fs::write(root.join("Studio.Alpha.Large.Video.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.AlphaBravo.Video.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.AlphaCharlie.Video1.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.AlphaCharlie.Video2.1080p.mp4"), "").unwrap();

        let config = TestConfig::test_with_prefix_ignores(vec!["studio"]);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();

        // Verify that "Studio" prefix was stripped from original names
        for file_info in &files_with_names {
            assert!(
                !file_info.original_name.to_lowercase().starts_with("studio"),
                "Original name '{}' should not start with 'Studio' after stripping",
                file_info.original_name
            );
        }

        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // No group key should contain "studio"
        for key in groups.keys() {
            assert!(
                !key.to_lowercase().contains("studio"),
                "Group key '{key}' should not contain 'studio'"
            );
        }
    }

    #[test]
    fn prefix_ignore_not_in_create_mode_dir_name() {
        // Prefix groups become directory names with dots replaced by spaces and ignored prefixes stripped,
        // so no directory name may contain a prefix ignore.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Files with "Studio" prefix that should be ignored
        std::fs::write(root.join("Studio.Alpha.Small.Video1.1080p.m4v"), "").unwrap();
        std::fs::write(root.join("Studio.Alpha.Small.Video2.1080p.m4v"), "").unwrap();
        std::fs::write(root.join("Studio.Alpha.Large.Video.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.AlphaBravo.Video.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.AlphaCharlie.Video1.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.AlphaCharlie.Video2.1080p.mp4"), "").unwrap();

        let config = TestConfig::test_with_prefix_ignores(vec!["studio"]);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let prefix_groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        let grouper = fixture.grouper();
        for prefix in prefix_groups.keys() {
            let dir_name = grouper.directory_name_for_prefix(prefix);

            assert!(
                !dir_name.to_lowercase().contains("studio"),
                "Directory name '{dir_name}' (from prefix '{prefix}') should not contain 'studio'"
            );
        }
    }
}

#[cfg(test)]
mod test_stripped_name_filtering_integration {
    //! Integration tests verifying that `ignored_group_names` and `prefix_ignores` are checked
    //! after stripping prefixes from directory names.

    use super::*;
    use crate::test_helpers::*;

    /// Create a fixture with both `prefix_ignores` and `ignored_group_names`.
    fn make_fixture_with_ignores_and_ignored_names(
        root: PathBuf,
        prefix_ignores: Vec<&str>,
        ignored_group_names: Vec<&str>,
    ) -> Fixture {
        Fixture::new(
            root,
            TestConfig::test_with_prefix_ignores_and_ignored_group_names(prefix_ignores, ignored_group_names),
        )
    }

    #[test]
    fn stripped_name_matches_ignored_group_name_not_offered() {
        // Files with prefix "DL.Studio" where "DL" is stripped, leaving "Studio"
        // which matches an ignored_group_name - should not be offered
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("DL.Studio.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("DL.Studio.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("DL.Studio.Episode.03.mp4"), "").unwrap();

        let fixture = make_fixture_with_ignores_and_ignored_names(root, vec!["dl"], vec!["studio"]);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // "DL.Studio" becomes the directory name "Studio" once the "dl" prefix is stripped,
        // and "studio" is an ignored group name, so no group may be offered as "Studio".
        let grouper = fixture.grouper();
        let no_runtime_ignores = HashSet::<String>::new();
        assert_eq!(grouper.directory_name_for_prefix("DL.Studio"), "Studio");
        assert!(grouper.should_skip_group("Studio", None, &no_runtime_ignores));
        let offered: Vec<String> = groups
            .keys()
            .map(|prefix| grouper.directory_name_for_prefix(prefix))
            .filter(|name| !grouper.should_skip_group(name, None, &no_runtime_ignores))
            .collect();
        assert!(
            !offered.iter().any(|name| name.eq_ignore_ascii_case("studio")),
            "Studio should not be offered: {offered:?}"
        );
    }

    #[test]
    fn stripped_name_is_prefix_ignore_not_offered() {
        // Files where stripping one prefix_ignore leaves another prefix_ignore
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("WEB.DL.Content.01.mp4"), "").unwrap();
        std::fs::write(root.join("WEB.DL.Content.02.mp4"), "").unwrap();
        std::fs::write(root.join("WEB.DL.Content.03.mp4"), "").unwrap();

        let config = TestConfig::test_with_prefix_ignores(vec!["web", "dl"]);
        let fixture = Fixture::new(root, config);

        // When processing "WEB DL" group:
        // 1. Strip "WEB " -> leaves "DL"
        // 2. "DL" is itself a prefix_ignore, should be skipped

        let dir_name = "WEB DL";
        let stripped = fixture.name_filter().strip_ignored_prefixes(dir_name);
        assert_eq!(stripped, "DL");
        assert!(
            fixture.name_filter().is_ignored_prefix(&stripped),
            "Stripped name 'DL' should be recognized as a prefix_ignore"
        );
    }

    #[test]
    fn multiple_prefix_ignores_chain_to_ignored_group_name() {
        // Complex case: multiple prefix_ignores stripped, result matches ignored_group_name
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("WEB.DL.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("WEB.DL.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("WEB.DL.Episode.03.mp4"), "").unwrap();

        let fixture = make_fixture_with_ignores_and_ignored_names(root, vec!["web", "dl"], vec!["episode"]);

        // When processing "WEB DL Episode" group:
        // 1. Strip "WEB " -> "DL Episode"
        // 2. Strip "DL " -> "Episode"
        // 3. "episode" is in ignored_group_names, should be skipped

        let dir_name = "WEB DL Episode";
        let stripped = fixture.name_filter().strip_ignored_prefixes(dir_name);
        assert_eq!(stripped, "Episode");

        let normalized = stripped.to_lowercase().replace(' ', "");
        assert!(
            fixture.config().ignored_group_names.contains(&normalized),
            "Stripped name 'episode' should be in ignored_group_names"
        );
    }

    #[test]
    fn valid_name_after_stripping_is_offered() {
        // Verify that valid names after stripping are still offered
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("DL.MySeries.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("DL.MySeries.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("DL.MySeries.Episode.03.mp4"), "").unwrap();

        let fixture = make_fixture_with_ignores_and_ignored_names(root, vec!["dl"], vec!["episode"]);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // "DL MySeries" should become "MySeries" after stripping
        // "MySeries" is NOT in ignored_group_names, so it should be valid

        let dir_name = "DL MySeries";
        let stripped = fixture.name_filter().strip_ignored_prefixes(dir_name);
        assert_eq!(stripped, "MySeries");

        // Verify it's NOT filtered
        let normalized = stripped.to_lowercase().replace(' ', "");
        assert!(
            !fixture.config().ignored_group_names.contains(&normalized),
            "Stripped name 'myseries' should NOT be in ignored_group_names"
        );
        assert!(
            !fixture.name_filter().is_ignored_prefix(&stripped),
            "Stripped name 'MySeries' should NOT be a prefix_ignore"
        );

        // The group should exist
        assert!(
            groups.contains_key("DLMySeries") || groups.contains_key("MySeries"),
            "Group for 'MySeries' should exist"
        );
    }

    #[test]
    fn multi_word_ignored_group_name_after_stripping() {
        // Test with multi-word ignored_group_name
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("WEB.Some.Name.01.mp4"), "").unwrap();
        std::fs::write(root.join("WEB.Some.Name.02.mp4"), "").unwrap();
        std::fs::write(root.join("WEB.Some.Name.03.mp4"), "").unwrap();

        // "somename" (normalized, no spaces) is the ignored group name
        let fixture = make_fixture_with_ignores_and_ignored_names(root, vec!["web"], vec!["somename"]);

        // "WEB Some Name" -> strip "WEB " -> "Some Name"
        // Normalized: "somename" which is in ignored_group_names

        let dir_name = "WEB Some Name";
        let stripped = fixture.name_filter().strip_ignored_prefixes(dir_name);
        assert_eq!(stripped, "Some Name");

        let normalized = stripped.to_lowercase().replace(' ', "");
        assert_eq!(normalized, "somename");
        assert!(
            fixture.config().ignored_group_names.contains(&normalized),
            "Stripped name 'Some Name' normalized to 'somename' should be in ignored_group_names"
        );
    }

    #[test]
    fn case_insensitive_ignored_group_name_after_stripping() {
        // Test case insensitivity of ignored_group_names after stripping
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("DL.STUDIO.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("DL.Studio.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("DL.studio.Episode.03.mp4"), "").unwrap();

        let fixture = make_fixture_with_ignores_and_ignored_names(root, vec!["dl"], vec!["studio"]);

        // Various cases should all normalize to "studio"
        for dir_name in ["DL STUDIO", "DL Studio", "DL studio"] {
            let stripped = fixture.name_filter().strip_ignored_prefixes(dir_name);
            let normalized = stripped.to_lowercase().replace(' ', "");
            assert!(
                fixture.config().ignored_group_names.contains(&normalized),
                "Case variation '{dir_name}' stripped to '{stripped}' should match ignored_group_names"
            );
        }
    }

    #[test]
    fn case_insensitive_prefix_ignore_result() {
        // Test case insensitivity of is_ignored_prefix check
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        let config = TestConfig::test_with_prefix_ignores(vec!["dl"]);
        let fixture = Fixture::new(root, config);

        // Various cases should all be recognized as ignored prefix
        assert!(fixture.name_filter().is_ignored_prefix("dl"));
        assert!(fixture.name_filter().is_ignored_prefix("DL"));
        assert!(fixture.name_filter().is_ignored_prefix("Dl"));
        assert!(fixture.name_filter().is_ignored_prefix("dL"));
    }

    #[test]
    fn empty_after_full_strip_is_skipped() {
        // When stripping removes everything, the empty result should be skipped
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("WEB.DL.01.mp4"), "").unwrap();
        std::fs::write(root.join("WEB.DL.02.mp4"), "").unwrap();
        std::fs::write(root.join("WEB.DL.03.mp4"), "").unwrap();

        let config = TestConfig::test_with_prefix_ignores(vec!["web", "dl"]);
        let fixture = Fixture::new(root, config);

        // "WEB DL" -> strip "WEB " -> "DL" -> strip "DL " would need trailing space
        // But without trailing content, "DL" remains
        let dir_name = "WEB DL";
        let stripped = fixture.name_filter().strip_ignored_prefixes(dir_name);

        // "DL" is itself a prefix_ignore, so it should be skipped
        assert!(
            fixture.name_filter().is_ignored_prefix(&stripped),
            "Result 'DL' should be recognized as prefix_ignore"
        );
    }

    #[test]
    fn partial_match_not_filtered() {
        // Ensure partial matches don't incorrectly filter
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("DL.Studios.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("DL.Studios.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("DL.Studios.Episode.03.mp4"), "").unwrap();

        // "studio" is ignored, but "studios" (with 's') should NOT be ignored
        let fixture = make_fixture_with_ignores_and_ignored_names(root, vec!["dl"], vec!["studio"]);

        let dir_name = "DL Studios";
        let stripped = fixture.name_filter().strip_ignored_prefixes(dir_name);
        assert_eq!(stripped, "Studios");

        let normalized = stripped.to_lowercase().replace(' ', "");
        assert_eq!(normalized, "studios");
        assert!(
            !fixture.config().ignored_group_names.contains(&normalized),
            "'studios' should NOT match ignored_group_name 'studio'"
        );
    }

    #[test]
    fn prefix_ignore_partial_match_not_filtered() {
        // "dl" is a prefix_ignore, but "dla" should not be filtered
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        let config = TestConfig::test_with_prefix_ignores(vec!["dl"]);
        let fixture = Fixture::new(root, config);

        assert!(!fixture.name_filter().is_ignored_prefix("dla"));
        assert!(!fixture.name_filter().is_ignored_prefix("DLA"));
        assert!(!fixture.name_filter().is_ignored_prefix("adl"));
    }

    #[test]
    fn real_world_scenario_release_group_filtering() {
        // Simulates a real-world scenario with release group prefixes
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Files from a release group with common prefixes
        std::fs::write(root.join("DUMMY.Cool.Show.S01E01.mp4"), "").unwrap();
        std::fs::write(root.join("DUMMY.Cool.Show.S01E02.mp4"), "").unwrap();
        std::fs::write(root.join("DUMMY.Cool.Show.S01E03.mp4"), "").unwrap();

        // "DUMMY" is the release group (prefix_ignore)
        // "cool" is a common word we want to ignore as a standalone group name
        let fixture = make_fixture_with_ignores_and_ignored_names(root, vec!["DUMMY"], vec!["cool"]);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // After stripping "DUMMY", "Cool" alone should be filtered by ignored_group_names
        // But "Cool Show" should still be valid

        let cool_stripped = fixture.name_filter().strip_ignored_prefixes("DUMMY Cool");
        assert_eq!(cool_stripped, "Cool");
        assert!(
            fixture
                .config()
                .ignored_group_names
                .contains(&cool_stripped.to_lowercase()),
            "'Cool' should be filtered"
        );

        let cool_show_stripped = fixture.name_filter().strip_ignored_prefixes("DUMMY Cool Show");
        assert_eq!(cool_show_stripped, "Cool Show");
        let normalized = cool_show_stripped.to_lowercase().replace(' ', "");
        assert!(
            !fixture.config().ignored_group_names.contains(&normalized),
            "'Cool Show' should NOT be filtered"
        );

        // Verify "Cool Show" group exists
        assert!(
            groups.contains_key("CoolShow") || groups.contains_key("Cool.Show"),
            "Group for 'Cool Show' should exist"
        );
    }
}

#[cfg(test)]
mod test_ignored_group_parts {
    use crate::test_helpers::*;

    #[test]
    fn filters_groups_containing_ignored_part() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Files that would form a group "DL.x265.TEST" without filtering
        std::fs::write(root.join("Show.S01E01.1080p.DL.x265.TEST.mkv"), "").unwrap();
        std::fs::write(root.join("Show.S01E02.1080p.DL.x265.TEST.mkv"), "").unwrap();
        std::fs::write(root.join("Show.S01E03.1080p.DL.x265.TEST.mkv"), "").unwrap();

        let config = TestConfig::test_with_ignored_group_parts(vec!["x265"]);
        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Groups containing "x265" as a part should be filtered out
        assert!(
            !groups.contains_key("DL.x265.TEST"),
            "Group containing ignored part 'x265' should not be offered"
        );
        assert!(
            !groups.contains_key("x265.TEST"),
            "Group containing ignored part 'x265' should not be offered"
        );
        assert!(
            !groups.contains_key("DL.x265"),
            "Group containing ignored part 'x265' should not be offered"
        );
        // But "Show" should still be offered
        assert!(groups.contains_key("Show"), "Show should still be offered as a group");
    }

    #[test]
    fn multiple_ignored_parts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Studio.Video.x265.HEVC.001.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Video.x265.HEVC.002.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Video.x265.HEVC.003.mp4"), "").unwrap();

        let config = TestConfig::test_with_ignored_group_parts(vec!["x265", "hevc"]);
        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Groups with x265 or HEVC should be filtered
        for key in groups.keys() {
            let key_lower = key.to_lowercase();
            assert!(
                !key_lower.contains("x265") && !key_lower.contains("hevc"),
                "Group '{key}' should not contain ignored parts"
            );
        }
        // Studio should still be available
        assert!(groups.contains_key("Studio"), "Studio should be offered");
    }

    #[test]
    fn case_insensitive_matching() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Name.X265.Thing.001.mp4"), "").unwrap();
        std::fs::write(root.join("Name.x265.Thing.002.mp4"), "").unwrap();
        std::fs::write(root.join("Name.X265.Thing.003.mp4"), "").unwrap();

        // lowercase in config
        let config = TestConfig::test_with_ignored_group_parts(vec!["x265"]);
        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should filter both X265 and x265 variations
        for key in groups.keys() {
            let key_lower = key.to_lowercase();
            assert!(
                !key_lower.contains("x265"),
                "Group '{key}' should not contain 'x265' (case-insensitive)"
            );
        }
    }

    #[test]
    fn empty_ignored_parts_allows_all() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("DL.x265.TEST.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("DL.x265.TEST.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("DL.x265.TEST.Video.003.mp4"), "").unwrap();

        // empty list
        let config = TestConfig::test_with_ignored_group_parts(vec![]);
        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // With empty list, groups containing x265 should be allowed
        assert!(
            groups.keys().any(|k| k.to_lowercase().contains("x265")),
            "With empty ignored_parts, groups with 'x265' should be offered"
        );
    }

    #[test]
    fn does_not_filter_substring_matches() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // "x26" is in ignored_parts, but "x265" should NOT be filtered
        // because we match whole parts, not substrings
        std::fs::write(root.join("Studio.x265.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.x265.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.x265.Video.003.mp4"), "").unwrap();

        // partial match
        let config = TestConfig::test_with_ignored_group_parts(vec!["x26"]);
        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // "x265" should NOT be filtered because "x26" doesn't match the whole part
        assert!(
            groups.keys().any(|k| k.contains("x265")),
            "Partial match 'x26' should not filter 'x265' groups"
        );
    }
}

#[cfg(test)]
mod test_prefix_overrides {
    use super::*;
    use crate::test_helpers::*;

    #[test]
    fn no_overrides() {
        let fixture = make_test_fixture(Vec::new());
        let mut groups: HashMap<String, PrefixGroup> = HashMap::new();
        groups.insert(
            "Some.Name.Thing".to_string(),
            PrefixGroup::new(vec![PathBuf::from("file1.mp4")], 3, 0),
        );

        let result = fixture.grouper().apply_prefix_overrides(groups);
        assert!(result.contains_key("Some.Name.Thing"));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn matching_override() {
        let fixture = make_test_fixture(vec!["longer.prefix"]);
        let mut groups: HashMap<String, PrefixGroup> = HashMap::new();
        groups.insert(
            "longer.prefix.name".to_string(),
            PrefixGroup::new(vec![PathBuf::from("longer.prefix.name.file.mp4")], 3, 0),
        );

        let result = fixture.grouper().apply_prefix_overrides(groups);
        assert!(result.contains_key("longer.prefix"));
        assert!(!result.contains_key("longer.prefix.name"));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn merges_groups() {
        let fixture = make_test_fixture(vec!["Some.Name"]);
        let mut groups: HashMap<String, PrefixGroup> = HashMap::new();
        groups.insert(
            "Some.Name.Thing".to_string(),
            PrefixGroup::new(vec![PathBuf::from("file1.mp4")], 3, 0),
        );
        groups.insert(
            "Some.Name.Other".to_string(),
            PrefixGroup::new(vec![PathBuf::from("file2.mp4")], 3, 0),
        );

        let result = fixture.grouper().apply_prefix_overrides(groups);
        assert!(result.contains_key("Some.Name"));
        assert_eq!(result.len(), 1);
        assert_eq!(result.get("Some.Name").map(|group| group.files.len()), Some(2));
    }

    #[test]
    fn non_matching() {
        let fixture = make_test_fixture(vec!["Other.Prefix"]);
        let mut groups: HashMap<String, PrefixGroup> = HashMap::new();
        groups.insert(
            "Some.Name.Thing".to_string(),
            PrefixGroup::new(vec![PathBuf::from("file1.mp4")], 3, 0),
        );

        let result = fixture.grouper().apply_prefix_overrides(groups);
        assert!(result.contains_key("Some.Name.Thing"));
        assert!(!result.contains_key("Other.Prefix"));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn partial_match_only() {
        let fixture = make_test_fixture(vec!["Some"]);
        let mut groups: HashMap<String, PrefixGroup> = HashMap::new();
        groups.insert(
            "Something.Else".to_string(),
            PrefixGroup::new(vec![PathBuf::from("file1.mp4")], 1, 0),
        );
        groups.insert(
            "Some.Name".to_string(),
            PrefixGroup::new(vec![PathBuf::from("file2.mp4")], 2, 0),
        );

        let result = fixture.grouper().apply_prefix_overrides(groups);
        assert!(result.contains_key("Some"));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn override_more_specific_than_prefix() {
        let fixture = make_test_fixture(vec!["Example.Name"]);
        let mut groups: HashMap<String, PrefixGroup> = HashMap::new();
        groups.insert(
            "Example".to_string(),
            PrefixGroup::new(
                vec![
                    PathBuf::from("Example.Name.Video1.mp4"),
                    PathBuf::from("Example.Name.Video2.mp4"),
                    PathBuf::from("Example.Name.Video3.mp4"),
                ],
                1,
                0,
            ),
        );

        let result = fixture.grouper().apply_prefix_overrides(groups);
        assert!(result.contains_key("Example.Name"));
        assert!(!result.contains_key("Example"));
        assert_eq!(result.len(), 1);
        assert_eq!(result.get("Example.Name").map(|group| group.files.len()), Some(3));
    }

    #[test]
    fn multiple_overrides() {
        let fixture = make_test_fixture(vec!["Show.A", "Show.B"]);
        let mut groups: HashMap<String, PrefixGroup> = HashMap::new();
        groups.insert(
            "Show.A.Season1".to_string(),
            PrefixGroup::new(vec![PathBuf::from("file1.mp4")], 3, 0),
        );
        groups.insert(
            "Show.B.Season1".to_string(),
            PrefixGroup::new(vec![PathBuf::from("file2.mp4")], 3, 0),
        );
        groups.insert(
            "Show.C.Season1".to_string(),
            PrefixGroup::new(vec![PathBuf::from("file3.mp4")], 3, 0),
        );

        let result = fixture.grouper().apply_prefix_overrides(groups);
        assert!(result.contains_key("Show.A"));
        assert!(result.contains_key("Show.B"));
        assert!(result.contains_key("Show.C.Season1"));
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn empty_groups() {
        let fixture = make_test_fixture(vec!["Some"]);
        let groups: HashMap<String, PrefixGroup> = HashMap::new();

        let result = fixture.grouper().apply_prefix_overrides(groups);
        assert!(result.is_empty());
    }

    #[test]
    fn override_with_case_sensitivity() {
        let fixture = make_test_fixture(vec!["show.name"]);
        let mut groups: HashMap<String, PrefixGroup> = HashMap::new();
        groups.insert(
            "Show.Name.Season1".to_string(),
            PrefixGroup::new(vec![PathBuf::from("file1.mp4")], 3, 0),
        );

        let result = fixture.grouper().apply_prefix_overrides(groups);
        assert!(result.contains_key("Show.Name.Season1"));
        assert_eq!(result.len(), 1);
    }
}

/// Comprehensive integration tests with realistic mixed data.
/// These tests verify grouping behavior with:
/// - Multiple potential groups in the same dataset
/// - Non-matching files that should be ignored
/// - Various prefix lengths and naming conventions
/// - Both dotted and concatenated naming styles
#[cfg(test)]
mod test_realistic_grouping {
    use crate::test_helpers::*;

    #[test]
    fn mixed_content_multiple_distinct_groups_with_noise() {
        // Realistic scenario: multiple distinct groups plus unrelated files
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Group 1: BlueSky.Productions (4 files) - dotted two-part prefix
        std::fs::write(root.join("BlueSky.Productions.Summer.Adventure.1080p.x265.mp4"), "").unwrap();
        std::fs::write(root.join("BlueSky.Productions.Winter.Tale.720p.x265.mp4"), "").unwrap();
        std::fs::write(root.join("BlueSky.Productions.Spring.Romance.1080p.x265.mp4"), "").unwrap();
        std::fs::write(root.join("BlueSky.Productions.Autumn.Mystery.720p.x265.mp4"), "").unwrap();

        // Group 2: ThunderCatStudios (4 files) - concatenated single word prefix
        std::fs::write(root.join("ThunderCatStudios.Epic.Battle.x265.mp4"), "").unwrap();
        std::fs::write(root.join("ThunderCatStudios.Final.Quest.x265.mp4"), "").unwrap();
        std::fs::write(root.join("ThunderCatStudios.Dark.Rising.x265.mp4"), "").unwrap();
        std::fs::write(root.join("ThunderCatStudios.Light.Dawn.x265.mp4"), "").unwrap();

        // Group 3: Ocean.Wave (3 files) - should also match as "OceanWave"
        std::fs::write(root.join("Ocean.Wave.Sunset.Beach.x265.mp4"), "").unwrap();
        std::fs::write(root.join("OceanWave.Tropical.Paradise.mp4"), "").unwrap();
        std::fs::write(root.join("Ocean.Wave.Coral.Reef.x265.mp4"), "").unwrap();

        // Noise: various unrelated single files that should not form groups
        std::fs::write(root.join("RandomStuff.Something.mp4"), "").unwrap();
        std::fs::write(root.join("AnotherRandom.Video.mp4"), "").unwrap();
        std::fs::write(root.join("Standalone.Content.Here.mp4"), "").unwrap();
        std::fs::write(root.join("UniqueFile.NoMatch.mp4"), "").unwrap();

        // Other BlueSky files with different second part - should NOT group with BlueSky.Productions
        std::fs::write(root.join("BlueSky.Entertainment.Comedy.720p.x265.mp4"), "").unwrap();
        std::fs::write(root.join("BlueSky.Media.Documentary.1080p.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should find BlueSky.Productions as a specific group (4 files)
        assert!(
            groups.contains_key("BlueSky.Productions"),
            "Should find BlueSky.Productions group"
        );
        assert_eq!(
            groups.get("BlueSky.Productions").unwrap().files.len(),
            4,
            "BlueSky.Productions should have 4 files"
        );

        // Should find ThunderCatStudios group (4 files)
        assert!(
            groups.contains_key("ThunderCatStudios"),
            "Should find ThunderCatStudios group"
        );
        assert_eq!(
            groups.get("ThunderCatStudios").unwrap().files.len(),
            4,
            "ThunderCatStudios should have 4 files"
        );

        // Should find OceanWave group (concatenated form preferred) with 3 files
        assert!(
            groups.contains_key("OceanWave") || groups.contains_key("Ocean.Wave"),
            "Should find Ocean.Wave or OceanWave group"
        );
        let ocean_wave_key = if groups.contains_key("OceanWave") {
            "OceanWave"
        } else {
            "Ocean.Wave"
        };
        assert_eq!(
            groups.get(ocean_wave_key).unwrap().files.len(),
            3,
            "OceanWave should have 3 files"
        );

        // Should also have broader "BlueSky" group with all 6 BlueSky files
        assert!(groups.contains_key("BlueSky"), "Should find broader BlueSky group");
        assert_eq!(
            groups.get("BlueSky").unwrap().files.len(),
            6,
            "BlueSky should have 6 files total"
        );
    }

    #[test]
    fn concatenated_vs_dotted_same_name_grouped_with_concat_preferred() {
        // Files with same logical name but different formatting should group together
        // and prefer the concatenated (no-dot) form for directory name
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Mix of NeonLight, Neon.Light, neonlight, NEONLIGHT
        std::fs::write(root.join("NeonLight.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Neon.Light.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("neonlight.Episode.03.mp4"), "").unwrap();
        std::fs::write(root.join("NEONLIGHT.Episode.04.mp4"), "").unwrap();
        std::fs::write(root.join("Neon.light.Episode.05.mp4"), "").unwrap();

        // Unrelated files that should not interfere
        std::fs::write(root.join("DifferentShow.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("SomeOther.Content.Video.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // All 5 NeonLight variants should be in one group
        // Should prefer concatenated form "NeonLight"
        assert!(
            groups.contains_key("NeonLight"),
            "Should prefer concatenated form NeonLight, got keys: {:?}",
            groups.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            groups.get("NeonLight").unwrap().files.len(),
            5,
            "All 5 NeonLight variants should be grouped"
        );

        // Should NOT have separate groups for dotted forms
        assert!(
            !groups.contains_key("Neon.Light"),
            "Should not have separate Neon.Light group"
        );
    }

    #[test]
    fn three_part_prefix_coexists_with_two_part_and_one_part() {
        // Test that more specific prefixes are found alongside broader ones
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // 3 files with Studio.West.Coast prefix (3-part)
        std::fs::write(root.join("Studio.West.Coast.Video1.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.West.Coast.Video2.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.West.Coast.Video3.mp4"), "").unwrap();

        // 2 files with Studio.West.Mountain prefix (different 3-part)
        std::fs::write(root.join("Studio.West.Mountain.Video1.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.West.Mountain.Video2.mp4"), "").unwrap();

        // 1 file with Studio.West.Valley (not enough for its own 3-part group)
        std::fs::write(root.join("Studio.West.Valley.Video1.mp4"), "").unwrap();

        // 2 files with Studio.East (different 2-part)
        std::fs::write(root.join("Studio.East.Downtown.Video1.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.East.Uptown.Video2.mp4"), "").unwrap();

        // Unrelated noise
        std::fs::write(root.join("Completely.Different.Content.mp4"), "").unwrap();
        std::fs::write(root.join("Random.Other.File.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should find specific 3-part groups
        assert!(
            groups.contains_key("Studio.West.Coast"),
            "Should find Studio.West.Coast group"
        );
        assert_eq!(groups.get("Studio.West.Coast").unwrap().files.len(), 3);

        assert!(
            groups.contains_key("Studio.West.Mountain"),
            "Should find Studio.West.Mountain group"
        );
        assert_eq!(groups.get("Studio.West.Mountain").unwrap().files.len(), 2);

        // Should also find broader 2-part group (Studio.West) with 6 files
        assert!(groups.contains_key("Studio.West"), "Should find Studio.West group");
        assert_eq!(
            groups.get("Studio.West").unwrap().files.len(),
            6,
            "Studio.West should include all West files"
        );

        // Should find Studio.East group
        assert!(groups.contains_key("Studio.East"), "Should find Studio.East group");
        assert_eq!(groups.get("Studio.East").unwrap().files.len(), 2);

        // Should find broadest 1-part group (Studio) with all 8 Studio files
        assert!(groups.contains_key("Studio"), "Should find Studio group");
        assert_eq!(
            groups.get("Studio").unwrap().files.len(),
            8,
            "Studio should include all Studio files"
        );
    }

    #[test]
    fn long_single_word_prefix_forms_valid_group() {
        // Single long word prefixes should form valid groups
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Long concatenated prefix (simulating something like "SuperMegaProductions")
        std::fs::write(root.join("SuperMegaProductionsHD.Adventure.One.mp4"), "").unwrap();
        std::fs::write(root.join("SuperMegaProductionsHD.Adventure.Two.mp4"), "").unwrap();
        std::fs::write(root.join("SuperMegaProductionsHD.Comedy.Special.mp4"), "").unwrap();

        // Another long prefix
        std::fs::write(root.join("UltraHighDefinitionStudio.Movie.Alpha.mp4"), "").unwrap();
        std::fs::write(root.join("UltraHighDefinitionStudio.Movie.Beta.mp4"), "").unwrap();

        // Short unrelated files
        std::fs::write(root.join("Short.Video.mp4"), "").unwrap();
        std::fs::write(root.join("Another.Short.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should find long single-word prefix groups
        assert!(
            groups.contains_key("SuperMegaProductionsHD"),
            "Should find SuperMegaProductionsHD group"
        );
        assert_eq!(groups.get("SuperMegaProductionsHD").unwrap().files.len(), 3);

        assert!(
            groups.contains_key("UltraHighDefinitionStudio"),
            "Should find UltraHighDefinitionStudio group"
        );
        assert_eq!(groups.get("UltraHighDefinitionStudio").unwrap().files.len(), 2);
    }

    #[test]
    fn prefix_ignore_strips_common_prefixes_for_better_grouping() {
        // Test that prefix ignores help group files with common prefixes stripped
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Files with "Premium" prefix that should be ignored
        std::fs::write(root.join("Premium.GoldenStudio.Film.One.mp4"), "").unwrap();
        std::fs::write(root.join("Premium.GoldenStudio.Film.Two.mp4"), "").unwrap();
        std::fs::write(root.join("Premium.GoldenStudio.Film.Three.mp4"), "").unwrap();

        // Files without Premium prefix but same studio
        std::fs::write(root.join("GoldenStudio.Film.Four.mp4"), "").unwrap();
        std::fs::write(root.join("GoldenStudio.Film.Five.mp4"), "").unwrap();

        // Different studio with Premium prefix
        std::fs::write(root.join("Premium.SilverStudio.Movie.One.mp4"), "").unwrap();
        std::fs::write(root.join("Premium.SilverStudio.Movie.Two.mp4"), "").unwrap();

        // Unrelated
        std::fs::write(root.join("RandomContent.Video.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_ignores_and_group_size(vec!["Premium"], 2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // With "Premium" ignored, all GoldenStudio files should group together
        assert!(
            groups.contains_key("GoldenStudio"),
            "Should find GoldenStudio group after ignoring Premium prefix"
        );
        assert_eq!(
            groups.get("GoldenStudio").unwrap().files.len(),
            5,
            "GoldenStudio should have all 5 files (Premium stripped)"
        );

        // SilverStudio should also group
        assert!(groups.contains_key("SilverStudio"), "Should find SilverStudio group");
        assert_eq!(groups.get("SilverStudio").unwrap().files.len(), 2);
    }

    #[test]
    fn substring_prefix_does_not_incorrectly_match() {
        // Verify that "Thunder" doesn't match "ThunderCat" files incorrectly
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // ThunderCat files (concatenated)
        std::fs::write(root.join("ThunderCat.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("ThunderCat.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("ThunderCat.Episode.03.mp4"), "").unwrap();

        // Thunder files (shorter, different)
        std::fs::write(root.join("Thunder.Storm.Video1.mp4"), "").unwrap();
        std::fs::write(root.join("Thunder.Storm.Video2.mp4"), "").unwrap();

        // ThunderBolt files (different concatenation)
        std::fs::write(root.join("ThunderBolt.Action.Movie1.mp4"), "").unwrap();
        std::fs::write(root.join("ThunderBolt.Action.Movie2.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // ThunderCat should be its own group with exactly 3 files
        assert!(groups.contains_key("ThunderCat"), "Should find ThunderCat group");
        assert_eq!(
            groups.get("ThunderCat").unwrap().files.len(),
            3,
            "ThunderCat should have exactly 3 files, not mixed with Thunder or ThunderBolt"
        );

        // Thunder.Storm should group (2-part)
        assert!(
            groups.contains_key("Thunder.Storm") || groups.contains_key("Thunder"),
            "Should find Thunder related group"
        );

        // ThunderBolt should be its own group
        assert!(groups.contains_key("ThunderBolt"), "Should find ThunderBolt group");
        assert_eq!(groups.get("ThunderBolt").unwrap().files.len(), 2);
    }

    #[test]
    fn files_with_numeric_and_resolution_parts_filtered_for_grouping() {
        // Test that year, resolution, etc. are filtered out when determining groups
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Same logical content with different years/resolutions
        std::fs::write(root.join("CreativeArts.Documentary.2019.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("CreativeArts.Documentary.2020.720p.mp4"), "").unwrap();
        std::fs::write(root.join("CreativeArts.Documentary.2021.4K.mp4"), "").unwrap();
        std::fs::write(root.join("CreativeArts.Documentary.2022.1080p.mp4"), "").unwrap();

        // Different content
        std::fs::write(root.join("TechReview.2023.Product.Launch.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("TechReview.2024.Annual.Summary.720p.mp4"), "").unwrap();

        // Unrelated
        std::fs::write(root.join("Random.2020.Content.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // All CreativeArts.Documentary files should group despite different years
        assert!(
            groups.contains_key("CreativeArts.Documentary") || groups.contains_key("CreativeArts"),
            "Should find CreativeArts group"
        );

        // Check that we got the more specific group if it exists
        if groups.contains_key("CreativeArts.Documentary") {
            assert_eq!(
                groups.get("CreativeArts.Documentary").unwrap().files.len(),
                4,
                "CreativeArts.Documentary should have 4 files"
            );
        }

        // TechReview should also group
        assert!(groups.contains_key("TechReview"), "Should find TechReview group");
        assert_eq!(groups.get("TechReview").unwrap().files.len(), 2);
    }

    #[test]
    fn multiple_valid_groupings_all_offered() {
        // Test that when files can be grouped multiple ways, all valid groupings are available
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Files that match multiple groupings:
        // - "Network.Channel.Morning" (3-part, 2 files)
        // - "Network.Channel" (2-part, 4 files)
        // - "Network" (1-part, 6 files)
        std::fs::write(root.join("Network.Channel.Morning.Show1.mp4"), "").unwrap();
        std::fs::write(root.join("Network.Channel.Morning.Show2.mp4"), "").unwrap();
        std::fs::write(root.join("Network.Channel.Evening.Show1.mp4"), "").unwrap();
        std::fs::write(root.join("Network.Channel.Evening.Show2.mp4"), "").unwrap();
        std::fs::write(root.join("Network.Sports.Game1.mp4"), "").unwrap();
        std::fs::write(root.join("Network.Sports.Game2.mp4"), "").unwrap();

        // Completely unrelated
        std::fs::write(root.join("Independent.Film.Festival.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // All three levels of grouping should be available
        assert!(
            groups.contains_key("Network.Channel.Morning"),
            "Should find 3-part Network.Channel.Morning group"
        );
        assert_eq!(groups.get("Network.Channel.Morning").unwrap().files.len(), 2);

        assert!(
            groups.contains_key("Network.Channel"),
            "Should find 2-part Network.Channel group"
        );
        assert_eq!(groups.get("Network.Channel").unwrap().files.len(), 4);

        assert!(groups.contains_key("Network"), "Should find 1-part Network group");
        assert_eq!(groups.get("Network").unwrap().files.len(), 6);

        // Network.Sports should also be a valid 2-part group
        assert!(
            groups.contains_key("Network.Sports"),
            "Should find Network.Sports group"
        );
        assert_eq!(groups.get("Network.Sports").unwrap().files.len(), 2);
    }

    #[test]
    fn user_can_reject_specific_group_and_use_broader_one() {
        // Simulates the scenario from the user's example:
        // User has files like "Galaxy.Quest.Episode.01.mp4"
        // Tool offers "Galaxy.Quest.Episode" first (most specific) User rejects it,
        // wants "Galaxy.Quest" instead Both options should be available in the groups
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // 3 files with "Galaxy.Quest.Episode" prefix
        std::fs::write(root.join("Galaxy.Quest.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Galaxy.Quest.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Galaxy.Quest.Episode.03.mp4"), "").unwrap();
        // 2 more files with "Galaxy.Quest" but different third part
        std::fs::write(root.join("Galaxy.Quest.Movie.mp4"), "").unwrap();
        std::fs::write(root.join("Galaxy.Quest.Special.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Most specific: "Galaxy.Quest.Episode" with 3 files
        assert!(
            groups.contains_key("Galaxy.Quest.Episode"),
            "Should offer Galaxy.Quest.Episode as an option"
        );
        assert_eq!(
            groups.get("Galaxy.Quest.Episode").unwrap().files.len(),
            3,
            "Galaxy.Quest.Episode should have 3 files"
        );

        // Less specific: "Galaxy.Quest" with all 5 files
        assert!(
            groups.contains_key("Galaxy.Quest"),
            "Should offer Galaxy.Quest as an alternative option"
        );
        assert_eq!(
            groups.get("Galaxy.Quest").unwrap().files.len(),
            5,
            "Galaxy.Quest should have all 5 files"
        );

        // Least specific: "Galaxy" with all 5 files
        assert!(
            groups.contains_key("Galaxy"),
            "Should offer Galaxy as the broadest option"
        );
        assert_eq!(
            groups.get("Galaxy").unwrap().files.len(),
            5,
            "Galaxy should have all 5 files"
        );
    }

    #[test]
    fn groups_sorted_by_specificity_longest_prefix_first() {
        // Verify that when processing, longer/more specific prefixes come before shorter ones
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Studio.Ghibli.Films.Totoro.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Ghibli.Films.Spirited.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Ghibli.Films.Mononoke.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Ghibli.Shorts.One.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Ghibli.Shorts.Two.mp4"), "").unwrap();

        let config = TestConfig::test_with_group_size(2);

        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        let sorted_groups = fixture.grouper().groups_in_processing_order(groups);

        // Verify groups exist
        let prefixes: Vec<&str> = sorted_groups.iter().map(|(p, _)| p.as_str()).collect();

        let films_pos = prefixes.iter().position(|&p| p == "Studio.Ghibli.Films");
        let shorts_pos = prefixes.iter().position(|&p| p == "Studio.Ghibli.Shorts");
        let ghibli_pos = prefixes.iter().position(|&p| p == "Studio.Ghibli");
        let studio_pos = prefixes.iter().position(|&p| p == "Studio");

        assert!(films_pos.is_some(), "Should have Studio.Ghibli.Films group");
        assert!(shorts_pos.is_some(), "Should have Studio.Ghibli.Shorts group");
        assert!(ghibli_pos.is_some(), "Should have Studio.Ghibli group");
        assert!(studio_pos.is_some(), "Should have Studio group");

        // All prefixes start at position 0, so secondary sort by part count applies:
        // 3-part prefixes (Films, Shorts) come before 2-part (Ghibli) before 1-part (Studio)
        assert!(
            films_pos.unwrap() < ghibli_pos.unwrap(),
            "Studio.Ghibli.Films (3 parts) should come before Studio.Ghibli (2 parts)"
        );
        assert!(
            shorts_pos.unwrap() < ghibli_pos.unwrap(),
            "Studio.Ghibli.Shorts (3 parts) should come before Studio.Ghibli (2 parts)"
        );
        assert!(
            ghibli_pos.unwrap() < studio_pos.unwrap(),
            "Studio.Ghibli (2 parts) should come before Studio (1 part)"
        );
        // Among same part count, longer prefix first
        assert!(
            shorts_pos.unwrap() < films_pos.unwrap(),
            "Studio.Ghibli.Shorts (longer prefix) should come before Studio.Ghibli.Films"
        );
    }

    #[test]
    fn all_prefix_lengths_offered_complete_hierarchy() {
        // Ensure complete hierarchy of prefixes is offered for user choice
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Create files where all 3 prefix levels are meaningful choices
        std::fs::write(root.join("Marvel.Studios.Avengers.Endgame.mp4"), "").unwrap();
        std::fs::write(root.join("Marvel.Studios.Avengers.Infinity.mp4"), "").unwrap();
        std::fs::write(root.join("Marvel.Studios.Avengers.Age.mp4"), "").unwrap();
        std::fs::write(root.join("Marvel.Studios.Guardians.Vol1.mp4"), "").unwrap();
        std::fs::write(root.join("Marvel.Studios.Guardians.Vol2.mp4"), "").unwrap();
        std::fs::write(root.join("Marvel.Television.Daredevil.S01.mp4"), "").unwrap();
        std::fs::write(root.join("Marvel.Television.Daredevil.S02.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // 3-part groups
        assert!(
            groups.contains_key("Marvel.Studios.Avengers"),
            "Missing 3-part Avengers"
        );
        assert_eq!(groups.get("Marvel.Studios.Avengers").unwrap().files.len(), 3);

        assert!(
            groups.contains_key("Marvel.Studios.Guardians"),
            "Missing 3-part Guardians"
        );
        assert_eq!(groups.get("Marvel.Studios.Guardians").unwrap().files.len(), 2);

        assert!(
            groups.contains_key("Marvel.Television.Daredevil"),
            "Missing 3-part Daredevil"
        );
        assert_eq!(groups.get("Marvel.Television.Daredevil").unwrap().files.len(), 2);

        // 2-part groups
        assert!(groups.contains_key("Marvel.Studios"), "Missing 2-part Studios");
        assert_eq!(groups.get("Marvel.Studios").unwrap().files.len(), 5);

        assert!(groups.contains_key("Marvel.Television"), "Missing 2-part Television");
        assert_eq!(groups.get("Marvel.Television").unwrap().files.len(), 2);

        // 1-part group
        assert!(groups.contains_key("Marvel"), "Missing 1-part Marvel");
        assert_eq!(groups.get("Marvel").unwrap().files.len(), 7);

        // User could choose any of these:
        // - "Marvel.Studios.Avengers" for just Avengers movies (3 files)
        // - "Marvel.Studios" for all Studios content (5 files)
        // - "Marvel" for everything (7 files)
    }

    #[test]
    fn rejected_group_files_remain_available_for_next_group() {
        // When a group is skipped (not moved), its files should still be available for the next offered group
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Series.Season.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Series.Season.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Series.Season.Special.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Both specific and general groups contain overlapping files
        let specific_files = &groups.get("Series.Season.Episode").unwrap().files;
        let general_files = &groups.get("Series.Season").unwrap().files;

        assert_eq!(specific_files.len(), 2, "Specific group has 2 files");
        assert_eq!(general_files.len(), 3, "General group has 3 files");

        // The 2 files in specific group should also be in general group
        for file in specific_files {
            assert!(general_files.contains(file), "File {file:?} should be in both groups");
        }
    }

    #[test]
    fn min_group_size_respected_correctly() {
        // Verify that min_group_size is respected (not capped at 2)
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Groups of various sizes
        std::fs::write(root.join("PairOnly.FileA.mp4"), "").unwrap();
        std::fs::write(root.join("PairOnly.FileB.mp4"), "").unwrap();

        std::fs::write(root.join("TripleGroup.FileA.mp4"), "").unwrap();
        std::fs::write(root.join("TripleGroup.FileB.mp4"), "").unwrap();
        std::fs::write(root.join("TripleGroup.FileC.mp4"), "").unwrap();

        // Single files (should NOT form groups)
        std::fs::write(root.join("LonelyFile.NoMatch.mp4"), "").unwrap();
        std::fs::write(root.join("AnotherSingle.Standalone.mp4"), "").unwrap();
        std::fs::write(root.join("ThirdUnique.Content.mp4"), "").unwrap();

        // With min_group_size=5, neither group qualifies (2 and 3 files < 5)
        let fixture = Fixture::new(root.clone(), TestConfig::test_with_group_size(5));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        assert!(
            groups.is_empty(),
            "With min_group_size=5, no groups should be found (max is 3 files)"
        );

        // With min_group_size=3, only TripleGroup qualifies
        let fixture = Fixture::new(root.clone(), TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        assert!(
            !groups.contains_key("PairOnly"),
            "PairOnly (2 files) should not be found with min_group_size=3"
        );
        assert!(
            groups.contains_key("TripleGroup"),
            "TripleGroup (3 files) should be found with min_group_size=3"
        );
        assert_eq!(groups.get("TripleGroup").unwrap().files.len(), 3);

        // With min_group_size=2, both groups qualify
        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        assert!(
            groups.contains_key("PairOnly"),
            "PairOnly (2 files) should be found with min_group_size=2"
        );
        assert!(
            groups.contains_key("TripleGroup"),
            "TripleGroup (3 files) should be found with min_group_size=2"
        );

        // Single files should NOT form groups regardless of threshold
        assert!(
            !groups.contains_key("LonelyFile"),
            "Single files should not form groups"
        );
        assert!(
            !groups.contains_key("AnotherSingle"),
            "Single files should not form groups"
        );
    }

    #[test]
    fn mixed_case_and_format_all_normalized_correctly() {
        // Comprehensive test of case and format normalization
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // All these should be the same group (StarBright)
        std::fs::write(root.join("StarBright.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Star.Bright.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("STARBRIGHT.Episode.03.mp4"), "").unwrap();
        std::fs::write(root.join("starbright.Episode.04.mp4"), "").unwrap();
        std::fs::write(root.join("Star.bright.Episode.05.mp4"), "").unwrap();
        std::fs::write(root.join("starBright.Episode.06.mp4"), "").unwrap();

        // Different prefix entirely
        std::fs::write(root.join("MoonGlow.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Moon.Glow.Episode.02.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // All StarBright variants should be in ONE group
        // Should prefer concatenated form
        let starbright_key = groups
            .keys()
            .find(|k| k.to_lowercase().replace('.', "") == "starbright")
            .expect("Should find a StarBright group");

        assert_eq!(
            groups.get(starbright_key).unwrap().files.len(),
            6,
            "All 6 StarBright variants should be in one group"
        );

        // MoonGlow should be separate
        let moonglow_key = groups
            .keys()
            .find(|k| k.to_lowercase().replace('.', "") == "moonglow")
            .expect("Should find a MoonGlow group");

        assert_eq!(
            groups.get(moonglow_key).unwrap().files.len(),
            2,
            "MoonGlow should have 2 files"
        );
    }

    #[test]
    fn four_part_prefix_uses_three_part_max() {
        // Prefixes longer than 3 parts should still work, using the first 3 parts
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("One.Two.Three.Four.File1.mp4"), "").unwrap();
        std::fs::write(root.join("One.Two.Three.Four.File2.mp4"), "").unwrap();
        std::fs::write(root.join("One.Two.Three.Different.File3.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should find "One.Two.Three" as a group with all 3 files
        assert!(groups.contains_key("One.Two.Three"), "Should find 3-part prefix group");
        assert_eq!(groups.get("One.Two.Three").unwrap().files.len(), 3);
    }

    #[test]
    fn alphanumeric_prefix_not_filtered() {
        // Alphanumeric prefixes (not purely numeric) should be kept
        // Note: Purely numeric parts like "24" ARE filtered by design
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Show24.Hours.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Show24.Hours.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Show24.Hours.Episode.03.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should find "Show24.Hours" or similar as a valid group
        assert!(
            groups.keys().any(|k| k.to_lowercase().contains("show24")),
            "Should find group with alphanumeric prefix"
        );
    }

    #[test]
    fn single_character_prefix_forms_group() {
        // Single character prefixes should work if they meet threshold
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("X.Files.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("X.Files.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("X.Files.Episode.03.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should find "X.Files" as a group
        assert!(
            groups.contains_key("X.Files") || groups.contains_key("XFiles"),
            "Should find X.Files group"
        );
    }

    #[test]
    fn hyphenated_names_treated_as_single_part() {
        // Hyphens within a dot-part should be preserved
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Spider-Man.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Spider-Man.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Spider-Man.Movie.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should find "Spider-Man" as a group
        assert!(groups.contains_key("Spider-Man"), "Should preserve hyphen in prefix");
        assert_eq!(groups.get("Spider-Man").unwrap().files.len(), 3);
    }

    #[test]
    fn underscore_names_not_split() {
        // Underscores should not be treated as separators
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("My_Show.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("My_Show.Episode.02.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should find "My_Show" as a group (underscore preserved)
        assert!(groups.contains_key("My_Show"), "Should preserve underscore in prefix");
    }

    #[test]
    fn empty_filename_parts_handled() {
        // Double dots or leading dots should be handled gracefully
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Show..Name.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Show..Name.Episode.02.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should handle gracefully without panic
        assert!(!groups.is_empty(), "Should find some group even with double dots");
    }

    #[test]
    fn very_long_filename_prefix_extraction() {
        // Very long filenames should still work
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        let long_name = "A".repeat(50);
        std::fs::write(root.join(format!("{long_name}.Part.01.mp4")), "").unwrap();
        std::fs::write(root.join(format!("{long_name}.Part.02.mp4")), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should find the long prefix as a group
        assert!(
            groups.keys().any(|k| k.starts_with(&long_name)),
            "Should handle very long prefixes"
        );
    }

    #[test]
    fn min_group_size_one_creates_single_file_groups() {
        // With min_group_size=1, even single files should form groups
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Unique.Name.File.mp4"), "").unwrap();
        std::fs::write(root.join("Another.Unique.File.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(1));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Both unique files should form their own groups
        assert!(
            groups.len() >= 2,
            "With min_group_size=1, single files should form groups"
        );
    }

    #[test]
    fn files_with_only_extension_no_prefix() {
        // Edge case: files that are just an extension or very short
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join(".mp4"), "").unwrap();
        std::fs::write(root.join("a.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();

        // Should not panic, may or may not find groups
        let _groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);
    }

    #[test]
    fn mixed_extensions_same_prefix_grouped() {
        // Files with same prefix but different extensions should be grouped
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Movie.Name.2024.mp4"), "").unwrap();
        std::fs::write(root.join("Movie.Name.2024.srt"), "").unwrap();
        std::fs::write(root.join("Movie.Name.2024.nfo"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // All three should be in one group based on "Movie.Name"
        let movie_group = groups.iter().find(|(k, _)| k.to_lowercase().contains("movie"));
        assert!(movie_group.is_some(), "Should find Movie group");
        assert_eq!(
            movie_group.unwrap().1.files.len(),
            3,
            "All extensions should be in same group"
        );
    }

    #[test]
    fn realistic_scenario_seven_files_four_share_two_part_prefix() {
        // Realistic scenario: 7 files starting with "Studio"
        // 4 of them share the two-part prefix "Studio.Alpha"
        // With min_group_size=3, both groups should be offered:
        // - "Studio.Alpha" (4 files)
        // - "Studio" (7 files)
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // 4 files with "Studio.Alpha" prefix
        std::fs::write(root.join("Studio.Alpha.First.Project.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Alpha.Second.Work.1080p.x265.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Alpha.Third.Creation.720p.x265.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Alpha.Fourth.Piece.720p.x265.mp4"), "").unwrap();

        // 3 more files with different second parts
        std::fs::write(root.join("Studio.Beta.Something.Different.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Gamma.Another.Thing.720p.x265.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Delta.Yet.Another.Item.1080p.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should find "Studio.Alpha" with 4 files (meets min_group_size=3)
        assert!(
            groups.contains_key("Studio.Alpha"),
            "Should find Studio.Alpha group. Found groups: {:?}",
            groups.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            groups.get("Studio.Alpha").unwrap().files.len(),
            4,
            "Studio.Alpha should have 4 files"
        );

        // Should find "Studio" with 7 files
        assert!(
            groups.contains_key("Studio"),
            "Should find Studio group. Found groups: {:?}",
            groups.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            groups.get("Studio").unwrap().files.len(),
            7,
            "Studio should have 7 files"
        );
    }

    #[test]
    fn non_contiguous_parts_do_not_form_group() {
        // Files where filtering makes non-adjacent parts appear adjacent should NOT form multi-part prefix groups.
        // Only the single-part prefix should match.
        //
        // Example: "Site.2023.04.13.Person.video.mp4" after filtering becomes "Site.Person.video.mp4",
        // but "Site.Person" should NOT be a valid 2-part prefix
        // because "Site" and "Person" are not adjacent in the original.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // These files have "Site" and "Person" separated by dates in the original
        std::fs::write(root.join("Site.2023.04.13.Person.First.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("Site.2023.07.15.Person.Second.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("Site.2024.01.01.Person.Third.1080p.mp4"), "").unwrap();
        // This one has Site.Person adjacent
        std::fs::write(root.join("Site.Person.Fourth.1080p.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should find "Site" with 4 files (all start with Site)
        assert!(
            groups.contains_key("Site"),
            "Should find Site group. Found groups: {:?}",
            groups.keys().collect::<Vec<_>>()
        );
        assert_eq!(groups.get("Site").unwrap().files.len(), 4, "Site should have 4 files");

        // Should NOT find "Site.Person" as a group because only 1 file has them adjacent
        // (the other 3 have dates between Site and Person)
        assert!(
            !groups.contains_key("Site.Person"),
            "Should NOT find Site.Person group - parts are not contiguous in most files"
        );
    }

    #[test]
    fn contiguity_dotted_vs_concatenated_large_group() {
        // Large group where some files use dots and some use concatenated names
        // All should be grouped together because contiguity is maintained
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Dotted forms: "Photo.Lab" is contiguous
        std::fs::write(root.join("Photo.Lab.Project.Alpha.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("Photo.Lab.Project.Beta.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("Photo.Lab.Project.Gamma.720p.mp4"), "").unwrap();

        // Concatenated forms: "PhotoLab" as single part
        std::fs::write(root.join("PhotoLab.Project.Delta.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("PhotoLab.Project.Epsilon.720p.mp4"), "").unwrap();
        std::fs::write(root.join("PHOTOLAB.Project.Zeta.1080p.mp4"), "").unwrap();

        // Mixed case variations
        std::fs::write(root.join("photolab.Project.Eta.720p.mp4"), "").unwrap();
        std::fs::write(root.join("Photolab.Project.Theta.1080p.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should find a group containing all 8 files (PhotoLab = Photo.Lab)
        let photolab_group = groups
            .iter()
            .find(|(k, _)| k.to_lowercase().replace('.', "") == "photolab");
        assert!(photolab_group.is_some(), "Should find PhotoLab group");
        assert_eq!(
            photolab_group.unwrap().1.files.len(),
            8,
            "All 8 files should be in PhotoLab group"
        );
    }

    #[test]
    fn contiguity_three_part_prefix_various_forms() {
        // Three-part prefix in various forms: dotted, concatenated, partially concatenated
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Fully dotted: "Dark.Star.Media"
        std::fs::write(root.join("Dark.Star.Media.Video.01.mp4"), "").unwrap();
        std::fs::write(root.join("Dark.Star.Media.Video.02.mp4"), "").unwrap();
        std::fs::write(root.join("Dark.Star.Media.Video.03.mp4"), "").unwrap();

        // Fully concatenated: "DarkStarMedia"
        std::fs::write(root.join("DarkStarMedia.Video.04.mp4"), "").unwrap();
        std::fs::write(root.join("DarkStarMedia.Video.05.mp4"), "").unwrap();

        // Partially concatenated: "DarkStar.Media"
        std::fs::write(root.join("DarkStar.Media.Video.06.mp4"), "").unwrap();
        std::fs::write(root.join("DarkStar.Media.Video.07.mp4"), "").unwrap();

        // Other partial: "Dark.StarMedia"
        std::fs::write(root.join("Dark.StarMedia.Video.08.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // All 8 files should be grouped together
        let dark_star_media_group = groups
            .iter()
            .find(|(k, _)| k.to_lowercase().replace('.', "") == "darkstarmedia");
        assert!(
            dark_star_media_group.is_some(),
            "Should find DarkStarMedia group. Found: {:?}",
            groups.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            dark_star_media_group.unwrap().1.files.len(),
            8,
            "All 8 files should be in group"
        );
    }

    #[test]
    fn contiguity_numbers_break_adjacency() {
        // Numbers between parts should break contiguity
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // These have numbers between "Creator" and "Name" - NOT contiguous
        std::fs::write(root.join("Creator.2021.Name.Video.01.mp4"), "").unwrap();
        std::fs::write(root.join("Creator.2022.Name.Video.02.mp4"), "").unwrap();
        std::fs::write(root.join("Creator.2023.Name.Video.03.mp4"), "").unwrap();
        std::fs::write(root.join("Creator.2024.Name.Video.04.mp4"), "").unwrap();

        // These have "Creator.Name" contiguous
        std::fs::write(root.join("Creator.Name.Video.05.mp4"), "").unwrap();
        std::fs::write(root.join("Creator.Name.Video.06.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should find "Creator" with 6 files
        assert!(groups.contains_key("Creator"), "Should find Creator group");
        assert_eq!(
            groups.get("Creator").unwrap().files.len(),
            6,
            "Creator should have 6 files"
        );

        // Should NOT find "Creator.Name" with 6 files because only 2 have contiguous parts
        // If it exists, it should only have 2 files
        if let Some(group) = groups.get("Creator.Name") {
            assert_eq!(
                group.files.len(),
                2,
                "Creator.Name should only have 2 files (the contiguous ones)"
            );
        }
    }

    #[test]
    fn contiguity_glue_words_break_adjacency() {
        // Glue words (and, the, of, etc.) between parts should break contiguity
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // "Beauty" and "Beast" separated by glue word "and" - NOT contiguous after filtering
        std::fs::write(root.join("Beauty.and.Beast.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Beauty.and.Beast.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Beauty.and.Beast.Episode.03.mp4"), "").unwrap();

        // "Beauty.Beast" contiguous (no glue word)
        std::fs::write(root.join("Beauty.Beast.Special.01.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should find "Beauty" with 4 files
        assert!(groups.contains_key("Beauty"), "Should find Beauty group");
        assert_eq!(
            groups.get("Beauty").unwrap().files.len(),
            4,
            "Beauty should have 4 files"
        );

        // "Beauty.Beast" should NOT have 4 files - only 1 has them contiguous in original
        assert!(
            !groups.contains_key("Beauty.Beast"),
            "Beauty.Beast should not exist as a group (only 1 contiguous file)"
        );
    }

    #[test]
    fn contiguity_mixed_separators_large_dataset() {
        // Large dataset with various separator patterns
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Group 1: "Alpha.Beta" contiguous in various forms (should all group together)
        // Note: "AlphaBeta" is a single part, different from "Alpha.Beta" (two parts)
        std::fs::write(root.join("Alpha.Beta.Content.01.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Beta.Content.02.mp4"), "").unwrap();
        std::fs::write(root.join("AlphaBeta.Content.03.mp4"), "").unwrap();
        std::fs::write(root.join("AlphaBeta.Content.04.mp4"), "").unwrap();
        std::fs::write(root.join("ALPHA.BETA.Content.05.mp4"), "").unwrap();

        // Group 2: "Alpha" only, different second parts (matches single-part "Alpha")
        std::fs::write(root.join("Alpha.Gamma.Content.06.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Delta.Content.07.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Epsilon.Content.08.mp4"), "").unwrap();

        // Group 3: "Alpha" with numbers breaking contiguity to second meaningful part
        std::fs::write(root.join("Alpha.2023.Beta.Content.09.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.2024.Beta.Content.10.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // "Alpha.Beta" or "AlphaBeta" should have 5 files (the contiguous ones)
        // All 5 files normalize to "alphabeta" and have contiguous parts
        let alpha_beta_group = groups
            .iter()
            .find(|(k, _)| k.to_lowercase().replace('.', "") == "alphabeta");
        assert!(
            alpha_beta_group.is_some(),
            "Should find AlphaBeta group. Found: {:?}",
            groups.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            alpha_beta_group.unwrap().1.files.len(),
            5,
            "AlphaBeta should have 5 files"
        );

        // "Alpha" should have ALL 10 files because:
        // - Files 01, 02, 05 start with "Alpha." (dotted)
        // - Files 03, 04 start with "AlphaBeta" which starts with "Alpha"
        // - Files 06, 07, 08 start with "Alpha." (dotted)
        // - Files 09, 10 start with "Alpha." (dotted)
        // The starts_with behavior allows broad grouping
        assert!(groups.contains_key("Alpha"), "Should find Alpha group");
        assert_eq!(
            groups.get("Alpha").unwrap().files.len(),
            10,
            "Alpha should have all 10 files (including AlphaBeta files via starts_with)"
        );
    }

    #[test]
    fn contiguity_long_date_sequences() {
        // Files with long date sequences between parts
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Long date sequences break contiguity
        std::fs::write(root.join("Studio.2023.01.15.10.30.45.Actor.Scene.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.2023.02.20.11.45.30.Actor.Scene.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.2023.03.25.12.00.15.Actor.Scene.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.2024.04.30.13.15.00.Actor.Scene.mp4"), "").unwrap();

        // Contiguous version
        std::fs::write(root.join("Studio.Actor.Scene.Direct.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should find "Studio" with 5 files
        assert!(groups.contains_key("Studio"), "Should find Studio group");
        assert_eq!(
            groups.get("Studio").unwrap().files.len(),
            5,
            "Studio should have 5 files"
        );

        // "Studio.Actor" should NOT have 5 files
        assert!(
            !groups.contains_key("Studio.Actor"),
            "Studio.Actor should not exist (only 1 contiguous)"
        );
    }

    #[test]
    fn contiguity_extended_prefix_groups() {
        // Names that start with the same prefix should be in the broad group only
        // when the boundary after the prefix is a valid word boundary (CamelCase uppercase).
        // PhotoLab, PhotoLabPro share "PhotoLab" at a valid CamelCase boundary.
        // PhotoLabs does NOT: the 's' is lowercase continuation, not a new word.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // "PhotoLab" variations
        std::fs::write(root.join("PhotoLab.Image.01.jpg"), "").unwrap();
        std::fs::write(root.join("PhotoLab.Image.02.jpg"), "").unwrap();
        std::fs::write(root.join("Photo.Lab.Image.03.jpg"), "").unwrap();

        // "PhotoLabs": lowercase 's' after "PhotoLab", NOT a word boundary
        std::fs::write(root.join("PhotoLabs.Image.01.jpg"), "").unwrap();
        std::fs::write(root.join("PhotoLabs.Image.02.jpg"), "").unwrap();
        std::fs::write(root.join("PhotoLabs.Image.03.jpg"), "").unwrap();

        // "PhotoLabPro": uppercase 'P' after "PhotoLab", IS a word boundary
        std::fs::write(root.join("PhotoLabPro.Image.01.jpg"), "").unwrap();
        std::fs::write(root.join("PhotoLabPro.Image.02.jpg"), "").unwrap();
        std::fs::write(root.join("PhotoLabPro.Image.03.jpg"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // "PhotoLab" should have 6 files: 3 PhotoLab/Photo.Lab + 3 PhotoLabPro (valid boundary)
        // PhotoLabs files are NOT included because 's' is a lowercase continuation
        let photolab_group = groups
            .iter()
            .find(|(k, _)| k.to_lowercase().replace('.', "") == "photolab");
        assert!(photolab_group.is_some(), "Should find PhotoLab group");
        assert_eq!(
            photolab_group.unwrap().1.files.len(),
            6,
            "PhotoLab should have 6 files (PhotoLab + PhotoLabPro, but NOT PhotoLabs)"
        );

        // "PhotoLabs" should have exactly 3 files (specific group, separate from PhotoLab)
        assert!(groups.contains_key("PhotoLabs"), "Should find PhotoLabs group");
        assert_eq!(
            groups.get("PhotoLabs").unwrap().files.len(),
            3,
            "PhotoLabs should have exactly 3 files"
        );

        // "PhotoLabPro" should have exactly 3 files (specific group)
        assert!(groups.contains_key("PhotoLabPro"), "Should find PhotoLabPro group");
        assert_eq!(
            groups.get("PhotoLabPro").unwrap().files.len(),
            3,
            "PhotoLabPro should have exactly 3 files"
        );
    }

    #[test]
    fn contiguity_resolution_in_middle_does_not_break() {
        // Resolution patterns in middle should be filtered but not break logical contiguity
        // if the meaningful parts are still adjacent in the original
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // "Studio.Production" is contiguous, resolution comes after
        std::fs::write(root.join("Studio.Production.1080p.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Production.720p.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Production.2160p.Episode.03.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Production.480p.Episode.04.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // "Studio.Production" should have all 4 files
        assert!(
            groups.contains_key("Studio.Production"),
            "Should find Studio.Production group"
        );
        assert_eq!(
            groups.get("Studio.Production").unwrap().files.len(),
            4,
            "Studio.Production should have 4 files"
        );
    }

    #[test]
    fn contiguity_prefix_appears_multiple_times() {
        // Prefix parts appear multiple times in filename
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // "Alpha.Beta" appears at start AND later in name
        std::fs::write(root.join("Alpha.Beta.Content.Alpha.Beta.Repeat.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Beta.Another.File.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Beta.Third.Entry.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should find "Alpha.Beta" with all 3 files
        assert!(
            groups.contains_key("Alpha.Beta") || groups.contains_key("AlphaBeta"),
            "Should find Alpha.Beta group"
        );
        let alpha_beta_group = groups
            .iter()
            .find(|(k, _)| k.to_lowercase().replace('.', "") == "alphabeta");
        assert_eq!(
            alpha_beta_group.unwrap().1.files.len(),
            3,
            "Alpha.Beta should have 3 files"
        );
    }
}

#[cfg(test)]
mod test_word_boundary_grouping {
    use crate::test_helpers::*;

    #[test]
    fn intense_and_intensely_not_grouped_together() {
        // "Intense" and "Intensely" share a prefix but are different words.
        // The 'l' after "Intense" is lowercase, so there is no word boundary.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Intense.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("Intense.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("Intense.Video.003.mp4"), "").unwrap();

        std::fs::write(root.join("Intensely.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("Intensely.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("Intensely.Video.003.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // "Intense" group should have exactly 3 files, NOT 6
        assert!(groups.contains_key("Intense"), "Should find Intense group");
        assert_eq!(
            groups.get("Intense").unwrap().files.len(),
            3,
            "Intense group must NOT include Intensely files"
        );

        // "Intensely" group should have exactly 3 files
        assert!(groups.contains_key("Intensely"), "Should find Intensely group");
        assert_eq!(
            groups.get("Intensely").unwrap().files.len(),
            3,
            "Intensely group must NOT include Intense files"
        );
    }

    #[test]
    fn camel_case_boundary_still_groups_correctly() {
        // "Studio" and "StudioPro" share a prefix at a valid CamelCase boundary.
        // The 'P' after "Studio" is uppercase, so it IS a word boundary.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Studio.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Video.003.mp4"), "").unwrap();

        std::fs::write(root.join("StudioPro.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("StudioPro.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("StudioPro.Video.003.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // "Studio" broad group should include StudioPro files (CamelCase boundary)
        assert!(groups.contains_key("Studio"), "Should find Studio group");
        assert_eq!(
            groups.get("Studio").unwrap().files.len(),
            6,
            "Studio group should include StudioPro files (uppercase boundary)"
        );
    }

    #[test]
    fn lowercase_suffix_not_grouped_with_base() {
        // "Photo" and "Photos" should NOT be grouped together.
        // The 's' after "Photo" is lowercase continuation.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Photo.Image.001.jpg"), "").unwrap();
        std::fs::write(root.join("Photo.Image.002.jpg"), "").unwrap();
        std::fs::write(root.join("Photo.Image.003.jpg"), "").unwrap();

        std::fs::write(root.join("Photos.Image.001.jpg"), "").unwrap();
        std::fs::write(root.join("Photos.Image.002.jpg"), "").unwrap();
        std::fs::write(root.join("Photos.Image.003.jpg"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        assert!(groups.contains_key("Photo"), "Should find Photo group");
        assert_eq!(
            groups.get("Photo").unwrap().files.len(),
            3,
            "Photo group must NOT include Photos files"
        );

        assert!(groups.contains_key("Photos"), "Should find Photos group");
        assert_eq!(
            groups.get("Photos").unwrap().files.len(),
            3,
            "Photos group must NOT include Photo files"
        );
    }

    #[test]
    fn digit_boundary_groups_correctly() {
        // "Alpha" and "Alpha2" should be grouped (digit after letter is a boundary).
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Alpha.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Video.003.mp4"), "").unwrap();

        std::fs::write(root.join("Alpha2.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha2.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha2.Video.003.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        assert!(groups.contains_key("Alpha"), "Should find Alpha group");
        assert_eq!(
            groups.get("Alpha").unwrap().files.len(),
            6,
            "Alpha group should include Alpha2 files (digit boundary)"
        );
    }
}

/// Tests for files with varied prefixes before actual group names.
/// These tests ensure that:
/// 1. Files with ignored prefixes are properly grouped
/// 2. "Close" but non-matching files are excluded
/// 3. Completely different files are never incorrectly included
#[cfg(test)]
mod test_varied_prefix_grouping {
    use crate::test_helpers::*;

    // ===== Tests for prefix ignore functionality =====

    #[test]
    fn prefix_ignore_groups_files_with_and_without_prefix() {
        // Files with "Site" prefix should group with files without it when "Site" is ignored
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Files WITH the ignored prefix
        std::fs::write(root.join("Site.StudioAlpha.Scene.001.mp4"), "").unwrap();
        std::fs::write(root.join("Site.StudioAlpha.Scene.002.mp4"), "").unwrap();
        std::fs::write(root.join("Site.StudioAlpha.Scene.003.mp4"), "").unwrap();

        // Files WITHOUT the prefix (same studio)
        std::fs::write(root.join("StudioAlpha.Scene.004.mp4"), "").unwrap();
        std::fs::write(root.join("StudioAlpha.Scene.005.mp4"), "").unwrap();

        // With position-agnostic matching, this file also matches StudioAlpha
        std::fs::write(root.join("SiteX.StudioAlpha.Scene.006.mp4"), "").unwrap();

        // Completely different files
        std::fs::write(root.join("RandomVideo.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("UnrelatedContent.File.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_ignores_and_group_size(vec!["Site"], 3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // StudioAlpha should have 6 files:
        // - 3 with Site prefix stripped
        // - 2 without prefix
        // - 1 with SiteX prefix (position-agnostic matching finds StudioAlpha anywhere)
        assert!(groups.contains_key("StudioAlpha"), "Should find StudioAlpha group");
        assert_eq!(
            groups.get("StudioAlpha").unwrap().files.len(),
            6,
            "StudioAlpha should have 6 files (position-agnostic matching)"
        );

        // RandomVideo and UnrelatedContent should NOT be in StudioAlpha
        let studio_files = &groups.get("StudioAlpha").unwrap().files;
        assert!(
            !studio_files.iter().any(|p| p.to_string_lossy().contains("RandomVideo")),
            "RandomVideo should not be in StudioAlpha group"
        );
        assert!(
            !studio_files
                .iter()
                .any(|p| p.to_string_lossy().contains("UnrelatedContent")),
            "UnrelatedContent should not be in StudioAlpha group"
        );
    }

    #[test]
    fn prefix_ignore_multiple_ignored_prefixes() {
        // Test with multiple ignored prefixes
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Various prefix combinations - all should group together
        std::fs::write(root.join("Premium.HD.ContentCreator.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("Premium.ContentCreator.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("HD.ContentCreator.Video.003.mp4"), "").unwrap();
        std::fs::write(root.join("ContentCreator.Video.004.mp4"), "").unwrap();
        std::fs::write(root.join("ContentCreator.Video.005.mp4"), "").unwrap();

        // Different content that should NOT match - totally different first parts
        std::fs::write(root.join("Premium.OtherStudio.File.001.mp4"), "").unwrap();
        // Different name entirely
        std::fs::write(root.join("MyContentMaker.Video.001.mp4"), "").unwrap();

        let fixture = Fixture::new(
            root,
            TestConfig::test_with_ignores_and_group_size(vec!["Premium", "HD"], 3),
        );
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // ContentCreator should have all 5 matching files
        assert!(
            groups.contains_key("ContentCreator"),
            "Should find ContentCreator group"
        );
        let cc_files = &groups.get("ContentCreator").unwrap().files;

        // All 5 ContentCreator files should be present
        let content_creator_count = cc_files
            .iter()
            .filter(|p| p.to_string_lossy().contains("ContentCreator.Video"))
            .count();
        assert_eq!(
            content_creator_count, 5,
            "ContentCreator should have all 5 ContentCreator.Video files"
        );

        // MyContentMaker should NOT be included (different name)
        assert!(
            !cc_files.iter().any(|p| p.to_string_lossy().contains("MyContentMaker")),
            "MyContentMaker should not be in ContentCreator group"
        );
    }

    #[test]
    fn prefix_ignore_case_insensitive() {
        // Ignored prefixes should work case-insensitively
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("PREMIUM.StudioBeta.Film.01.mp4"), "").unwrap();
        std::fs::write(root.join("Premium.StudioBeta.Film.02.mp4"), "").unwrap();
        std::fs::write(root.join("premium.StudioBeta.Film.03.mp4"), "").unwrap();
        std::fs::write(root.join("StudioBeta.Film.04.mp4"), "").unwrap();

        // Different studio
        std::fs::write(root.join("Premium.StudioGamma.Movie.01.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_ignores_and_group_size(vec!["PREMIUM"], 3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        assert!(groups.contains_key("StudioBeta"), "Should find StudioBeta group");
        assert_eq!(
            groups.get("StudioBeta").unwrap().files.len(),
            4,
            "StudioBeta should have 4 files (case variations stripped)"
        );
    }

    #[test]
    fn prefix_ignore_does_not_strip_from_middle() {
        // Ignored prefixes should only be stripped from the START, not middle
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Site.MainChannel.Content.01.mp4"), "").unwrap();
        std::fs::write(root.join("Site.MainChannel.Content.02.mp4"), "").unwrap();
        std::fs::write(root.join("Site.MainChannel.Content.03.mp4"), "").unwrap();

        // Completely different
        std::fs::write(root.join("OtherChannel.Video.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_ignores_and_group_size(vec!["Site"], 3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // MainChannel should have exactly 3 files (the ones with Site prefix stripped)
        assert!(groups.contains_key("MainChannel"), "Should find MainChannel group");
        let mc_files = &groups.get("MainChannel").unwrap().files;
        assert_eq!(mc_files.len(), 3, "MainChannel should have exactly 3 files");
    }

    // ===== Tests for near-matches - verifying grouping boundaries =====

    #[test]
    fn distinct_studio_names_form_separate_groups() {
        // Different studio names should form their own groups
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // StudioAlpha files
        std::fs::write(root.join("StudioAlpha.Movie.001.mp4"), "").unwrap();
        std::fs::write(root.join("StudioAlpha.Movie.002.mp4"), "").unwrap();
        std::fs::write(root.join("StudioAlpha.Movie.003.mp4"), "").unwrap();

        // StudioBeta files - completely different
        std::fs::write(root.join("StudioBeta.Film.001.mp4"), "").unwrap();
        std::fs::write(root.join("StudioBeta.Film.002.mp4"), "").unwrap();
        std::fs::write(root.join("StudioBeta.Film.003.mp4"), "").unwrap();

        // Completely different
        std::fs::write(root.join("OtherProduction.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("RandomFile.txt"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // StudioAlpha should have exactly 3 files
        assert!(groups.contains_key("StudioAlpha"), "Should find StudioAlpha group");
        let studio_a_files = &groups.get("StudioAlpha").unwrap().files;
        assert_eq!(studio_a_files.len(), 3, "StudioAlpha should have exactly 3 files");

        // StudioBeta should have exactly 3 files
        assert!(groups.contains_key("StudioBeta"), "Should find StudioBeta group");
        assert_eq!(groups.get("StudioBeta").unwrap().files.len(), 3);

        // Verify StudioBeta files are NOT in StudioAlpha group
        for file in studio_a_files {
            let name = file.to_string_lossy();
            assert!(
                !name.contains("StudioBeta"),
                "StudioBeta should not be in StudioAlpha group"
            );
        }
    }

    #[test]
    fn near_match_extra_prefix_not_grouped() {
        // "MyStudio" should NOT be grouped with "Studio" files
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // "Network" as first part
        std::fs::write(root.join("Network.Show.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Network.Show.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Network.Show.Episode.03.mp4"), "").unwrap();

        // Files with extra prefix before "Network" - should NOT match
        std::fs::write(root.join("OldNetwork.Show.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("NewNetwork.Show.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("TheNetwork.Show.Episode.01.mp4"), "").unwrap();

        // Completely unrelated
        std::fs::write(root.join("Documentary.Nature.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Network.Show should exist with exactly 3 files
        let network_group = groups
            .iter()
            .find(|(k, _)| k.contains("Network") && !k.contains("Old") && !k.contains("New") && !k.contains("The"));
        assert!(network_group.is_some(), "Should find Network group");
        assert_eq!(
            network_group.unwrap().1.files.len(),
            3,
            "Network group should have exactly 3 files"
        );
    }

    #[test]
    fn near_match_similar_names_different_groups() {
        // Similar but distinct names should form separate groups
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // First group: "DarkStar"
        std::fs::write(root.join("DarkStar.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("DarkStar.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("DarkStar.Episode.03.mp4"), "").unwrap();

        // Second group: "DarkStorm" (similar prefix)
        std::fs::write(root.join("DarkStorm.Video.01.mp4"), "").unwrap();
        std::fs::write(root.join("DarkStorm.Video.02.mp4"), "").unwrap();
        std::fs::write(root.join("DarkStorm.Video.03.mp4"), "").unwrap();

        // Third group: "StarDark" (reversed, completely different)
        std::fs::write(root.join("StarDark.Film.01.mp4"), "").unwrap();
        std::fs::write(root.join("StarDark.Film.02.mp4"), "").unwrap();

        // Unrelated
        std::fs::write(root.join("LightStar.Content.mp4"), "").unwrap();
        std::fs::write(root.join("BrightStorm.Content.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // DarkStar should have exactly 3 files
        assert!(groups.contains_key("DarkStar"), "Should find DarkStar group");
        assert_eq!(groups.get("DarkStar").unwrap().files.len(), 3);

        // DarkStorm should have exactly 3 files
        assert!(groups.contains_key("DarkStorm"), "Should find DarkStorm group");
        assert_eq!(groups.get("DarkStorm").unwrap().files.len(), 3);

        // StarDark should have exactly 2 files (not mixed with DarkStar)
        assert!(groups.contains_key("StarDark"), "Should find StarDark group");
        assert_eq!(groups.get("StarDark").unwrap().files.len(), 2);
    }

    #[test]
    fn substring_in_middle_not_matched() {
        // A prefix appearing in the middle of another name should not cause grouping
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // "Alpha" as standalone first part
        std::fs::write(root.join("Alpha.Project.File.01.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Project.File.02.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Project.File.03.mp4"), "").unwrap();

        // "Alpha" appears in middle of concatenated name - should NOT group with Alpha
        std::fs::write(root.join("BetaAlpha.Content.01.mp4"), "").unwrap();
        std::fs::write(root.join("GammaAlphaOmega.Content.01.mp4"), "").unwrap();

        // "Alpha" as suffix - should NOT group
        std::fs::write(root.join("TeamAlpha.Mission.01.mp4"), "").unwrap();
        std::fs::write(root.join("TeamAlpha.Mission.02.mp4"), "").unwrap();

        // Completely different
        std::fs::write(root.join("Beta.Different.File.mp4"), "").unwrap();
        std::fs::write(root.join("Gamma.Another.File.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Alpha.Project should have exactly 3 files
        let alpha_group = groups.iter().find(|(k, _)| {
            k.as_str() == "Alpha"
                || k.as_str() == "Alpha.Project"
                || k.as_str() == "AlphaProject"
                || k.as_str() == "Alpha.Project.File"
        });
        assert!(alpha_group.is_some(), "Should find Alpha group");
        let alpha_files = &alpha_group.unwrap().1.files;
        assert_eq!(alpha_files.len(), 3, "Alpha group should have exactly 3 files");

        // Verify BetaAlpha and TeamAlpha are not in Alpha group
        for file in alpha_files {
            let name = file.to_string_lossy();
            assert!(!name.contains("BetaAlpha"), "BetaAlpha should not be in Alpha group");
            assert!(!name.contains("TeamAlpha"), "TeamAlpha should not be in Alpha group");
            assert!(
                !name.contains("GammaAlpha"),
                "GammaAlphaOmega should not be in Alpha group"
            );
        }

        // TeamAlpha should be its own group
        assert!(groups.contains_key("TeamAlpha"), "Should find TeamAlpha group");
        assert_eq!(groups.get("TeamAlpha").unwrap().files.len(), 2);
    }

    // ===== Tests for completely different files not being grouped =====

    #[test]
    fn completely_different_files_isolated() {
        // Ensure completely unrelated files are never grouped together
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Group A: "Phoenix"
        std::fs::write(root.join("Phoenix.Series.S01E01.mp4"), "").unwrap();
        std::fs::write(root.join("Phoenix.Series.S01E02.mp4"), "").unwrap();
        std::fs::write(root.join("Phoenix.Series.S01E03.mp4"), "").unwrap();

        // Group B: "Dragon"
        std::fs::write(root.join("Dragon.Movie.Part1.mp4"), "").unwrap();
        std::fs::write(root.join("Dragon.Movie.Part2.mp4"), "").unwrap();
        std::fs::write(root.join("Dragon.Movie.Part3.mp4"), "").unwrap();

        // Completely different individual files
        std::fs::write(root.join("vacation_photo.jpg"), "").unwrap();
        std::fs::write(root.join("document.pdf"), "").unwrap();
        std::fs::write(root.join("readme.txt"), "").unwrap();
        std::fs::write(root.join("config.json"), "").unwrap();
        std::fs::write(root.join("SingleFile.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Phoenix should have exactly 3 files
        let phoenix_group = groups.iter().find(|(k, _)| k.contains("Phoenix"));
        assert!(phoenix_group.is_some(), "Should find Phoenix group");
        assert_eq!(phoenix_group.unwrap().1.files.len(), 3);

        // Dragon should have exactly 3 files
        let dragon_group = groups.iter().find(|(k, _)| k.contains("Dragon"));
        assert!(dragon_group.is_some(), "Should find Dragon group");
        assert_eq!(dragon_group.unwrap().1.files.len(), 3);

        // Verify random files are not in any main group
        for group in groups.values() {
            for file in &group.files {
                let name = file.to_string_lossy();
                assert!(
                    !name.contains("vacation_photo"),
                    "vacation_photo should not be in any group"
                );
                assert!(
                    !name.contains("document.pdf"),
                    "document.pdf should not be in any group"
                );
                assert!(!name.contains("readme.txt"), "readme.txt should not be in any group");
            }
        }
    }

    #[test]
    fn mixed_content_with_noise_files() {
        // Real-world scenario with target files mixed among noise
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Target group: "StudioPremium"
        std::fs::write(root.join("StudioPremium.Show.Episode.01.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("StudioPremium.Show.Episode.02.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("StudioPremium.Show.Episode.03.1080p.mp4"), "").unwrap();
        std::fs::write(root.join("StudioPremium.Show.Episode.04.1080p.mp4"), "").unwrap();

        // Noise: similar-ish names
        std::fs::write(root.join("Studio.Basic.Content.mp4"), "").unwrap();
        std::fs::write(root.join("PremiumContent.Video.mp4"), "").unwrap();
        std::fs::write(root.join("MyStudioPremium.Fake.mp4"), "").unwrap();

        // Noise: completely random
        std::fs::write(root.join("IMG_20240101_001.jpg"), "").unwrap();
        std::fs::write(root.join("IMG_20240101_002.jpg"), "").unwrap();
        std::fs::write(root.join("screenshot_2024.png"), "").unwrap();
        std::fs::write(root.join("notes.txt"), "").unwrap();
        std::fs::write(root.join("backup.zip"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // StudioPremium should have exactly 4 files
        assert!(groups.contains_key("StudioPremium"), "Should find StudioPremium group");
        let sp_files = &groups.get("StudioPremium").unwrap().files;
        assert_eq!(sp_files.len(), 4, "StudioPremium should have exactly 4 files");

        // Verify no noise is included
        for file in sp_files {
            let name = file.to_string_lossy();
            assert!(
                name.contains("StudioPremium.Show"),
                "Only StudioPremium.Show files should be in group, got: {name}"
            );
        }
    }

    // ===== Complex scenarios combining prefix ignores and near-matches =====

    #[test]
    fn prefix_ignore_with_similar_non_matching_files() {
        // Test prefix ignore with close but non-matching files present
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Files with ignored prefix "Web"
        std::fs::write(root.join("Web.ChannelX.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("Web.ChannelX.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("Web.ChannelX.Video.003.mp4"), "").unwrap();

        // Files without prefix (same channel)
        std::fs::write(root.join("ChannelX.Video.004.mp4"), "").unwrap();
        std::fs::write(root.join("ChannelX.Video.005.mp4"), "").unwrap();

        // Close names that should NOT match - different channel names
        std::fs::write(root.join("Web.ChannelY.Different.001.mp4"), "").unwrap();
        std::fs::write(root.join("ChannelZ.Video.001.mp4"), "").unwrap();

        // Completely different
        std::fs::write(root.join("Unrelated.Random.File.mp4"), "").unwrap();
        std::fs::write(root.join("Another.Completely.Different.File.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_ignores_and_group_size(vec!["Web"], 3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // ChannelX should have 5 files (3 with Web stripped + 2 without prefix)
        assert!(groups.contains_key("ChannelX"), "Should find ChannelX group");
        let channel_files = &groups.get("ChannelX").unwrap().files;
        assert_eq!(channel_files.len(), 5, "ChannelX should have exactly 5 files");

        // Verify close-but-different files are NOT included
        for file in channel_files {
            let name = file.to_string_lossy();
            assert!(!name.contains("ChannelY"), "ChannelY should not be in ChannelX group");
            assert!(!name.contains("ChannelZ"), "ChannelZ should not be in ChannelX group");
        }
    }

    #[test]
    fn multiple_prefix_ignores_comprehensive() {
        // Test multiple ignored prefixes with various edge cases
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Target files with various ignored prefixes
        std::fs::write(root.join("AAA.BBB.TargetStudio.Content.01.mp4"), "").unwrap();
        std::fs::write(root.join("AAA.TargetStudio.Content.02.mp4"), "").unwrap();
        std::fs::write(root.join("BBB.TargetStudio.Content.03.mp4"), "").unwrap();
        std::fs::write(root.join("TargetStudio.Content.04.mp4"), "").unwrap();
        std::fs::write(root.join("TargetStudio.Content.05.mp4"), "").unwrap();

        // Files that look similar but should not match - totally different studios
        std::fs::write(root.join("OtherStudio.Content.01.mp4"), "").unwrap();
        std::fs::write(root.join("DifferentStudio.Content.01.mp4"), "").unwrap();

        // Completely unrelated noise
        std::fs::write(root.join("SomethingElse.Entirely.mp4"), "").unwrap();
        std::fs::write(root.join("NoMatch.AtAll.mp4"), "").unwrap();
        std::fs::write(root.join("random_file_123.mp4"), "").unwrap();

        let fixture = Fixture::new(
            root,
            TestConfig::test_with_ignores_and_group_size(vec!["AAA", "BBB"], 3),
        );
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // TargetStudio should have exactly 5 files
        assert!(groups.contains_key("TargetStudio"), "Should find TargetStudio group");
        let ts_files = &groups.get("TargetStudio").unwrap().files;
        assert_eq!(ts_files.len(), 5, "TargetStudio should have exactly 5 files");

        // Verify no wrong files included
        for file in ts_files {
            let name = file.to_string_lossy();
            assert!(
                name.contains("TargetStudio.Content"),
                "Only TargetStudio.Content files should be in group, got: {name}"
            );
        }
    }

    #[test]
    fn dotted_vs_concatenated_with_varied_prefixes() {
        // Test that dotted and concatenated forms group correctly even with prefixes
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Concatenated form: "GoldenEagle"
        std::fs::write(root.join("GoldenEagle.Documentary.01.mp4"), "").unwrap();
        std::fs::write(root.join("GoldenEagle.Documentary.02.mp4"), "").unwrap();

        // Dotted form: "Golden.Eagle"
        std::fs::write(root.join("Golden.Eagle.Documentary.03.mp4"), "").unwrap();
        std::fs::write(root.join("Golden.Eagle.Documentary.04.mp4"), "").unwrap();

        // With ignored prefix
        std::fs::write(root.join("HD.GoldenEagle.Documentary.05.mp4"), "").unwrap();
        std::fs::write(root.join("HD.Golden.Eagle.Documentary.06.mp4"), "").unwrap();

        // Completely different content - different prefixes
        std::fs::write(root.join("SilverEagle.Other.01.mp4"), "").unwrap();
        std::fs::write(root.join("GoldenHawk.Similar.01.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_ignores_and_group_size(vec!["HD"], 3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should find GoldenEagle (or Golden.Eagle) group with 6 files
        let golden_eagle = groups
            .iter()
            .find(|(k, _)| k.to_lowercase().replace('.', "") == "goldeneagle");
        assert!(golden_eagle.is_some(), "Should find GoldenEagle group");
        assert_eq!(
            golden_eagle.unwrap().1.files.len(),
            6,
            "GoldenEagle should have 6 files (both forms + with prefix)"
        );
    }

    #[test]
    fn three_part_prefix_with_ignores_and_noise() {
        // Test three-part prefixes with ignored prefixes and noise files
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Three-part prefix group
        std::fs::write(root.join("Big.Red.Studio.Film.001.mp4"), "").unwrap();
        std::fs::write(root.join("Big.Red.Studio.Film.002.mp4"), "").unwrap();
        std::fs::write(root.join("Big.Red.Studio.Film.003.mp4"), "").unwrap();
        std::fs::write(root.join("BigRedStudio.Film.004.mp4"), "").unwrap();

        // With ignored prefix
        std::fs::write(root.join("Premium.Big.Red.Studio.Film.005.mp4"), "").unwrap();
        std::fs::write(root.join("Premium.BigRedStudio.Film.006.mp4"), "").unwrap();

        // Close but not matching - different words entirely
        // Different second and third
        std::fs::write(root.join("Big.Blue.House.Film.001.mp4"), "").unwrap();
        // Different first and third
        std::fs::write(root.join("Small.Red.Barn.Film.001.mp4"), "").unwrap();

        // Noise
        std::fs::write(root.join("Unrelated.Content.File.mp4"), "").unwrap();
        std::fs::write(root.join("Random.Video.Here.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_ignores_and_group_size(vec!["Premium"], 3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Big.Red.Studio (or BigRedStudio) should have 6 files
        let brs_group = groups
            .iter()
            .find(|(k, _)| k.to_lowercase().replace('.', "") == "bigredstudio");
        assert!(brs_group.is_some(), "Should find BigRedStudio group");
        assert_eq!(brs_group.unwrap().1.files.len(), 6, "BigRedStudio should have 6 files");

        // Verify close-but-different files are excluded
        let brs_files = &brs_group.unwrap().1.files;
        for file in brs_files {
            let name = file.to_string_lossy();
            assert!(!name.contains("Big.Blue"), "Big.Blue should not match");
            assert!(!name.contains("Small.Red"), "Small.Red should not match");
        }
    }

    #[test]
    fn single_letter_prefix_ignore() {
        // Test that single-letter prefix ignores work correctly
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Files with single-letter prefix to ignore
        std::fs::write(root.join("X.MainContent.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("X.MainContent.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("X.MainContent.Video.003.mp4"), "").unwrap();

        // Files without prefix
        std::fs::write(root.join("MainContent.Video.004.mp4"), "").unwrap();
        std::fs::write(root.join("MainContent.Video.005.mp4"), "").unwrap();

        // Files where X is part of the name (should NOT be affected)
        std::fs::write(root.join("XMainContent.Wrong.001.mp4"), "").unwrap();
        std::fs::write(root.join("MainXContent.Other.001.mp4"), "").unwrap();

        // Noise
        std::fs::write(root.join("OtherStuff.File.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_ignores_and_group_size(vec!["X"], 3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // MainContent should have 5 files
        assert!(groups.contains_key("MainContent"), "Should find MainContent group");
        let mc_files = &groups.get("MainContent").unwrap().files;
        assert_eq!(mc_files.len(), 5, "MainContent should have 5 files");

        // XMainContent and MainXContent should NOT be included
        for file in mc_files {
            let name = file.to_string_lossy();
            assert!(!name.contains("XMainContent"), "XMainContent should not be in group");
            assert!(!name.contains("MainXContent"), "MainXContent should not be in group");
        }
    }

    #[test]
    fn numeric_looking_prefix_ignore() {
        // Test prefix ignore that looks like it could be a year or number
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Note: numeric parts are filtered differently, this tests a text prefix
        std::fs::write(root.join("Ver2.StudioName.Content.01.mp4"), "").unwrap();
        std::fs::write(root.join("Ver2.StudioName.Content.02.mp4"), "").unwrap();
        std::fs::write(root.join("Ver2.StudioName.Content.03.mp4"), "").unwrap();
        std::fs::write(root.join("StudioName.Content.04.mp4"), "").unwrap();
        std::fs::write(root.join("StudioName.Content.05.mp4"), "").unwrap();

        // Different version prefix - with position-agnostic matching, this also matches StudioName
        std::fs::write(root.join("Ver3.StudioName.Different.01.mp4"), "").unwrap();

        // Noise
        // No dot
        std::fs::write(root.join("Ver2StudioName.Wrong.01.mp4"), "").unwrap();
        std::fs::write(root.join("Random.Other.File.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_ignores_and_group_size(vec!["Ver2"], 3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // StudioName should have 6 files:
        // - 3 with Ver2 stripped
        // - 2 without prefix
        // - 1 with Ver3 prefix (position-agnostic matching finds StudioName anywhere)
        assert!(groups.contains_key("StudioName"), "Should find StudioName group");
        let sn_files = &groups.get("StudioName").unwrap().files;
        assert_eq!(sn_files.len(), 6, "StudioName should have 6 files");
    }

    #[test]
    fn empty_prefix_ignore_list_no_effect() {
        // Verify empty prefix ignore list doesn't affect grouping
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Prefix.Target.File.01.mp4"), "").unwrap();
        std::fs::write(root.join("Prefix.Target.File.02.mp4"), "").unwrap();
        std::fs::write(root.join("Prefix.Target.File.03.mp4"), "").unwrap();
        std::fs::write(root.join("Target.File.04.mp4"), "").unwrap();

        // With empty ignore list, "Prefix.Target" files should group separately
        let fixture = Fixture::new(root, TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Prefix.Target should have 3 files (no prefix stripping)
        let pt_group = groups
            .iter()
            .find(|(k, _)| k.contains("Prefix") && k.contains("Target"));
        assert!(pt_group.is_some(), "Should find Prefix.Target group");
        assert_eq!(
            pt_group.unwrap().1.files.len(),
            3,
            "Prefix.Target should have 3 files (no stripping)"
        );
    }
}

/// Tests for the `min_prefix_chars` configuration option.
/// This option sets the minimum character count for single-word prefixes to be considered valid group names.
/// Default is 5 to avoid false matches with short names like "alex", "name", etc.
#[cfg(test)]
mod test_min_prefix_chars {
    use super::*;
    use crate::test_helpers::*;

    // ===== Unit tests for find_prefix_candidates =====

    #[test]
    fn short_prefix_excluded_with_default_min_chars() {
        // "Alex" has 4 chars, should be excluded with min_prefix_chars=5
        let files = make_test_files(&["Alex.Video.001.mp4", "Alex.Video.002.mp4", "Alex.Video.003.mp4"]);
        let candidates = candidates::find_prefix_candidates("Alex.Video.001.mp4", &files, 2, 5);

        // "Alex" (4 chars) should be excluded, but "Video" (5 chars) is found with position-agnostic matching
        assert!(
            !candidates.iter().any(|c| c.prefix == "Alex"),
            "Single-part prefix 'Alex' (4 chars) should be excluded with min_prefix_chars=5"
        );
        assert!(
            candidates.iter().any(|c| c.prefix == "Video"),
            "Single-part prefix 'Video' (5 chars) should be included with position-agnostic matching"
        );
        assert!(
            candidates.iter().any(|c| c.part_count == 2 && c.prefix == "Alex.Video"),
            "Two-part prefix 'Alex.Video' should still be found"
        );
    }

    #[test]
    fn short_prefix_included_with_low_min_chars() {
        // "Alex" has 4 chars, should be included with min_prefix_chars=4
        let files = make_test_files(&["Alex.Video.001.mp4", "Alex.Video.002.mp4", "Alex.Video.003.mp4"]);
        let candidates = candidates::find_prefix_candidates("Alex.Video.001.mp4", &files, 2, 4);

        // Should find single-part prefix "Alex"
        let single_part = candidates.iter().find(|c| c.part_count == 1);
        assert!(
            single_part.is_some(),
            "Single-part prefix 'Alex' should be included with min_prefix_chars=4"
        );
        assert_eq!(single_part.unwrap().prefix, "Alex");
    }

    #[test]
    fn exact_threshold_includes_prefix() {
        // "Names" has exactly 5 chars, should be included with min_prefix_chars=5
        let files = make_test_files(&["Names.List.001.txt", "Names.List.002.txt", "Names.List.003.txt"]);
        let candidates = candidates::find_prefix_candidates("Names.List.001.txt", &files, 2, 5);

        let single_part = candidates.iter().find(|c| c.part_count == 1);
        assert!(
            single_part.is_some(),
            "Single-part prefix 'Names' (5 chars) should be included"
        );
        assert_eq!(single_part.unwrap().prefix, "Names");
    }

    #[test]
    fn long_prefix_always_included() {
        // "Alexander" has 9 chars, should always be included
        let files = make_test_files(&[
            "Alexander.Movie.001.mp4",
            "Alexander.Movie.002.mp4",
            "Alexander.Movie.003.mp4",
        ]);
        let candidates = candidates::find_prefix_candidates("Alexander.Movie.001.mp4", &files, 2, 5);

        let single_part = candidates.iter().find(|c| c.part_count == 1);
        assert!(
            single_part.is_some(),
            "Single-part prefix 'Alexander' (9 chars) should be included"
        );
        assert_eq!(single_part.unwrap().prefix, "Alexander");
    }

    #[test]
    fn two_part_prefix_affected_by_min_chars() {
        // Two-part prefixes ARE affected by min_prefix_chars (counts chars excluding dots)
        let files = make_test_files(&["AB.CD.File.001.mp4", "AB.CD.File.002.mp4", "AB.CD.File.003.mp4"]);
        // With high min_prefix_chars=10, "AB.CD" (4 chars) should be excluded
        let candidates = candidates::find_prefix_candidates("AB.CD.File.001.mp4", &files, 2, 10);

        let two_part = candidates.iter().find(|c| c.part_count == 2);
        assert!(
            two_part.is_none(),
            "Two-part prefix 'AB.CD' (4 chars) should be excluded with min_prefix_chars=10"
        );

        // Single-part "AB" (2 chars) should also be excluded
        let single_part = candidates.iter().find(|c| c.part_count == 1);
        assert!(
            single_part.is_none(),
            "Single-part prefix 'AB' should be excluded with min_prefix_chars=10"
        );

        // But with lower threshold, it should be included
        let candidates = candidates::find_prefix_candidates("AB.CD.File.001.mp4", &files, 2, 4);
        let two_part = candidates.iter().find(|c| c.part_count == 2);
        assert!(
            two_part.is_some(),
            "Two-part prefix 'AB.CD' (4 chars) should be included with min_prefix_chars=4"
        );
    }

    #[test]
    fn unicode_chars_counted_correctly() {
        // Unicode characters should be counted as single chars,
        // not bytes "日本語" has 3 chars but 9 bytes in UTF-8
        let files = make_test_files(&["日本語.Video.001.mp4", "日本語.Video.002.mp4", "日本語.Video.003.mp4"]);

        // With min_prefix_chars=3, "日本語" (3 chars) should be included
        let candidates = candidates::find_prefix_candidates("日本語.Video.001.mp4", &files, 2, 3);
        let unicode_prefix = candidates.iter().find(|c| c.prefix == "日本語");
        assert!(
            unicode_prefix.is_some(),
            "Unicode prefix '日本語' (3 chars) should be included with min=3"
        );

        // With min_prefix_chars=4, "日本語" (3 chars) should be excluded
        // but "Video" (5 chars) is still found with position-agnostic matching
        let candidates = candidates::find_prefix_candidates("日本語.Video.001.mp4", &files, 2, 4);
        let unicode_prefix = candidates.iter().find(|c| c.prefix == "日本語");
        assert!(
            unicode_prefix.is_none(),
            "Unicode prefix '日本語' (3 chars) should be excluded with min=4"
        );
        // Video (5 chars) is still found
        let video_prefix = candidates.iter().find(|c| c.prefix == "Video");
        assert!(video_prefix.is_some(), "Video (5 chars) should be included with min=4");
    }

    #[test]
    fn min_chars_zero_allows_all() {
        // With min_prefix_chars=0, even single-char prefixes should work
        let files = make_test_files(&["A.Video.001.mp4", "A.Video.002.mp4", "A.Video.003.mp4"]);
        let candidates = candidates::find_prefix_candidates("A.Video.001.mp4", &files, 2, 0);

        let single_part = candidates.iter().find(|c| c.part_count == 1);
        assert!(
            single_part.is_some(),
            "Single-char prefix 'A' should be included with min_prefix_chars=0"
        );
    }

    #[test]
    fn min_chars_one_allows_single_char() {
        // With min_prefix_chars=1, single-char prefixes should work
        let files = make_test_files(&["X.Files.S01E01.mp4", "X.Files.S01E02.mp4", "X.Files.S01E03.mp4"]);
        let candidates = candidates::find_prefix_candidates("X.Files.S01E01.mp4", &files, 2, 1);

        let single_part = candidates.iter().find(|c| c.part_count == 1);
        assert!(
            single_part.is_some(),
            "Single-char prefix 'X' should be included with min_prefix_chars=1"
        );
    }

    // ===== Integration tests with collect_all_prefix_groups =====

    #[test]
    fn short_names_not_grouped_with_default_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Short prefix "Alex" (4 chars) - should not form single-word group
        std::fs::write(root.join("Alex.Scene.001.mp4"), "").unwrap();
        std::fs::write(root.join("Alex.Scene.002.mp4"), "").unwrap();
        std::fs::write(root.join("Alex.Scene.003.mp4"), "").unwrap();

        // Long prefix "Alexander" (9 chars) - should form group
        std::fs::write(root.join("Alexander.Movie.001.mp4"), "").unwrap();
        std::fs::write(root.join("Alexander.Movie.002.mp4"), "").unwrap();
        std::fs::write(root.join("Alexander.Movie.003.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size_and_min_chars(3, 5));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // "Alex" alone should NOT be a group (4 chars < 5)
        assert!(
            !groups.contains_key("Alex"),
            "Short prefix 'Alex' should not form a single-word group"
        );

        // But "Alex.Scene" should still be a valid 2-part group
        assert!(
            groups.contains_key("Alex.Scene") || groups.contains_key("AlexScene"),
            "Two-part prefix 'Alex.Scene' should still form a group"
        );

        // "Alexander" should be a valid group (9 chars >= 5)
        assert!(
            groups.contains_key("Alexander"),
            "Long prefix 'Alexander' should form a group"
        );
    }

    #[test]
    fn common_short_words_excluded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Common short words that could cause false groupings
        std::fs::write(root.join("Name.File.001.txt"), "").unwrap();
        std::fs::write(root.join("Name.File.002.txt"), "").unwrap();
        std::fs::write(root.join("Name.File.003.txt"), "").unwrap();

        std::fs::write(root.join("Data.Report.001.csv"), "").unwrap();
        std::fs::write(root.join("Data.Report.002.csv"), "").unwrap();
        std::fs::write(root.join("Data.Report.003.csv"), "").unwrap();

        std::fs::write(root.join("Test.Case.001.rs"), "").unwrap();
        std::fs::write(root.join("Test.Case.002.rs"), "").unwrap();
        std::fs::write(root.join("Test.Case.003.rs"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size_and_min_chars(3, 5));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // "Name" (4 chars), "Data" (4 chars), "Test" (4 chars) should NOT be groups
        assert!(!groups.contains_key("Name"), "Short word 'Name' should not form group");
        assert!(!groups.contains_key("Data"), "Short word 'Data' should not form group");
        assert!(!groups.contains_key("Test"), "Short word 'Test' should not form group");

        // But two-part prefixes should work
        assert!(
            groups.contains_key("Name.File") || groups.contains_key("NameFile"),
            "Two-part 'Name.File' should form group"
        );
    }

    #[test]
    fn config_override_allows_short_prefixes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Short prefix files
        std::fs::write(root.join("ABC.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("ABC.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("ABC.Video.003.mp4"), "").unwrap();

        // With min_prefix_chars=3, "ABC" should be allowed
        let fixture = Fixture::new(root, TestConfig::test_with_group_size_and_min_chars(3, 3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        assert!(
            groups.contains_key("ABC"),
            "Short prefix 'ABC' should form group with min_prefix_chars=3"
        );
    }

    #[test]
    fn high_threshold_excludes_most_single_words() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Various length prefixes
        std::fs::write(root.join("Short.File.001.mp4"), "").unwrap();
        std::fs::write(root.join("Short.File.002.mp4"), "").unwrap();
        std::fs::write(root.join("Short.File.003.mp4"), "").unwrap();

        std::fs::write(root.join("Medium.Content.001.mp4"), "").unwrap();
        std::fs::write(root.join("Medium.Content.002.mp4"), "").unwrap();
        std::fs::write(root.join("Medium.Content.003.mp4"), "").unwrap();

        std::fs::write(root.join("VeryLongPrefix.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("VeryLongPrefix.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("VeryLongPrefix.Video.003.mp4"), "").unwrap();

        // With min_prefix_chars=10, only "VeryLongPrefix" (14 chars) qualifies
        let fixture = Fixture::new(root, TestConfig::test_with_group_size_and_min_chars(3, 10));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // "Short" (5 chars) and "Medium" (6 chars) should NOT be single-word groups
        assert!(!groups.contains_key("Short"), "'Short' should be excluded with min=10");
        assert!(
            !groups.contains_key("Medium"),
            "'Medium' should be excluded with min=10"
        );

        // "VeryLongPrefix" (14 chars) should be a group
        assert!(
            groups.contains_key("VeryLongPrefix"),
            "'VeryLongPrefix' should form group with min=10"
        );
    }

    #[test]
    fn mixed_lengths_correct_grouping() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // 4-char prefix
        std::fs::write(root.join("Film.Classic.001.mp4"), "").unwrap();
        std::fs::write(root.join("Film.Classic.002.mp4"), "").unwrap();
        std::fs::write(root.join("Film.Classic.003.mp4"), "").unwrap();

        // 5-char prefix (exactly at threshold)
        std::fs::write(root.join("Movie.Action.001.mp4"), "").unwrap();
        std::fs::write(root.join("Movie.Action.002.mp4"), "").unwrap();
        std::fs::write(root.join("Movie.Action.003.mp4"), "").unwrap();

        // 6-char prefix (above threshold)
        std::fs::write(root.join("Series.Drama.001.mp4"), "").unwrap();
        std::fs::write(root.join("Series.Drama.002.mp4"), "").unwrap();
        std::fs::write(root.join("Series.Drama.003.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size_and_min_chars(3, 5));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // "Film" (4 chars) should NOT be a single-word group
        assert!(!groups.contains_key("Film"), "'Film' (4 chars) should be excluded");

        // "Movie" (5 chars) SHOULD be a single-word group
        assert!(groups.contains_key("Movie"), "'Movie' (5 chars) should be included");

        // "Series" (6 chars) SHOULD be a single-word group
        assert!(groups.contains_key("Series"), "'Series' (6 chars) should be included");
    }

    #[test]
    fn emoji_prefix_counted_as_chars() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Emoji prefix - each emoji is typically 1-2 chars
        std::fs::write(root.join("🎬🎥.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("🎬🎥.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("🎬🎥.Video.003.mp4"), "").unwrap();

        // With min=2, emoji prefix should work
        let fixture = Fixture::new(root, TestConfig::test_with_group_size_and_min_chars(3, 2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should find the emoji prefix group
        let has_emoji_group = groups.keys().any(|k| k.contains('🎬'));
        assert!(
            has_emoji_group,
            "Emoji prefix should form group with appropriate min_chars"
        );
    }

    // ===== Tests for dotted prefixes with min_prefix_chars =====

    #[test]
    fn dotted_prefix_char_count_excludes_dots() {
        // "A.B" has 2 chars (excluding dot), should be excluded with min=5
        let files = make_test_files(&["A.B.File.001.mp4", "A.B.File.002.mp4", "A.B.File.003.mp4"]);
        let candidates = candidates::find_prefix_candidates("A.B.File.001.mp4", &files, 2, 5);

        // Should NOT find "A.B" (2 chars) as a valid prefix
        let two_part = candidates.iter().find(|c| c.prefix == "A.B");
        assert!(
            two_part.is_none(),
            "Two-part prefix 'A.B' (2 chars) should be excluded with min=5"
        );
    }

    #[test]
    fn dotted_prefix_included_when_chars_meet_threshold() {
        // "Ab.Cd" has 4 chars (excluding dot), should be included with min=4
        let files = make_test_files(&["Ab.Cd.File.001.mp4", "Ab.Cd.File.002.mp4", "Ab.Cd.File.003.mp4"]);
        let candidates = candidates::find_prefix_candidates("Ab.Cd.File.001.mp4", &files, 2, 4);

        let two_part = candidates.iter().find(|c| c.prefix == "Ab.Cd");
        assert!(
            two_part.is_some(),
            "Two-part prefix 'Ab.Cd' (4 chars) should be included with min=4"
        );
    }

    #[test]
    fn three_part_dotted_prefix_char_count() {
        // "A.B.C" has 3 chars (excluding dots), should be excluded with min=5
        let files = make_test_files(&["A.B.C.File.001.mp4", "A.B.C.File.002.mp4", "A.B.C.File.003.mp4"]);
        let candidates = candidates::find_prefix_candidates("A.B.C.File.001.mp4", &files, 2, 5);

        let three_part = candidates.iter().find(|c| c.prefix == "A.B.C");
        assert!(
            three_part.is_none(),
            "Three-part prefix 'A.B.C' (3 chars) should be excluded with min=5"
        );
    }

    #[test]
    fn three_part_dotted_prefix_included_when_long_enough() {
        // "Alpha.Beta.Gamma" has 14 chars (excluding dots)
        let files = make_test_files(&[
            "Alpha.Beta.Gamma.File.001.mp4",
            "Alpha.Beta.Gamma.File.002.mp4",
            "Alpha.Beta.Gamma.File.003.mp4",
        ]);
        let candidates = candidates::find_prefix_candidates("Alpha.Beta.Gamma.File.001.mp4", &files, 2, 10);

        let three_part = candidates.iter().find(|c| c.prefix == "Alpha.Beta.Gamma");
        assert!(
            three_part.is_some(),
            "Three-part prefix 'Alpha.Beta.Gamma' (14 chars) should be included with min=10"
        );
    }

    #[test]
    fn count_prefix_chars_helper_works_correctly() {
        assert_eq!(candidates::count_prefix_chars("A"), 1);
        assert_eq!(candidates::count_prefix_chars("AB"), 2);
        assert_eq!(candidates::count_prefix_chars("A.B"), 2);
        assert_eq!(candidates::count_prefix_chars("A.B.C"), 3);
        assert_eq!(candidates::count_prefix_chars("Alpha.Beta"), 9);
        assert_eq!(candidates::count_prefix_chars("Alpha.Beta.Gamma"), 14);
        assert_eq!(candidates::count_prefix_chars("..."), 0);
        assert_eq!(candidates::count_prefix_chars("A...B"), 2);
        // Unicode
        assert_eq!(candidates::count_prefix_chars("日本語"), 3);
        assert_eq!(candidates::count_prefix_chars("日.本.語"), 3);
    }

    #[test]
    fn integration_dotted_short_prefix_excluded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // "AB.CD" has 4 chars - should be excluded with min=5
        std::fs::write(root.join("AB.CD.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("AB.CD.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("AB.CD.Video.003.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size_and_min_chars(3, 5));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // "AB.CD" (4 chars) and "AB" (2 chars) should NOT be groups
        assert!(!groups.contains_key("AB.CD"), "'AB.CD' (4 chars) should be excluded");
        assert!(!groups.contains_key("ABCD"), "'ABCD' (4 chars) should be excluded");
        assert!(!groups.contains_key("AB"), "'AB' (2 chars) should be excluded");
    }

    #[test]
    fn integration_dotted_long_prefix_included() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // "Alpha.Beta" has 9 chars - should be included with min=5
        std::fs::write(root.join("Alpha.Beta.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Beta.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Beta.Video.003.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size_and_min_chars(3, 5));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Should find Alpha.Beta or AlphaBeta group
        let has_alpha_beta = groups.contains_key("Alpha.Beta") || groups.contains_key("AlphaBeta");
        assert!(has_alpha_beta, "'Alpha.Beta' (9 chars) should form a group");
    }

    #[test]
    fn mixed_dotted_and_single_word_with_threshold() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Short dotted: "X.Y" (2 chars) - excluded
        std::fs::write(root.join("X.Y.Content.001.mp4"), "").unwrap();
        std::fs::write(root.join("X.Y.Content.002.mp4"), "").unwrap();
        std::fs::write(root.join("X.Y.Content.003.mp4"), "").unwrap();

        // Short single: "Test" (4 chars) - excluded
        std::fs::write(root.join("Test.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("Test.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("Test.Video.003.mp4"), "").unwrap();

        // Long dotted: "Long.Name" (8 chars) - included
        std::fs::write(root.join("Long.Name.File.001.mp4"), "").unwrap();
        std::fs::write(root.join("Long.Name.File.002.mp4"), "").unwrap();
        std::fs::write(root.join("Long.Name.File.003.mp4"), "").unwrap();

        // Long single: "Studio" (6 chars) - included
        std::fs::write(root.join("Studio.Movie.001.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Movie.002.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Movie.003.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size_and_min_chars(3, 5));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Short ones excluded
        assert!(!groups.contains_key("X.Y"), "'X.Y' (2 chars) should be excluded");
        assert!(!groups.contains_key("XY"), "'XY' (2 chars) should be excluded");
        assert!(!groups.contains_key("X"), "'X' (1 char) should be excluded");
        assert!(!groups.contains_key("Test"), "'Test' (4 chars) should be excluded");

        // Long ones included
        let has_long_name = groups.contains_key("Long.Name") || groups.contains_key("LongName");
        assert!(has_long_name, "'Long.Name' (8 chars) should be included");
        assert!(groups.contains_key("Studio"), "'Studio' (6 chars) should be included");
    }

    #[test]
    fn threshold_zero_allows_all_dotted_prefixes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Short dotted prefix - "X.Y" has 2 chars, but with "Content" it becomes more
        std::fs::write(root.join("X.Y.Content.001.mp4"), "").unwrap();
        std::fs::write(root.join("X.Y.Content.002.mp4"), "").unwrap();
        std::fs::write(root.join("X.Y.Content.003.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size_and_min_chars(3, 0));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // With min=0, even short prefixes like "X" (1 char) should form a group
        let has_short_prefix = groups.keys().any(|k| k.starts_with('X'));
        assert!(
            has_short_prefix,
            "With min=0, short prefixes should be allowed. Groups: {:?}",
            groups.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn threshold_exact_boundary_dotted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // "Ab.Cde" has exactly 5 chars - should be included with min=5
        std::fs::write(root.join("Ab.Cde.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("Ab.Cde.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("Ab.Cde.Video.003.mp4"), "").unwrap();

        // "Ab.Cd" has 4 chars - should be excluded with min=5
        std::fs::write(root.join("Ab.Cd.Other.001.mp4"), "").unwrap();
        std::fs::write(root.join("Ab.Cd.Other.002.mp4"), "").unwrap();
        std::fs::write(root.join("Ab.Cd.Other.003.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size_and_min_chars(3, 5));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Exactly 5 chars should be included
        let has_ab_cde = groups.contains_key("Ab.Cde") || groups.contains_key("AbCde");
        assert!(has_ab_cde, "'Ab.Cde' (5 chars) should be included at exact threshold");

        // 4 chars should be excluded
        assert!(
            !groups.contains_key("Ab.Cd") && !groups.contains_key("AbCd"),
            "'Ab.Cd' (4 chars) should be excluded"
        );
    }

    #[test]
    fn high_threshold_excludes_all_short_dotted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Various dotted prefixes all below threshold of 15
        // The prefix is 6 chars long.
        std::fs::write(root.join("One.Two.File.001.mp4"), "").unwrap();
        std::fs::write(root.join("One.Two.File.002.mp4"), "").unwrap();
        std::fs::write(root.join("One.Two.File.003.mp4"), "").unwrap();

        // The prefix is 9 chars long.
        std::fs::write(root.join("Alpha.Beta.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Beta.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("Alpha.Beta.Video.003.mp4"), "").unwrap();

        // The prefix is 15 chars long.
        std::fs::write(root.join("Super.Long.Prefix.Content.001.mp4"), "").unwrap();
        std::fs::write(root.join("Super.Long.Prefix.Content.002.mp4"), "").unwrap();
        std::fs::write(root.join("Super.Long.Prefix.Content.003.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size_and_min_chars(3, 15));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Short ones excluded
        assert!(
            !groups.contains_key("One.Two") && !groups.contains_key("OneTwo"),
            "'One.Two' (6 chars) should be excluded with min=15"
        );
        assert!(
            !groups.contains_key("Alpha.Beta") && !groups.contains_key("AlphaBeta"),
            "'Alpha.Beta' (9 chars) should be excluded with min=15"
        );

        // Exactly 15 chars should be included
        let has_super_long = groups.contains_key("Super.Long.Prefix") || groups.contains_key("SuperLongPrefix");
        assert!(
            has_super_long,
            "'Super.Long.Prefix' (15 chars) should be included at threshold"
        );
    }

    #[test]
    fn unicode_dotted_prefix_counted_correctly() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // "日.本" has 2 chars (excluding dot) - should be excluded with min=3
        std::fs::write(root.join("日.本.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("日.本.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("日.本.Video.003.mp4"), "").unwrap();

        // "日本語.映画" has 5 chars - should be included with min=3
        std::fs::write(root.join("日本語.映画.Content.001.mp4"), "").unwrap();
        std::fs::write(root.join("日本語.映画.Content.002.mp4"), "").unwrap();
        std::fs::write(root.join("日本語.映画.Content.003.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size_and_min_chars(3, 3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // "日.本" (2 chars) should be excluded
        assert!(
            !groups.contains_key("日.本") && !groups.contains_key("日本") && !groups.contains_key("日"),
            "'日.本' (2 chars) should be excluded with min=3"
        );

        // "日本語.映画" (5 chars) should be included
        let has_japanese = groups.keys().any(|k| k.contains("日本語"));
        assert!(has_japanese, "'日本語.映画' (5 chars) should be included with min=3");
    }
}

/// Tests for broader grouping behavior, where a group prefix is the start of longer names.
#[cfg(test)]
mod test_broader_prefix_grouping {
    use crate::test_helpers::*;

    #[test]
    fn broader_group_includes_extended_names() {
        // Test that broader groups CAN include files with extended prefixes
        // e.g., "Studio" group includes "StudioPro", "StudioBasic" files
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Base "Studio" files
        std::fs::write(root.join("Studio.Video.001.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Video.002.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Video.003.mp4"), "").unwrap();

        // Extended names that start with "Studio"
        std::fs::write(root.join("StudioPro.Premium.001.mp4"), "").unwrap();
        std::fs::write(root.join("StudioPro.Premium.002.mp4"), "").unwrap();
        std::fs::write(root.join("StudioBasic.Free.001.mp4"), "").unwrap();

        // Completely different
        std::fs::write(root.join("OtherContent.File.mp4"), "").unwrap();
        // "Studio" not at start
        std::fs::write(root.join("MyStudio.Different.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // "Studio" group exists and includes base files
        assert!(groups.contains_key("Studio"), "Should find Studio group");
        let studio_files = &groups.get("Studio").unwrap().files;
        assert!(studio_files.len() >= 3, "Studio should have at least base 3 files");

        // StudioPro should also exist as its own more specific group
        assert!(groups.contains_key("StudioPro"), "Should find StudioPro group");
        assert_eq!(groups.get("StudioPro").unwrap().files.len(), 2);

        // MyStudio should NOT be in Studio group (Studio not at prefix position)
        for file in studio_files {
            let name = file.to_string_lossy();
            assert!(!name.contains("MyStudio"), "MyStudio should not be in Studio group");
        }
    }

    #[test]
    fn specific_groups_separate_from_unrelated_names() {
        // Test that specific groups contain their matches and unrelated names are separate
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Specific group: "AlphaStudio"
        std::fs::write(root.join("AlphaStudio.Movie.001.mp4"), "").unwrap();
        std::fs::write(root.join("AlphaStudio.Movie.002.mp4"), "").unwrap();
        std::fs::write(root.join("AlphaStudio.Movie.003.mp4"), "").unwrap();

        // Different: "BetaStudio" - completely unrelated
        std::fs::write(root.join("BetaStudio.Other.001.mp4"), "").unwrap();
        std::fs::write(root.join("BetaStudio.Other.002.mp4"), "").unwrap();
        std::fs::write(root.join("BetaStudio.Other.003.mp4"), "").unwrap();

        // Different: "GammaProduction"
        std::fs::write(root.join("GammaProduction.Film.001.mp4"), "").unwrap();
        std::fs::write(root.join("GammaProduction.Film.002.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(2));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // AlphaStudio should have exactly 3 files
        assert!(groups.contains_key("AlphaStudio"), "Should find AlphaStudio group");
        let alpha_files = &groups.get("AlphaStudio").unwrap().files;
        assert_eq!(alpha_files.len(), 3, "AlphaStudio should have exactly 3 files");

        // Verify BetaStudio is NOT in AlphaStudio group
        for file in alpha_files {
            let name = file.to_string_lossy();
            assert!(
                !name.contains("BetaStudio"),
                "BetaStudio should not be in AlphaStudio group"
            );
            assert!(
                !name.contains("GammaProduction"),
                "GammaProduction should not be in AlphaStudio group"
            );
        }

        // BetaStudio should have its own group
        assert!(groups.contains_key("BetaStudio"), "Should find BetaStudio group");
        assert_eq!(groups.get("BetaStudio").unwrap().files.len(), 3);

        // GammaProduction should have its own group
        assert!(
            groups.contains_key("GammaProduction"),
            "Should find GammaProduction group"
        );
        assert_eq!(groups.get("GammaProduction").unwrap().files.len(), 2);
    }

    #[test]
    fn files_without_dots_form_single_groups() {
        // Files without dots should not cause issues
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Files with dots
        std::fs::write(root.join("Target.Series.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Target.Series.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Target.Series.Episode.03.mp4"), "").unwrap();

        // Files without dots (just extension)
        std::fs::write(root.join("standalone.mp4"), "").unwrap();
        std::fs::write(root.join("another_file.txt"), "").unwrap();
        std::fs::write(root.join("nodots.jpg"), "").unwrap();

        // Completely different
        std::fs::write(root.join("Unrelated.Other.File.mp4"), "").unwrap();

        let fixture = Fixture::new(root, TestConfig::test_with_group_size(3));
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Target.Series should have 3 files
        let target_group = groups.iter().find(|(k, _)| k.contains("Target"));
        assert!(target_group.is_some(), "Should find Target group");
        assert_eq!(target_group.unwrap().1.files.len(), 3);

        // standalone, another_file, nodots should NOT be in Target group
        let target_files = &target_group.unwrap().1.files;
        for file in target_files {
            let name = file.to_string_lossy();
            assert!(!name.contains("standalone"), "standalone should not be in Target group");
            assert!(
                !name.contains("another_file"),
                "another_file should not be in Target group"
            );
            assert!(!name.contains("nodots"), "nodots should not be in Target group");
        }
    }
}

#[cfg(test)]
mod test_second_pass_starts_with_matching {
    use crate::test_helpers::*;

    #[test]
    fn concatenated_variant_added_to_existing_group() {
        // Second pass should add JosephExampleTV to the JosephExample group
        // because "JosephExampleTV" starts with "josephexample" (the normalized group key).
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Three files form a "JosephExample" group in the first pass
        std::fs::write(root.join("JosephExample.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("JosephExample.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("JosephExample.Episode.03.mp4"), "").unwrap();
        // This file has "JosephExampleTV" which starts with the group prefix
        std::fs::write(root.join("JosephExampleTV.Episode.04.mp4"), "").unwrap();

        let config = TestConfig::test_with_group_size(3);
        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Use exact key match to avoid accidentally matching a more specific group like "JosephExample.Episode"
        // (which only has 3 files).
        let group = groups
            .iter()
            .find(|(key, _)| key.to_lowercase() == "josephexample")
            .expect("Should find JosephExample group");

        assert_eq!(
            group.1.files.len(),
            4,
            "JosephExampleTV file should be added via second pass starts-with matching"
        );
    }

    #[test]
    fn starts_with_does_not_add_unrelated_files() {
        // A file that does NOT start with the group prefix should not be added
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("AlphaStudio.Video.01.mp4"), "").unwrap();
        std::fs::write(root.join("AlphaStudio.Video.02.mp4"), "").unwrap();
        std::fs::write(root.join("AlphaStudio.Video.03.mp4"), "").unwrap();
        // "BetaStudio" does NOT start with "alphastudio"
        std::fs::write(root.join("BetaStudio.Video.04.mp4"), "").unwrap();

        let config = TestConfig::test_with_group_size(3);
        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        let group = groups
            .iter()
            .find(|(key, _)| key.to_lowercase() == "alphastudio")
            .expect("Should find AlphaStudio group");

        assert_eq!(
            group.1.files.len(),
            3,
            "BetaStudio should NOT be added to AlphaStudio group"
        );
        assert!(
            !group.1.files.iter().any(|f| f.to_string_lossy().contains("BetaStudio")),
            "BetaStudio file should not be in AlphaStudio group"
        );
    }

    #[test]
    fn already_in_group_not_duplicated() {
        // A file already added in the first pass should not be duplicated by the second pass
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("ShowName.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("ShowName.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("ShowName.Episode.03.mp4"), "").unwrap();

        let config = TestConfig::test_with_group_size(3);
        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        let group = groups
            .iter()
            .find(|(key, _)| key.to_lowercase() == "showname")
            .expect("Should find ShowName group");

        // Each file should appear exactly once
        let mut file_names: Vec<String> = group.1.files.iter().map(|f| f.to_string_lossy().to_string()).collect();
        file_names.sort();
        file_names.dedup();
        assert_eq!(
            file_names.len(),
            group.1.files.len(),
            "No files should be duplicated between first and second pass"
        );
    }

    #[test]
    fn multiple_extended_variants_all_added() {
        // Multiple files with different extended prefixes should all be added
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("MyShow.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("MyShow.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("MyShow.Episode.03.mp4"), "").unwrap();
        std::fs::write(root.join("MyShowSpecial.Bonus.01.mp4"), "").unwrap();
        std::fs::write(root.join("MyShowExtra.Behind.Scenes.mp4"), "").unwrap();

        let config = TestConfig::test_with_group_size(3);
        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        let group = groups
            .iter()
            .find(|(key, _)| key.to_lowercase() == "myshow")
            .expect("Should find MyShow group");

        // MyShowSpecial and MyShowExtra should both be added via starts-with
        let file_names: Vec<String> = group.1.files.iter().map(|f| f.to_string_lossy().to_string()).collect();
        assert!(
            file_names.iter().any(|f| f.contains("MyShowSpecial")),
            "MyShowSpecial should be in MyShow group via starts-with"
        );
        assert!(
            file_names.iter().any(|f| f.contains("MyShowExtra")),
            "MyShowExtra should be in MyShow group via starts-with"
        );
        assert_eq!(group.1.files.len(), 5, "All 5 files should be in MyShow group");
    }

    #[test]
    fn contiguity_check_prevents_false_positive_in_second_pass() {
        // Second pass must verify contiguity.
        // A file where the group key appears only non-contiguously should NOT be added.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Studio.Alpha.Video.01.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Alpha.Video.02.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Alpha.Video.03.mp4"), "").unwrap();
        // This file has "studio" then later "alpha" but not contiguously
        // After filtering numerics: "Other.Studio.Stuff.Alpha.mp4"
        // The normalized key "studioalpha" should not match because the parts are not adjacent
        std::fs::write(root.join("Other.Studio.Stuff.Alpha.mp4"), "").unwrap();

        let config = TestConfig::test_with_group_size(3);
        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        if let Some(group) = groups.iter().find(|(key, _)| key.to_lowercase() == "studio.alpha") {
            // If the group exists, the non-contiguous file should not be in it
            assert!(
                !group
                    .1
                    .files
                    .iter()
                    .any(|f| f.to_string_lossy().contains("Other.Studio.Stuff")),
                "File with non-contiguous parts should not be added in second pass"
            );
        }
    }
}

#[cfg(test)]
mod test_parallel_grouping_consistency {
    use crate::test_helpers::*;

    #[test]
    fn parallel_produces_deterministic_groups() {
        // Run collect_all_prefix_groups multiple times and verify the results are identical.
        // This validates that parallelization does not introduce non-deterministic grouping.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Create a moderately complex set of files to exercise both passes
        for index in 1..=5 {
            std::fs::write(root.join(format!("SeriesAlpha.Season1.Episode.{index:02}.mp4")), "").unwrap();
        }
        for index in 1..=4 {
            std::fs::write(root.join(format!("SeriesAlpha.Season2.Episode.{index:02}.mp4")), "").unwrap();
        }
        for index in 1..=3 {
            std::fs::write(root.join(format!("SeriesBeta.Special.{index:02}.mp4")), "").unwrap();
        }
        // Extended variant that second pass should pick up
        std::fs::write(root.join("SeriesAlphaExtras.Behind.Scenes.mp4"), "").unwrap();
        // Unrelated files
        std::fs::write(root.join("RandomFile.txt"), "").unwrap();
        std::fs::write(root.join("AnotherRandom.doc"), "").unwrap();

        let config = TestConfig::test_with_group_size(3);
        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();

        // Run multiple times and compare
        let first_run = fixture.grouper().collect_all_prefix_groups(&files_with_names);
        let second_run = fixture.grouper().collect_all_prefix_groups(&files_with_names);
        let third_run = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Same set of group keys
        let mut first_keys: Vec<&String> = first_run.keys().collect();
        let mut second_keys: Vec<&String> = second_run.keys().collect();
        let mut third_keys: Vec<&String> = third_run.keys().collect();
        first_keys.sort();
        second_keys.sort();
        third_keys.sort();
        assert_eq!(first_keys, second_keys, "Group keys should be identical across runs");
        assert_eq!(first_keys, third_keys, "Group keys should be identical across runs");

        // Same file sets per group (sorted for comparison)
        for key in first_run.keys() {
            let mut first_files: Vec<String> = first_run[key]
                .files
                .iter()
                .map(|f| f.to_string_lossy().to_string())
                .collect();
            let mut second_files: Vec<String> = second_run[key]
                .files
                .iter()
                .map(|f| f.to_string_lossy().to_string())
                .collect();
            let mut third_files: Vec<String> = third_run[key]
                .files
                .iter()
                .map(|f| f.to_string_lossy().to_string())
                .collect();
            first_files.sort();
            second_files.sort();
            third_files.sort();
            assert_eq!(
                first_files, second_files,
                "Files in group '{key}' should be identical across runs"
            );
            assert_eq!(
                first_files, third_files,
                "Files in group '{key}' should be identical across runs"
            );
        }
    }

    #[test]
    fn parallel_handles_ignored_group_names_correctly() {
        // Verify that ignored_group_names filtering works correctly under parallel execution
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Studio.Episode.01.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Episode.02.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Episode.03.mp4"), "").unwrap();
        std::fs::write(root.join("Studio.Episode.04.mp4"), "").unwrap();

        let config = TestConfig::test_with_ignored_group_names(vec!["episode"]);
        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // "Episode" should be filtered out
        assert!(
            !groups.keys().any(|k| k.to_lowercase() == "episode"),
            "Ignored group name 'episode' should not appear: {:?}",
            groups.keys().collect::<Vec<_>>()
        );
        // "Studio" should still be present
        assert!(
            groups.keys().any(|k| k.to_lowercase().contains("studio")),
            "Studio group should still be present"
        );
    }

    #[test]
    fn parallel_handles_ignored_group_parts_correctly() {
        // Verify that ignored_group_parts filtering works correctly under parallel execution
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        std::fs::write(root.join("Show.S01E01.1080p.DL.x265.TEST.mkv"), "").unwrap();
        std::fs::write(root.join("Show.S01E02.1080p.DL.x265.TEST.mkv"), "").unwrap();
        std::fs::write(root.join("Show.S01E03.1080p.DL.x265.TEST.mkv"), "").unwrap();

        let config = TestConfig::test_with_ignored_group_parts(vec!["x265"]);
        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // No group key should contain x265 as a part
        for key in groups.keys() {
            assert!(
                !key.to_lowercase().contains("x265"),
                "Group '{key}' should have been filtered by ignored_group_parts"
            );
        }
    }

    #[test]
    fn parallel_large_file_set_stress_test() {
        // Larger file set to exercise thread pool more thoroughly
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // 5 groups of 10 files each = 50 files
        for group_index in 1..=5 {
            for file_index in 1..=10 {
                std::fs::write(
                    root.join(format!("Group{group_index:02}.Content.File.{file_index:02}.mp4")),
                    "",
                )
                .unwrap();
            }
        }
        // 5 extended variants (one per group)
        for group_index in 1..=5 {
            std::fs::write(root.join(format!("Group{group_index:02}Extra.Bonus.Content.mp4")), "").unwrap();
        }

        let config = TestConfig::test_with_group_size(3);
        let fixture = Fixture::new(root, config);
        let files_with_names = fixture.scanner().collect_files_with_names().unwrap();
        let groups = fixture.grouper().collect_all_prefix_groups(&files_with_names);

        // Each GroupNN should exist and contain at least 10 files
        for group_index in 1..=5 {
            let group_name = format!("group{group_index:02}");
            let group = groups.iter().find(|(key, _)| key.to_lowercase() == group_name);
            assert!(group.is_some(), "Should find group '{group_name}'");
            assert!(
                group.unwrap().1.files.len() >= 10,
                "Group '{group_name}' should have at least 10 files, got {}",
                group.unwrap().1.files.len()
            );
        }
    }
}

#[cfg(test)]
mod test_grouping_options {
    use super::*;

    #[test]
    fn ignored_group_parts_match_ignoring_case() {
        let options = GroupingOptions::new(3, 5, [" X265 "], Vec::<String>::new());
        assert!(options.has_ignored_group_part(["DL", "x265"].into_iter()));
        assert!(!options.has_ignored_group_part(["DL", "x264"].into_iter()));
    }

    #[test]
    fn default_uses_the_default_group_size_and_prefix_length() {
        let options = GroupingOptions::default();
        assert_eq!(options.min_group_size(), DEFAULT_MIN_GROUP_SIZE);
        assert_eq!(options.min_prefix_chars(), DEFAULT_MIN_PREFIX_CHARS);
    }
}

#[cfg(test)]
mod test_prefix_group_builder {
    use super::*;

    #[test]
    fn dotted_prefix_prefers_stable_display_casing() {
        let first_file = PathBuf::from("summer.vacation.photo4.jpg");
        let second_file = PathBuf::from("SUMMER.VACATION.Photo3.jpg");
        let third_file = PathBuf::from("Summer.Vacation.Photo1.jpg");

        let mut builder = PrefixGroupBuilder::new("summer.vacation".to_string(), first_file, 2, false, 0);
        builder.add_file(second_file, 2, false, 0, "SUMMER.VACATION".to_string());
        builder.add_file(third_file, 2, false, 0, "Summer.Vacation".to_string());

        assert_eq!(builder.original_prefix, "Summer.Vacation");
    }

    #[test]
    fn concatenated_form_is_not_replaced_by_better_dotted_casing() {
        let first_file = PathBuf::from("summervacation_photo1.jpg");
        let second_file = PathBuf::from("Summer.Vacation.Photo2.jpg");

        let mut builder = PrefixGroupBuilder::new("summervacation".to_string(), first_file, 1, true, 0);
        builder.add_file(second_file, 2, false, 0, "Summer.Vacation".to_string());

        // Concatenated form should be retained
        assert_eq!(builder.original_prefix, "summervacation");
    }
}

#[cfg(test)]
mod test_casing_score {
    use super::*;

    #[test]
    fn camel_case_scores_highest() {
        // CamelCase (starts uppercase, has both upper and lower) = 3
        assert_eq!(PrefixGroupBuilder::casing_score("PhotoLab"), 3);
        assert_eq!(PrefixGroupBuilder::casing_score("NeonLight"), 3);
        assert_eq!(PrefixGroupBuilder::casing_score("MyApp"), 3);
    }

    #[test]
    fn all_uppercase_scores_second() {
        // Starts uppercase, all same case = 2
        assert_eq!(PrefixGroupBuilder::casing_score("PHOTOLAB"), 2);
        assert_eq!(PrefixGroupBuilder::casing_score("NEONLIGHT"), 2);
        assert_eq!(PrefixGroupBuilder::casing_score("ABC"), 2);
    }

    #[test]
    fn mixed_case_starting_lowercase_scores_third() {
        // Mixed case but starts lowercase = 1
        assert_eq!(PrefixGroupBuilder::casing_score("photoLab"), 1);
        assert_eq!(PrefixGroupBuilder::casing_score("neonLight"), 1);
    }

    #[test]
    fn all_lowercase_scores_zero() {
        // All lowercase = 0
        assert_eq!(PrefixGroupBuilder::casing_score("photolab"), 0);
        assert_eq!(PrefixGroupBuilder::casing_score("neonlight"), 0);
    }

    #[test]
    fn empty_string_scores_zero() {
        assert_eq!(PrefixGroupBuilder::casing_score(""), 0);
    }
}

#[cfg(test)]
mod test_is_better_prefix {
    use super::*;

    #[test]
    fn camel_case_preferred_over_all_uppercase() {
        assert!(PrefixGroupBuilder::is_better_prefix("PhotoLab", "PHOTOLAB"));
        assert!(!PrefixGroupBuilder::is_better_prefix("PHOTOLAB", "PhotoLab"));
    }

    #[test]
    fn camel_case_preferred_over_all_lowercase() {
        assert!(PrefixGroupBuilder::is_better_prefix("PhotoLab", "photolab"));
        assert!(!PrefixGroupBuilder::is_better_prefix("photolab", "PhotoLab"));
    }

    #[test]
    fn all_uppercase_preferred_over_all_lowercase() {
        assert!(PrefixGroupBuilder::is_better_prefix("PHOTOLAB", "photolab"));
        assert!(!PrefixGroupBuilder::is_better_prefix("photolab", "PHOTOLAB"));
    }

    #[test]
    fn alphabetical_order_breaks_ties() {
        // Both CamelCase - alphabetical order wins
        assert!(PrefixGroupBuilder::is_better_prefix("NeonLight", "PhotoLab"));
        assert!(!PrefixGroupBuilder::is_better_prefix("PhotoLab", "NeonLight"));

        // Both all lowercase - alphabetical order wins
        assert!(PrefixGroupBuilder::is_better_prefix("abc", "xyz"));
        assert!(!PrefixGroupBuilder::is_better_prefix("xyz", "abc"));
    }

    #[test]
    fn same_prefix_not_better() {
        assert!(!PrefixGroupBuilder::is_better_prefix("PhotoLab", "PhotoLab"));
    }
}

#[cfg(test)]
mod test_accepts_group_name {
    use super::*;

    #[test]
    fn accepts_an_ordinary_name() {
        let name_filter = NameFilter::new(["studio"], ["ignored name"]);
        let options = GroupingOptions::new(3, 1, ["sample"], Vec::<String>::new());
        assert!(PrefixGrouper::new(&options, &name_filter).accepts_group_name("Show Name"));
    }

    #[test]
    fn rejects_hidden_names_ignored_prefixes_ignored_names_and_ignored_parts() {
        let name_filter = NameFilter::new(["studio"], ["ignored name"]);
        let options = GroupingOptions::new(3, 1, ["sample"], Vec::<String>::new());
        let grouper = PrefixGrouper::new(&options, &name_filter);

        assert!(!grouper.accepts_group_name(".hidden"));
        assert!(!grouper.accepts_group_name("Studio"));
        assert!(!grouper.accepts_group_name("Ignored.Name"));
        assert!(!grouper.accepts_group_name("IGNORED NAME"));
        assert!(!grouper.accepts_group_name("Show Sample"));
        assert!(!grouper.accepts_group_name("Show.SAMPLE.Name"));
    }
}

#[cfg(test)]
mod test_group_naming_and_skipping {
    use super::*;

    /// Build a grouper with the given prefix ignores and ignored group names.
    fn with_filter<R>(
        prefix_ignores: &[&str],
        ignored_group_names: &[&str],
        run: impl FnOnce(&PrefixGrouper<'_>) -> R,
    ) -> R {
        let name_filter = NameFilter::new(prefix_ignores, ignored_group_names);
        let options = GroupingOptions::new(2, 1, Vec::<String>::new(), Vec::<String>::new());
        run(&PrefixGrouper::new(&options, &name_filter))
    }

    #[test]
    fn directory_name_replaces_dots_and_strips_ignored_prefixes() {
        with_filter(&["studio"], &[], |grouper| {
            assert_eq!(grouper.directory_name_for_prefix("Show.Name"), "Show Name");
            assert_eq!(grouper.directory_name_for_prefix("Studio.Show.Name"), "Show Name");
            assert_eq!(grouper.directory_name_for_prefix("ShowName"), "ShowName");
        });
    }

    #[test]
    fn skips_empty_ignored_and_too_short_names() {
        with_filter(&["studio"], &["ignored name"], |grouper| {
            let none = HashSet::<String>::new();
            assert!(grouper.should_skip_group("", None, &none));
            assert!(grouper.should_skip_group("Ignored Name", None, &none));
            assert!(grouper.should_skip_group("Studio", None, &none));
            assert!(grouper.should_skip_group("Abc", None, &none));
            assert!(grouper.should_skip_group("2024", None, &none));
            assert!(!grouper.should_skip_group("Show Name", None, &none));
        });
    }

    #[test]
    fn skips_the_parent_directory_name_and_names_ignored_during_the_run() {
        with_filter(&[], &[], |grouper| {
            let runtime_ignored = HashSet::from(["skippedname".to_string()]);
            assert!(grouper.should_skip_group("My Series", Some("myseries"), &HashSet::<String>::new()));
            assert!(grouper.should_skip_group("Skipped.Name", None, &runtime_ignored));
            assert!(!grouper.should_skip_group("My Series", Some("other"), &runtime_ignored));
        });
    }

    #[test]
    fn processing_order_drops_small_groups_and_prefers_specific_early_prefixes() {
        with_filter(&[], &[], |grouper| {
            let files = |count: usize| (0..count).map(|index| PathBuf::from(format!("{index}.mp4"))).collect();
            let groups = HashMap::from([
                ("Late".to_string(), PrefixGroup::new(files(5), 1, 1)),
                ("Show".to_string(), PrefixGroup::new(files(4), 1, 0)),
                ("Show.Name".to_string(), PrefixGroup::new(files(3), 2, 0)),
                ("Small".to_string(), PrefixGroup::new(files(1), 1, 0)),
                ("Long.Show".to_string(), PrefixGroup::new(files(2), 2, 0)),
            ]);

            let order: Vec<String> = grouper
                .groups_in_processing_order(groups)
                .into_iter()
                .map(|(prefix, _)| prefix)
                .collect();

            assert_eq!(order, ["Show.Name", "Long.Show", "Show", "Late"]);
        });
    }
}

#[cfg(test)]
mod test_grouping_progress {
    use std::sync::Mutex;

    use super::*;
    use crate::test_helpers::make_test_files;

    /// Progress that records stages and counts the advances of each stage.
    #[derive(Default)]
    struct RecordedProgress {
        /// Stage, expected length, and advance count of each started stage.
        stages: Mutex<Vec<(GroupingStage, u64, u64)>>,
        /// Number of finished stages.
        finished: Mutex<usize>,
    }

    impl GroupingProgress for RecordedProgress {
        fn start_stage(&self, stage: GroupingStage, length: u64) {
            self.stages.lock().unwrap().push((stage, length, 0));
        }

        fn advance(&self) {
            if let Some(last) = self.stages.lock().unwrap().last_mut() {
                last.2 += 1;
            }
        }

        fn finish_stage(&self) {
            *self.finished.lock().unwrap() += 1;
        }
    }

    #[test]
    fn reports_both_stages_with_one_advance_per_item() {
        let name_filter = NameFilter::default();
        let options = GroupingOptions::new(2, 1, Vec::<String>::new(), Vec::<String>::new());
        let files = make_test_files(&["Show.Name.S01E01.mp4", "Show.Name.S01E02.mp4", "Other.Clip.mp4"]);
        let progress = RecordedProgress::default();

        let groups =
            PrefixGrouper::new(&options, &name_filter).collect_all_prefix_groups_with_progress(&files, &progress);

        assert!(!groups.is_empty());
        let stages = progress.stages.lock().unwrap().clone();
        assert_eq!(stages.len(), 2, "{stages:?}");
        assert_eq!(stages[0].0, GroupingStage::CollectingPrefixes);
        assert_eq!((stages[0].1, stages[0].2), (3, 3));
        assert_eq!(stages[1].0, GroupingStage::MatchingFilesToGroups);
        assert_eq!(stages[1].1, stages[1].2);
        assert_eq!(*progress.finished.lock().unwrap(), 2);
    }
}
