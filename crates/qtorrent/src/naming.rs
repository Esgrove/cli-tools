//! Name suggestions for added torrents.
//!
//! Builds the suggested output name from the torrent file name and the internal torrent name,
//! cleans custom names, and makes the two suggestions comparable by sharing their date and extension.

use cli_tools_common::date::RE_CORRECT_DATE_FORMAT;
use cli_tools_dots::{DotFormat, DotRenameConfig};

use crate::utils;
use crate::utils::TorrentInfo;

/// Options for building torrent output names.
#[derive(Debug, Clone, Default)]
pub struct NamingOptions {
    /// Substrings to remove from the torrent file name when generating the suggested name.
    pub remove_from_name: Vec<String>,
    /// Apply dots formatting to custom names entered by the user.
    pub format_custom_name: bool,
    /// If the torrent file name contains any of these strings, use the internal name instead.
    pub ignore_torrent_names: Vec<String>,
}

/// Suggests and cleans output names for torrents, with optional dots formatting.
#[derive(Debug, Default)]
pub struct NameSuggester {
    /// Name building options.
    pub options: NamingOptions,
    /// Dots formatting config, `None` to leave names unformatted.
    dot_rename: Option<DotRenameConfig>,
}

impl NameSuggester {
    /// Create a name suggester.
    ///
    /// Names are formatted with dots formatting when a dots config is given.
    #[must_use]
    pub const fn new(options: NamingOptions, dot_rename: Option<DotRenameConfig>) -> Self {
        Self { options, dot_rename }
    }

    /// Dots formatter for the configured dots config, if dots formatting is enabled.
    #[must_use]
    pub const fn dot_formatter(&self) -> Option<DotFormat<'_>> {
        if let Some(dot_rename_config) = &self.dot_rename {
            Some(DotFormat::new(dot_rename_config))
        } else {
            None
        }
    }

    /// Get the suggested name with `remove_from_name` substrings removed and dots formatting applied.
    #[must_use]
    pub fn clean_suggested_name(&self, info: &TorrentInfo) -> String {
        let mut name = info.suggested_name_raw(&self.options.ignore_torrent_names).into_owned();

        for substring in &self.options.remove_from_name {
            name = name.replace(substring, "");
        }

        // Trim any leading or trailing whitespace left over from the removal
        name = name.trim().to_string();

        if let Some(dot_rename) = self.dot_formatter() {
            name = utils::format_torrent_name(&dot_rename, &name, info.effective_is_multi_file);
        }

        name
    }

    /// Format the internal torrent name with dots formatting applied.
    #[must_use]
    pub fn clean_internal_name(&self, info: &TorrentInfo) -> Option<String> {
        let internal_name = info.torrent.name()?;

        let name = self.dot_formatter().map_or_else(
            || internal_name.to_string(),
            |dot_rename| utils::format_torrent_name(&dot_rename, internal_name, info.effective_is_multi_file),
        );

        Some(name)
    }

    /// Sanitize a custom name entered by the user and apply dots formatting when enabled.
    #[must_use]
    pub fn format_custom_name(&self, name: &str, is_multi_file: bool) -> String {
        let name = utils::sanitize_custom_name(name);
        if self.options.format_custom_name
            && let Some(dot_rename) = self.dot_formatter()
        {
            return utils::format_torrent_name(&dot_rename, &name, is_multi_file);
        }
        name
    }

    /// Build the output name from a custom name typed for a new torrent.
    ///
    /// For a single file, the extension of the suggested name is restored when the custom name has none.
    #[must_use]
    pub fn custom_output_name(&self, input: &str, info: &TorrentInfo, suggested: &str) -> String {
        let sanitized = utils::sanitize_custom_name(input);
        let custom = if info.effective_is_multi_file {
            sanitized
        } else {
            utils::restore_file_extension(&sanitized, suggested)
        };
        self.format_custom_name(&custom, info.effective_is_multi_file)
    }

    /// Normalize two rename options by ensuring both have the same date and extension if applicable.
    ///
    /// If one option has a file extension and the other doesn't, the extension is added.
    /// If one option has a date (yyyy.mm.dd format) and the other doesn't, the date is added.
    ///
    /// Extensions are checked first to avoid date parts (like `.15`) being mistaken for extensions.
    #[must_use]
    pub fn normalize_rename_options(suggested: &str, internal: Option<&str>) -> (String, Option<String>) {
        let Some(internal) = internal else {
            return (suggested.to_string(), None);
        };

        let mut normalized_suggested = suggested.to_string();
        let mut normalized_internal = internal.to_string();

        // Extract extensions from both options before adding dates,
        // so date parts like ".15" are not mistaken for extensions
        let suggested_extension = utils::extract_file_extension(&normalized_suggested);
        let internal_extension = utils::extract_file_extension(&normalized_internal);

        match (&suggested_extension, &internal_extension) {
            (Some(extension), None) => {
                normalized_internal = format!("{normalized_internal}.{extension}");
            }
            (None, Some(extension)) => {
                normalized_suggested = format!("{normalized_suggested}.{extension}");
            }
            _ => {}
        }

        let suggested_date = RE_CORRECT_DATE_FORMAT
            .find(&normalized_suggested)
            .map(|found| found.as_str().to_string());
        let internal_date = RE_CORRECT_DATE_FORMAT
            .find(&normalized_internal)
            .map(|found| found.as_str().to_string());

        // If one has a date and the other doesn't, insert the date before the extension of the one missing it
        match (&suggested_date, &internal_date) {
            (Some(date), None) => {
                normalized_internal = utils::insert_date_before_extension(&normalized_internal, date);
            }
            (None, Some(date)) => {
                normalized_suggested = utils::insert_date_before_extension(&normalized_suggested, date);
            }
            _ => {}
        }

        (normalized_suggested, Some(normalized_internal))
    }
}

