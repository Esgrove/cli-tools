//! Sorted index of the part combinations of every file, for prefix lookups in `dirmove`.
//!
//! `find_prefix_candidates` asks, for every candidate prefix of every file, how many files match it.
//! Scanning all files for each candidate makes grouping quadratic in the number of files,
//! so this index sorts the lowercased single, two part, and three part combinations of all files once.
//! Every combination that starts with a prefix then sits in one contiguous range found by binary search.

use super::types::{FileInfo, FilteredParts};

/// Part combinations of a set of files, sorted by their lowercased text.
pub struct PrefixIndex<'a> {
    /// Every combination of every file, sorted by `lower`.
    entries: Vec<IndexEntry<'a>>,
}

/// One single, two part, or three part combination of one file.
struct IndexEntry<'a> {
    /// Lowercased combination, the sort key.
    lower: &'a str,
    /// The same combination in its original casing, used for the word boundary check.
    original: &'a str,
    /// Index of the file in the slice the index was built from.
    file_index: usize,
}

impl<'a> PrefixIndex<'a> {
    /// Index the part combinations of the files.
    #[must_use]
    pub fn new(files: &'a [FileInfo<'_>]) -> Self {
        let mut entries = Vec::new();
        for (file_index, info) in files.iter().enumerate() {
            let parts = &info.filtered_parts;
            let combinations = parts
                .parts_lower
                .iter()
                .zip(&parts.parts_original)
                .chain(parts.two_parts_lower.iter().zip(&parts.two_parts_original))
                .chain(parts.three_parts_lower.iter().zip(&parts.three_parts_original));
            entries.extend(combinations.map(|(lower, original)| IndexEntry {
                lower,
                original,
                file_index,
            }));
        }
        entries.sort_unstable_by(|first, second| first.lower.cmp(second.lower));
        Self { entries }
    }

    /// Fill `matches` with the sorted, unique indices of the files matching the normalized target.
    ///
    /// A file matches exactly when [`FilteredParts::prefix_matches_normalized`] accepts it:
    /// one of its combinations equals the target,
    /// or starts with it at a word boundary of the original casing.
    pub fn matching_files(&self, target: &str, matches: &mut Vec<usize>) {
        matches.clear();
        if target.is_empty() {
            return;
        }
        let start = self.entries.partition_point(|entry| entry.lower < target);
        let candidates = self
            .entries
            .get(start..)
            .unwrap_or_default()
            .iter()
            .take_while(|entry| entry.lower.starts_with(target));
        for entry in candidates {
            if entry.lower.len() == target.len() || FilteredParts::has_word_boundary_at(entry.original, target.len()) {
                matches.push(entry.file_index);
            }
        }
        matches.sort_unstable();
        matches.dedup();
    }
}

#[cfg(test)]
mod test_prefix_index {
    use std::path::PathBuf;

    use super::*;
    use crate::dir_move::filter_numeric_resolution_and_glue_parts;

    /// Build the file infos the way `dirmove` does, filtering each name first.
    fn files(names: &[&str]) -> Vec<FileInfo<'static>> {
        names
            .iter()
            .map(|name| {
                let filtered = filter_numeric_resolution_and_glue_parts(name);
                FileInfo::new(PathBuf::from(*name), (*name).to_string(), filtered)
            })
            .collect()
    }

    #[test]
    fn matches_the_same_files_as_a_full_scan() {
        let files = files(&[
            "PhotoLab.Image.One.jpg",
            "Photo.Lab.Image.Two.jpg",
            "photolabs.extra.jpg",
            "PhotoLabTV.Show.mp4",
            "Jane.Doe.S01E01.720p.mp4",
            "JaneDoe.Special.mp4",
            "Janet.Other.mp4",
            "Ärtist.Nämé.Song.mp3",
            "ärtistnämé.live.mp3",
            "Show.2024.S01E01.mkv",
            "Mixed.CASE.Name.mkv",
        ]);
        let index = PrefixIndex::new(&files);
        let mut matches = Vec::new();
        let targets = [
            "",
            "p",
            "photo",
            "photolab",
            "photolabs",
            "photolabimage",
            "jane",
            "janed",
            "janedoe",
            "janet",
            "ärtist",
            "ärtistnämé",
            "show",
            "shows01e01",
            "mixedcase",
            "mixedcasename",
            "missing",
            "mp4",
        ];
        for target in targets {
            index.matching_files(target, &mut matches);
            let expected: Vec<usize> = files
                .iter()
                .enumerate()
                .filter(|(_, file)| file.filtered_parts.prefix_matches_normalized(target))
                .map(|(position, _)| position)
                .collect();
            assert_eq!(matches, expected, "target {target:?}");
        }
    }

    #[test]
    fn an_empty_index_matches_nothing() {
        let index = PrefixIndex::new(&[]);
        let mut matches = vec![1, 2];
        index.matching_files("anything", &mut matches);
        assert!(matches.is_empty());
    }
}
