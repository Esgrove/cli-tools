//! Tag rules for torrents.
//!
//! Resolves the tags of a torrent from its file name prefix or the directory it is in,
//! falling back to the default tags.

use std::path::Path;

/// A tag overwrite rule:
/// if a torrent filename starts with the prefix,
/// use the associated tag value.
#[derive(Debug, Clone)]
pub struct TagOverwrite {
    /// Tag value to use when the prefix matches.
    pub tag: String,
    /// Lowercase prefix for case-insensitive matching.
    lowercase_prefix: String,
}

/// A path tag overwrite rule:
/// if a torrent file is in a matching directory,
/// use the associated tag value.
#[derive(Debug, Clone)]
pub struct TagPathOverwrite {
    /// Tag value to use when the path matches.
    pub tag: String,
    /// Lowercase normalized path for sorting and diagnostics.
    lowercase_path: String,
    /// Lowercase path components for case-insensitive matching.
    lowercase_components: Vec<String>,
}

/// Default tags and the overwrite rules that replace them for matching torrents.
#[derive(Debug, Clone, Default)]
pub struct TagRules {
    /// Tags used when no overwrite rule matches.
    pub default_tags: Option<String>,
    /// Prefix rules, longest prefix first.
    prefixes: Vec<TagOverwrite>,
    /// Path rules, most specific path first.
    paths: Vec<TagPathOverwrite>,
}

impl TagOverwrite {
    /// Create a rule that tags torrents whose file name starts with the prefix, ignoring case.
    #[must_use]
    pub fn new(prefix: &str, tag: String) -> Self {
        Self {
            tag,
            lowercase_prefix: prefix.to_lowercase(),
        }
    }

    /// The prefix in lowercase.
    #[must_use]
    pub fn lowercase_prefix(&self) -> &str {
        &self.lowercase_prefix
    }
}

impl TagPathOverwrite {
    /// Create a rule that tags torrents in a directory matching the path, ignoring case.
    ///
    /// Both Windows and Unix separators are accepted.
    /// Returns `None` when the path has no components.
    #[must_use]
    pub fn new(path: &str, tag: String) -> Option<Self> {
        let lowercase_components = lowercase_path_components(path);
        (!lowercase_components.is_empty()).then(|| Self {
            tag,
            lowercase_path: lowercase_components.join("/"),
            lowercase_components,
        })
    }

    /// The path in lowercase with `/` separators.
    #[must_use]
    pub fn lowercase_path(&self) -> &str {
        &self.lowercase_path
    }
}

impl TagRules {
    /// Create tag rules from the default tags and the overwrite rules.
    ///
    /// Prefix rules are sorted longest first for longest match first semantics.
    /// Path rules with more components are checked first, then longer paths.
    #[must_use]
    pub fn new(
        default_tags: Option<String>,
        mut prefixes: Vec<TagOverwrite>,
        mut paths: Vec<TagPathOverwrite>,
    ) -> Self {
        prefixes.sort_by_key(|entry| std::cmp::Reverse(entry.lowercase_prefix.chars().count()));
        paths.sort_by_key(|entry| {
            std::cmp::Reverse((entry.lowercase_components.len(), entry.lowercase_path.chars().count()))
        });
        Self {
            default_tags,
            prefixes,
            paths,
        }
    }

    /// Create tag rules from `[prefix, tag]` and `[path, tag]` pairs.
    ///
    /// Path pairs without any path components are ignored.
    #[must_use]
    pub fn from_pairs(default_tags: Option<String>, prefixes: Vec<[String; 2]>, paths: Vec<[String; 2]>) -> Self {
        let prefixes = prefixes
            .into_iter()
            .map(|[prefix, tag]| TagOverwrite::new(&prefix, tag))
            .collect();
        let paths = paths
            .into_iter()
            .filter_map(|[path, tag]| TagPathOverwrite::new(&path, tag))
            .collect();
        Self::new(default_tags, prefixes, paths)
    }

    /// Prefix rules, longest prefix first.
    #[must_use]
    pub fn prefixes(&self) -> &[TagOverwrite] {
        &self.prefixes
    }