#[cfg(test)]
mod test_normalize_rename_options {
    use super::*;

    #[test]
    fn both_same_returns_equal() {
        let (suggested, internal) =
            NameSuggester::normalize_rename_options("Name.2024.01.15.mp4", Some("Name.2024.01.15.mp4"));
        assert_eq!(suggested, "Name.2024.01.15.mp4");
        assert_eq!(internal.as_deref(), Some("Name.2024.01.15.mp4"));
    }

    #[test]
    fn no_internal_returns_suggested_only() {
        let (suggested, internal) = NameSuggester::normalize_rename_options("Name.2024.01.15.mp4", None);
        assert_eq!(suggested, "Name.2024.01.15.mp4");
        assert!(internal.is_none());
    }

    #[test]
    fn date_added_to_internal_when_missing() {
        let (suggested, internal) = NameSuggester::normalize_rename_options("Name.2024.01.15.mp4", Some("Name.mp4"));
        assert_eq!(suggested, "Name.2024.01.15.mp4");
        assert_eq!(internal.as_deref(), Some("Name.2024.01.15.mp4"));
    }

    #[test]
    fn date_added_to_suggested_when_missing() {
        let (suggested, internal) = NameSuggester::normalize_rename_options("Name.mp4", Some("Name.2024.01.15.mp4"));
        assert_eq!(suggested, "Name.2024.01.15.mp4");
        assert_eq!(internal.as_deref(), Some("Name.2024.01.15.mp4"));
    }

    #[test]
    fn extension_added_to_internal_when_missing() {
        let (suggested, internal) = NameSuggester::normalize_rename_options("Name.mp4", Some("Name"));
        assert_eq!(suggested, "Name.mp4");
        assert_eq!(internal.as_deref(), Some("Name.mp4"));
    }

    #[test]
    fn extension_added_to_suggested_when_missing() {
        let (suggested, internal) = NameSuggester::normalize_rename_options("Name", Some("Name.mp4"));
        assert_eq!(suggested, "Name.mp4");
        assert_eq!(internal.as_deref(), Some("Name.mp4"));
    }

    #[test]
    fn both_date_and_extension_added() {
        let (suggested, internal) = NameSuggester::normalize_rename_options("Name.2024.01.15.mp4", Some("Name"));
        assert_eq!(suggested, "Name.2024.01.15.mp4");
        assert_eq!(internal.as_deref(), Some("Name.2024.01.15.mp4"));
    }

    #[test]
    fn different_dates_remain_different() {
        let (suggested, internal) =
            NameSuggester::normalize_rename_options("Name.2024.01.15.mp4", Some("Name.2023.12.25.mp4"));
        assert_eq!(suggested, "Name.2024.01.15.mp4");
        assert_eq!(internal.as_deref(), Some("Name.2023.12.25.mp4"));
    }

    #[test]
    fn different_extensions_remain_different() {
        let (suggested, internal) = NameSuggester::normalize_rename_options("Name.mp4", Some("Name.mkv"));
        assert_eq!(suggested, "Name.mp4");
        assert_eq!(internal.as_deref(), Some("Name.mkv"));
    }

    #[test]
    fn directory_names_without_extension() {
        let (suggested, internal) = NameSuggester::normalize_rename_options("Show Name 2024.01.15", Some("Show Name"));
        assert_eq!(suggested, "Show Name 2024.01.15");
        assert_eq!(internal.as_deref(), Some("Show Name.2024.01.15"));
    }

    #[test]
    fn extension_added_when_names_differ() {
        // When one has extension and names are different, extension should be added to the other
        let (suggested, internal) =
            NameSuggester::normalize_rename_options("Different.Name.2024.01.15.mp4", Some("Other.Name"));
        assert_eq!(suggested, "Different.Name.2024.01.15.mp4");
        assert_eq!(internal.as_deref(), Some("Other.Name.2024.01.15.mp4"));
    }

    #[test]
    fn non_extension_dots_not_treated_as_extension() {
        // "Show.Name" should not have "Name" treated as an extension
        let (suggested, internal) = NameSuggester::normalize_rename_options("Show.Name.mp4", Some("Show.Name"));
        assert_eq!(suggested, "Show.Name.mp4");
        assert_eq!(internal.as_deref(), Some("Show.Name.mp4"));
    }

    #[test]
    fn both_without_extension_different_names() {
        // Neither has a real extension, names differ - no extension added
        let (suggested, internal) = NameSuggester::normalize_rename_options("Show.Name.One", Some("Show.Name.Two"));
        assert_eq!(suggested, "Show.Name.One");
        assert_eq!(internal.as_deref(), Some("Show.Name.Two"));
    }
}

#[cfg(test)]
mod test_name_suggester {
    use crate::utils::test_torrent_info_helpers::create_torrent_info;

    use super::*;

    #[test]
    fn removes_part_suffix_only_when_torrent_name_formatting_is_enabled() {
        let mut info = create_torrent_info(
            "torrents/Example Video Part 1.torrent",
            Some("Example Video Part 1.mp4"),
            None,
            false,
            false,
            None,
        );
        let mut names = NameSuggester::default();

        assert_eq!(names.clean_suggested_name(&info), "Example Video Part 1.mp4");
        assert_eq!(
            names.clean_internal_name(&info).as_deref(),
            Some("Example Video Part 1.mp4")
        );

        names.dot_rename = Some(DotRenameConfig::default());
        assert_eq!(names.clean_suggested_name(&info), "Example.Video.mp4");
        assert_eq!(names.clean_internal_name(&info).as_deref(), Some("Example.Video.mp4"));
        assert_eq!(
            names.format_custom_name("Example Video Part 1.mp4", false),
            "Example Video Part 1.mp4"
        );
        names.options.format_custom_name = true;
        assert_eq!(
            names.format_custom_name("Example Video Part 1.mp4", false),
            "Example.Video.mp4"
        );

        info.effective_is_multi_file = true;
        info.torrent.info.name = Some("Example Video Part 1".to_string());
        assert_eq!(names.clean_suggested_name(&info), "Example Video");
        assert_eq!(names.clean_internal_name(&info).as_deref(), Some("Example Video"));
        assert_eq!(names.format_custom_name("Example Video Part 1", true), "Example Video");
    }

    #[test]
    fn removes_configured_substrings_from_the_suggestion() {
        let info = create_torrent_info("Show.Name-RELEASE.torrent", Some("show.mkv"), None, false, false, None);
        let names = NameSuggester::new(
            NamingOptions {
                remove_from_name: vec!["-RELEASE".to_string()],
                ..NamingOptions::default()
            },
            None,
        );

        assert_eq!(names.clean_suggested_name(&info), "Show.Name.mkv");
    }

    #[test]
    fn custom_output_name_restores_the_single_file_extension() {
        let info = create_torrent_info("Show.torrent", Some("Show.mkv"), None, false, false, None);
        let names = NameSuggester::default();

        assert_eq!(names.custom_output_name("New Name", &info, "Show.mkv"), "New Name.mkv");
        assert_eq!(
            names.custom_output_name("New Name.mp4", &info, "Show.mkv"),
            "New Name.mp4"
        );
    }
}