    /// Path rules, most specific path first.
    #[must_use]
    pub fn paths(&self) -> &[TagPathOverwrite] {
        &self.paths
    }

    /// Resolve tags for a given torrent file path.
    ///
    /// If the torrent filename (without extension) starts with one of the prefixes (case-insensitive),
    /// returns the associated tag value.
    /// Prefix rules take precedence over path rules.
    /// If no prefix matches, a path rule can match the torrent file's directory path.
    /// Otherwise, returns the default tags.
    #[must_use]
    pub fn resolve(&self, torrent_path: &Path) -> Option<String> {
        if !self.prefixes.is_empty() {
            let filename = lowercase_torrent_file_stem(torrent_path);

            for entry in &self.prefixes {
                if filename.starts_with(&entry.lowercase_prefix) {
                    return Some(entry.tag.clone());
                }
            }
        }

        if !self.paths.is_empty() {
            let directory_components = lowercase_directory_components(torrent_path);
            let mut best_match: Option<(&TagPathOverwrite, (usize, usize, usize))> = None;

            for entry in &self.paths {
                let Some(score) = path_components_match_score(&directory_components, &entry.lowercase_components)
                else {
                    continue;
                };

                if best_match.is_none_or(|(_, best_score)| score > best_score) {
                    best_match = Some((entry, score));
                }
            }

            if let Some((entry, _score)) = best_match {
                return Some(entry.tag.clone());
            }
        }

        self.default_tags.clone()
    }
}

/// Return the torrent file stem in lowercase.
fn lowercase_torrent_file_stem(torrent_path: &Path) -> String {
    let path_text = torrent_path.to_string_lossy();
    let filename = path_text.rsplit(['/', '\\']).next().unwrap_or_default();
    let lowercase_filename = filename.to_lowercase();

    lowercase_filename
        .strip_suffix(".torrent")
        .unwrap_or(&lowercase_filename)
        .to_string()
}

/// Return lowercase directory components for a torrent file path.
fn lowercase_directory_components(torrent_path: &Path) -> Vec<String> {
    let path_text = torrent_path.to_string_lossy();
    let Some((directory, _filename)) = path_text.rsplit_once(['/', '\\']) else {
        return Vec::new();
    };

    lowercase_path_components(directory)
}

/// Return lowercase path components after normalizing Windows and Unix separators.
fn lowercase_path_components(path: &str) -> Vec<String> {
    path.trim()
        .split(['/', '\\'])
        .map(str::trim)
        .filter(|component| !component.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Return a match score when all rule components appear as a contiguous directory segment.
fn path_components_match_score(
    path_components: &[String],
    rule_components: &[String],
) -> Option<(usize, usize, usize)> {
    if rule_components.is_empty() {
        return None;
    }

    let mut best_score: Option<(usize, usize, usize)> = None;

    for (index, components) in path_components.windows(rule_components.len()).enumerate() {
        if components != rule_components {
            continue;
        }

        let end_index = index + rule_components.len();
        let character_count = rule_components.iter().map(|component| component.chars().count()).sum();
        let score = (end_index, rule_components.len(), character_count);
        best_score = Some(best_score.map_or(score, |current_score| current_score.max(score)));
    }

    best_score
}

#[cfg(test)]
mod test_tag_rule_construction {
    use super::*;

    fn pairs(entries: &[(&str, &str)]) -> Vec<[String; 2]> {
        entries
            .iter()
            .map(|(first, second)| [(*first).to_string(), (*second).to_string()])
            .collect()
    }

    #[test]
    fn prefixes_are_lowercased_and_sorted_longest_first() {
        let rules = TagRules::from_pairs(
            None,
            pairs(&[("Short", "shorttag"), ("LongPrefix", "longtag")]),
            Vec::new(),
        );
        let prefixes: Vec<(&str, &str)> = rules
            .prefixes()
            .iter()
            .map(|entry| (entry.lowercase_prefix(), entry.tag.as_str()))
            .collect();
        assert_eq!(prefixes, vec![("longprefix", "longtag"), ("short", "shorttag")]);
    }

    #[test]
    fn paths_are_normalized_and_sorted_by_specificity() {
        let rules = TagRules::from_pairs(
            None,
            Vec::new(),
            pairs(&[
                (r"Media\Movies\priority", "priority-tag"),
                (r"C:\Archive\Media\Movies", "movie-tag"),
                ("Series", "series-tag"),
                (" / ", "ignored"),
            ]),
        );
        let paths: Vec<(&str, &str)> = rules
            .paths()
            .iter()
            .map(|entry| (entry.lowercase_path(), entry.tag.as_str()))
            .collect();
        assert_eq!(
            paths,
            vec![
                ("c:/archive/media/movies", "movie-tag"),
                ("media/movies/priority", "priority-tag"),
                ("series", "series-tag"),
            ]
        );
    }

    #[test]
    fn prefixes_sorted_by_length_descending() {
        let rules = TagRules::from_pairs(
            None,
            pairs(&[("a", "tag-a"), ("ccc", "tag-c"), ("bb", "tag-b")]),
            Vec::new(),
        );
        let prefixes: Vec<&str> = rules.prefixes().iter().map(TagOverwrite::lowercase_prefix).collect();
        assert_eq!(prefixes, vec!["ccc", "bb", "a"]);
    }
}

#[cfg(test)]
mod test_resolve_tags {
    use std::path::Path;

    use super::*;

    /// Helper to create tag rules with given default tags and prefix-tag pairs.
    fn make_rules(tags: Option<&str>, prefixes: Vec<(&str, &str)>) -> TagRules {
        make_rules_with_paths(tags, prefixes, Vec::new())
    }

    /// Helper to create tag rules with given default tags, prefix rules, and path rules.
    fn make_rules_with_paths(tags: Option<&str>, prefixes: Vec<(&str, &str)>, paths: Vec<(&str, &str)>) -> TagRules {
        TagRules::new(
            tags.map(String::from),
            prefixes
                .into_iter()
                .map(|(prefix, tag)| TagOverwrite::new(prefix, tag.to_string()))
                .collect(),
            paths
                .into_iter()
                .filter_map(|(path, tag)| TagPathOverwrite::new(path, tag.to_string()))
                .collect(),
        )
    }

    #[test]
    fn returns_default_tags_when_no_prefixes_configured() {
        let rules = make_rules(Some("default-tag"), vec![]);
        let result = rules.resolve(Path::new("something.torrent"));
        assert_eq!(result, Some("default-tag".to_string()));
    }

    #[test]
    fn returns_none_when_no_tags_and_no_prefix_match() {
        let rules = make_rules(None, vec![("prefix", "mytag")]);
        let result = rules.resolve(Path::new("other.torrent"));
        assert_eq!(result, None);
    }

    #[test]
    fn returns_default_tags_when_no_prefix_matches() {
        let rules = make_rules(Some("default-tag"), vec![("prefix", "mytag")]);
        let result = rules.resolve(Path::new("other.torrent"));
        assert_eq!(result, Some("default-tag".to_string()));
    }

    #[test]
    fn returns_associated_tag_on_prefix_match() {
        let rules = make_rules(Some("default-tag"), vec![("MyPrefix", "custom-tag")]);
        let result = rules.resolve(Path::new("MyPrefix.Something.2024.torrent"));
        assert_eq!(result, Some("custom-tag".to_string()));
    }

    #[test]
    fn matches_case_insensitively() {
        let rules = make_rules(Some("default-tag"), vec![("MyPrefix", "custom-tag")]);
        let result = rules.resolve(Path::new("myprefix.something.torrent"));
        assert_eq!(result, Some("custom-tag".to_string()));
    }

    #[test]
    fn longer_prefix_matches_first() {
        let rules = make_rules(
            Some("default-tag"),
            vec![("name", "short-tag"), ("nameprefix", "long-tag")],
        );
        let result = rules.resolve(Path::new("nameprefix.something.torrent"));
        assert_eq!(result, Some("long-tag".to_string()));
    }

    #[test]
    fn shorter_prefix_matches_when_longer_does_not() {
        let rules = make_rules(
            Some("default-tag"),
            vec![("name", "short-tag"), ("nameprefix", "long-tag")],
        );
        let result = rules.resolve(Path::new("name.other.torrent"));
        assert_eq!(result, Some("short-tag".to_string()));
    }

    #[test]
    fn uses_file_stem_without_torrent_extension() {
        let rules = make_rules(Some("default-tag"), vec![("test", "testtag")]);
        let result = rules.resolve(Path::new("/some/path/test.file.torrent"));
        assert_eq!(result, Some("testtag".to_string()));
    }

    #[test]
    fn handles_path_with_directories() {
        let rules = make_rules(Some("default-tag"), vec![("MyPrefix", "custom-tag")]);
        let result = rules.resolve(Path::new("/downloads/torrents/MyPrefix.Show.torrent"));
        assert_eq!(result, Some("custom-tag".to_string()));
    }

    #[test]
    fn returns_none_when_no_tags_and_no_prefixes() {
        let rules = make_rules(None, vec![]);
        let result = rules.resolve(Path::new("something.torrent"));
        assert_eq!(result, None);
    }

    #[test]
    fn prefix_match_takes_precedence_over_path_match() {
        let rules = make_rules_with_paths(
            Some("default-tag"),
            vec![("Special", "prefix-tag")],
            vec![(r"Media\Movies", "path-tag")],
        );
        let result = rules.resolve(Path::new(r"C:\Archive\Media\Movies\Special.Show.torrent"));
        assert_eq!(result, Some("prefix-tag".to_string()));
    }

    #[test]
    fn path_match_overrides_default_tags() {
        let rules = make_rules_with_paths(Some("default-tag"), vec![], vec![(r"Media\Movies", "movie-tag")]);
        let result = rules.resolve(Path::new(r"C:\Archive\Media\Movies\Show.torrent"));
        assert_eq!(result, Some("movie-tag".to_string()));
    }

    #[test]
    fn path_match_handles_partial_path_from_right() {
        let rules = make_rules_with_paths(
            Some("default-tag"),
            vec![],
            vec![(r"Media\Movies\priority", "priority-tag")],
        );
        let result = rules.resolve(Path::new(r"C:\Archive\Media\Movies\priority\Show.torrent"));
        assert_eq!(result, Some("priority-tag".to_string()));
    }

    #[test]
    fn path_match_handles_single_directory_name() {
        let rules = make_rules_with_paths(Some("default-tag"), vec![], vec![("Series", "series-tag")]);
        let result = rules.resolve(Path::new(r"C:\Archive\Media\Series\Show.torrent"));
        assert_eq!(result, Some("series-tag".to_string()));
    }

    #[test]
    fn deepest_path_match_wins() {
        let rules = make_rules_with_paths(
            Some("default-tag"),
            vec![],
            vec![
                (r"C:\Archive\Media\Movies", "movie-tag"),
                (r"Media\Movies\priority", "priority-tag"),
            ],
        );
        let result = rules.resolve(Path::new(r"C:\Archive\Media\Movies\priority\Show.torrent"));
        assert_eq!(result, Some("priority-tag".to_string()));
    }

    #[test]
    fn longer_path_match_wins_when_matches_end_at_same_directory() {
        let rules = make_rules_with_paths(
            Some("default-tag"),
            vec![],
            vec![("Movies", "short"), (r"Media\Movies", "long")],
        );
        let result = rules.resolve(Path::new(r"C:\Archive\Media\Movies\Show.torrent"));
        assert_eq!(result, Some("long".to_string()));
    }

    #[test]
    fn path_match_is_case_insensitive() {
        let rules = make_rules_with_paths(Some("default-tag"), vec![], vec![(r"media\movies", "movie-tag")]);
        let result = rules.resolve(Path::new(r"C:\Archive\Media\Movies\Show.torrent"));
        assert_eq!(result, Some("movie-tag".to_string()));
    }

    #[test]
    fn returns_default_tags_when_no_path_matches() {
        let rules = make_rules_with_paths(Some("default-tag"), vec![], vec![("Series", "series-tag")]);
        let result = rules.resolve(Path::new(r"C:\Archive\Media\Movies\Show.torrent"));
        assert_eq!(result, Some("default-tag".to_string()));
    }
}
