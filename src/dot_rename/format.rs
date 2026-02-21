use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use unicode_segmentation::UnicodeSegmentation;

use crate::date::Date;
use crate::date::{CURRENT_YEAR, RE_CORRECT_DATE_FORMAT, RE_YEAR};
use crate::dot_rename::DotRenameConfig;

static RE_BRACKETS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\[({\]})]+").expect("Failed to create regex pattern for brackets"));

static RE_WHITESPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+").expect("Failed to compile whitespace regex"));

/// Matches two or more consecutive dots.
static RE_CONSECUTIVE_DOTS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.{2,}").expect("Failed to create regex pattern for consecutive dots"));

static RE_EXCLAMATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!+").expect("Failed to compile exclamation regex"));

static RE_DOTCOM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(\.com|\.net)\b").expect("Failed to compile .com regex"));

static RE_IDENTIFIER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9]{9,20}").expect("Failed to compile id regex"));

static RE_RESOLUTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{3,4}x\d{3,4}\b").expect("Failed to compile resolution regex"));

static RE_LEADING_DIGITS: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"^\d{5,}\b").expect("Invalid leading digits regex"));

static RE_WRITTEN_DATE_MDY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?P<month>Jan(?:uary)?|Feb(?:ruary)?|Mar(?:ch)?|Apr(?:il)?|May|Jun(?:e)?|Jul(?:y)?|Aug(?:ust)?|Sep(?:tember)?|Oct(?:ober)?|Nov(?:ember)?|Dec(?:ember)?)\.(?P<day>\d{1,2})\.(?P<year>\d{4})\b",
    )
        .expect("Failed to compile MDY written date regex")
});

static RE_WRITTEN_DATE_DMY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?P<day>\d{1,2})\.(?P<month>Jan(?:uary)?|Feb(?:ruary)?|Mar(?:ch)?|Apr(?:il)?|May|Jun(?:e)?|Jul(?:y)?|Aug(?:ust)?|Sep(?:tember)?|Oct(?:ober)?|Nov(?:ember)?|Dec(?:ember)?)\.(?P<year>\d{4})\b",
    )
        .expect("Failed to compile DMY written date regex")
});

static WRITTEN_MONTHS_MAP: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    [
        ("jan", "01"),
        ("january", "01"),
        ("feb", "02"),
        ("february", "02"),
        ("mar", "03"),
        ("march", "03"),
        ("apr", "04"),
        ("april", "04"),
        ("may", "05"),
        ("jun", "06"),
        ("june", "06"),
        ("jul", "07"),
        ("july", "07"),
        ("aug", "08"),
        ("august", "08"),
        ("sep", "09"),
        ("september", "09"),
        ("oct", "10"),
        ("october", "10"),
        ("nov", "11"),
        ("november", "11"),
        ("dec", "12"),
        ("december", "12"),
    ]
    .into_iter()
    .collect()
});

static REPLACE: [(&str, &str); 27] = [
    (" ", "."),
    (" - ", " "),
    (", ", " "),
    ("_", "."),
    ("-", "."),
    ("–", "."),
    (".&.", ".and."),
    ("*", "."),
    ("~", "."),
    ("¡", "."),
    ("#", "."),
    ("$", "."),
    (";", "."),
    ("@", "."),
    ("+", "."),
    ("=", "."),
    (",.", "."),
    (",", "."),
    ("-=-", "."),
    (".-.", "."),
    (".rq", ""),
    ("www.", ""),
    ("^", ""),
    ("｜", ""),
    ("`", "'"),
    ("'", "'"),
    ("\"", "'"),
];

const RESOLUTIONS: [&str; 7] = ["2160", "1440", "1080", "720", "540", "480", "360"];

/// Formatter for applying dot-style formatting to file and directory names.
#[derive(Debug)]
pub struct DotFormat<'a> {
    config: &'a DotRenameConfig,
}

impl<'a> DotFormat<'a> {
    /// Create a new formatter with the given config reference.
    #[must_use]
    pub const fn new(config: &'a DotRenameConfig) -> Self {
        Self { config }
    }
}

impl DotFormat<'_> {
    /// Format a file or directory name.
    ///
    /// Filenames should be given without the file extension.
    /// This is the main entry point for name formatting and applies all configured
    /// transformations including replacements, date reordering, prefix/suffix, etc.
    #[must_use]
    pub fn format_name(&self, file_name: &str) -> String {
        let mut new_name = String::from(file_name);

        for (pattern, replacement) in &self.config.pre_replace {
            new_name = new_name.replace(pattern, replacement);
        }

        Self::apply_replacements(&mut new_name);
        Self::remove_special_characters(&mut new_name);

        if self.config.convert_case {
            new_name = new_name.to_lowercase();
        }

        self.apply_config_replacements(&mut new_name);

        if self.config.remove_random {
            Self::remove_random_identifiers(&mut new_name);
        }

        Self::apply_titlecase(&mut new_name);
        Self::convert_written_date_format(&mut new_name);

        if let Some(date_flipped_name) = if self.config.rename_directories {
            Date::reorder_directory_date(&new_name)
        } else {
            Date::reorder_filename_date(&new_name, self.config.date_starts_with_year, false, false)
        } {
            new_name = date_flipped_name;
        }

        if let Some(ref prefix) = self.config.prefix {
            new_name = Self::apply_prefix(&new_name, prefix);
        }

        if let Some(ref suffix) = self.config.suffix {
            new_name = self.apply_suffix(&new_name, suffix);
        }

        if !self.config.move_to_start.is_empty() {
            self.move_to_start(&mut new_name);
        }
        if !self.config.move_to_end.is_empty() {
            self.move_to_end(&mut new_name);
        }
        if !self.config.move_date_after_prefix.is_empty() {
            self.move_date_after_prefix(&mut new_name);
        }
        if !self.config.remove_from_start.is_empty() {
            self.remove_from_start(&mut new_name);
        }

        // Apply regex replacements (workaround for prefix regex)
        // Skip reordering if name (after prefix) starts with 5+ digits followed by a boundary
        if !self.config.regex_replace_after.is_empty() {
            let skip_reorder = self.config.prefix.as_ref().is_some_and(|prefix| {
                new_name
                    .strip_prefix(prefix)
                    .map(|s| s.strip_prefix('.').unwrap_or(s))
                    .is_some_and(Self::starts_with_five_or_more_digits)
            });

            if !skip_reorder {
                for (regex, replacement) in &self.config.regex_replace_after {
                    new_name = regex.replace_all(&new_name, replacement).into_owned();
                }
            }
        }

        // Remove consecutive duplicate patterns from substitutions and prefix_dir
        if !self.config.deduplicate_patterns.is_empty() {
            Self::remove_consecutive_duplicates(&mut new_name, &self.config.deduplicate_patterns);
        }

        remove_extra_dots(&mut new_name);

        new_name
    }

    /// Format name without applying prefix/suffix (used for recursive mode).
    #[must_use]
    pub fn format_name_without_prefix_suffix(&self, file_name: &str) -> String {
        let mut new_name = String::from(file_name);

        for (pattern, replacement) in &self.config.pre_replace {
            new_name = new_name.replace(pattern, replacement);
        }

        Self::apply_replacements(&mut new_name);
        Self::remove_special_characters(&mut new_name);

        if self.config.convert_case {
            new_name = new_name.to_lowercase();
        }

        self.apply_config_replacements(&mut new_name);

        if self.config.remove_random {
            Self::remove_random_identifiers(&mut new_name);
        }

        Self::apply_titlecase(&mut new_name);
        Self::convert_written_date_format(&mut new_name);

        if let Some(date_flipped_name) = if self.config.rename_directories {
            Date::reorder_directory_date(&new_name)
        } else {
            Date::reorder_filename_date(&new_name, self.config.date_starts_with_year, false, false)
        } {
            new_name = date_flipped_name;
        }

        if !self.config.move_to_start.is_empty() {
            self.move_to_start(&mut new_name);
        }
        if !self.config.move_to_end.is_empty() {
            self.move_to_end(&mut new_name);
        }
        if !self.config.move_date_after_prefix.is_empty() {
            self.move_date_after_prefix(&mut new_name);
        }
        if !self.config.remove_from_start.is_empty() {
            self.remove_from_start(&mut new_name);
        }

        remove_extra_dots(&mut new_name);

        new_name
    }

    /// Format a name for use as a directory name.
    ///
    /// This applies the same formatting as `format_name` but replaces dots with spaces,
    /// which is the convention for directory names.
    #[must_use]
    pub fn format_directory_name(&self, name: &str) -> String {
        let name = self.format_name(name);
        Date::replace_file_date_with_directory_date(&name).replace('.', " ")
    }

    /// Format a single file using its parent directory name as prefix/suffix.
    #[must_use]
    pub fn format_file_with_parent_prefix_suffix(&self, path: &Path) -> Option<PathBuf> {
        let parent_dir = path.parent()?;
        let parent_name = crate::get_normalized_dir_name(parent_dir).ok()?;
        let formatted_parent = self.format_name_without_prefix_suffix(&parent_name);

        let (file_name, file_extension) = crate::get_normalized_file_name_and_extension(path).ok()?;
        let formatted_name = self.format_name_without_prefix_suffix(&file_name);

        // Apply prefix or suffix based on config
        let final_name = if self.config.prefix_dir_recursive {
            Self::apply_prefix(&formatted_name, &formatted_parent)
        } else {
            self.apply_suffix(&formatted_name, &formatted_parent)
        };

        let new_file = format!("{}.{}", final_name, file_extension.to_lowercase());
        Some(path.with_file_name(new_file))
    }

    /// Apply suffix to the filename, handling various matching scenarios.
    fn apply_suffix(&self, name: &str, suffix: &str) -> String {
        let mut new_name = name.to_string();

        if new_name.starts_with(suffix) {
            new_name = new_name.replacen(suffix, "", 1);
        }

        if new_name.contains(suffix) {
            self.remove_from_start(&mut new_name);
            return new_name;
        }

        let lower_name = new_name.to_lowercase();
        let lower_suffix = suffix.to_lowercase();

        if lower_name.ends_with(&lower_suffix) {
            format!("{}{}", &new_name[..new_name.len() - lower_suffix.len()], suffix)
        } else {
            format!("{new_name}.{suffix}")
        }
    }

    /// Apply user-configured replacements from args and config file.
    fn apply_config_replacements(&self, name: &mut String) {
        for (pattern, replacement) in &self.config.replace {
            *name = name.replace(pattern, replacement);
        }

        for (regex, replacement) in &self.config.regex_replace {
            *name = regex.replace_all(name, replacement).into_owned();
        }
    }

    fn move_to_start(&self, name: &mut String) {
        for pattern in &self.config.move_to_start {
            let re = Regex::new(&format!(r"\b{}\b", regex::escape(pattern))).expect("Failed to create regex pattern");

            if re.is_match(name) {
                *name = format!("{}.{}", pattern, re.replace(name, ""));
            }
        }
    }

    fn move_to_end(&self, name: &mut String) {
        for sub in &self.config.move_to_end {
            if name.contains(sub) {
                *name = format!("{}.{}", name.replace(sub, ""), sub);
            }
        }
    }

    fn move_date_after_prefix(&self, name: &mut String) {
        for prefix in &self.config.move_date_after_prefix {
            if name.starts_with(prefix) {
                if let Some(date_match) = RE_CORRECT_DATE_FORMAT.find(name) {
                    let date = date_match.as_str();
                    let mut new_name = name.clone();

                    // Remove the date from its current location
                    new_name.replace_range(date_match.range(), "");

                    let insert_pos = prefix.len();
                    new_name.insert_str(insert_pos, &format!(".{date}."));

                    *name = new_name;
                }
                if let Some(date_match) = RE_YEAR.find(name) {
                    let date = date_match.as_str().parse::<i32>().expect("Failed to parse year");
                    if date <= *CURRENT_YEAR {
                        let mut new_name = name.clone();

                        new_name.replace_range(date_match.range(), "");

                        let insert_pos = prefix.len();
                        new_name.insert_str(insert_pos, &format!(".{date}."));

                        *name = new_name;
                    }
                }
            }
        }
    }

    fn remove_from_start(&self, name: &mut String) {
        for pattern in &self.config.remove_from_start {
            let re = Regex::new(&format!(r"\b{}\b", regex::escape(pattern))).expect("Failed to create regex pattern");
            if let Some(last_match) = re.find_iter(name).last() {
                // Split the text into parts before the last regex match
                let before_last = &name[..last_match.start()];
                let after_last = &name[last_match.start()..];

                // Remove all occurrences from the first part using regex
                *name = format!("{}.{after_last}", re.replace_all(before_last, ""));
            }
        }
    }

    /// Apply prefix to the filename, handling various matching scenarios.
    fn apply_prefix(name: &str, prefix: &str) -> String {
        let mut new_name = name.to_string();

        if !new_name.starts_with(prefix) && new_name.contains(prefix) {
            new_name = new_name.replacen(prefix, "", 1);
        }

        let lower_name = new_name.to_lowercase();
        let lower_prefix = prefix.to_lowercase();

        if lower_name.starts_with(&lower_prefix) {
            // Full prefix match - update capitalization
            return format!("{}{}", prefix, &new_name[prefix.len()..]);
        }

        // Check if new_name starts with any suffix of the prefix
        let prefix_parts: Vec<&str> = prefix.split('.').collect();
        for i in 1..prefix_parts.len() {
            let suffix = prefix_parts[i..].join(".");
            let lower_suffix = suffix.to_lowercase();

            if lower_name.starts_with(&lower_suffix) {
                // Found a matching suffix, replace with full prefix
                return format!("{}{}", prefix, &new_name[suffix.len()..]);
            }
        }

        format!("{prefix}.{new_name}")
    }

    /// Apply titlecase formatting.
    fn apply_titlecase(name: &mut String) {
        *name = name.trim_start_matches('.').trim_end_matches('.').to_string();
        // Temporarily convert dots back to whitespace so titlecase works
        *name = name.replace('.', " ");
        *name = titlecase::titlecase(name);
        *name = name.replace(' ', ".");
        // Fix encoding capitalization
        *name = name.replace("X265", "x265").replace("X264", "x264");
    }

    /// Apply static and pre-configured replacements to a filename.
    fn apply_replacements(name: &mut String) {
        // Apply static replacements
        for (pattern, replacement) in REPLACE {
            *name = name.replace(pattern, replacement);
        }

        *name = RE_BRACKETS.replace_all(name, ".").into_owned();
        *name = RE_DOTCOM.replace_all(name, ".").into_owned();
        *name = RE_EXCLAMATION.replace_all(name, ".").into_owned();
        *name = RE_WHITESPACE.replace_all(name, ".").into_owned();
        *name = RE_CONSECUTIVE_DOTS.replace_all(name, ".").into_owned();
    }

    /// Remove consecutive duplicate occurrences of patterns from the name.
    /// For example, "Some.Name.Some.Name.File" with pattern "Some.Name." becomes "Some.Name.File"
    fn remove_consecutive_duplicates(name: &mut String, patterns: &[(Regex, String)]) {
        for (regex, replacement) in patterns {
            *name = regex.replace_all(name, replacement.as_str()).into_owned();
        }
    }

    /// Check if the name starts with 5 or more digits followed by a boundary.
    /// Used to skip `prefix_dir` reordering for filenames with leading numeric identifiers.
    pub fn starts_with_five_or_more_digits(name: &str) -> bool {
        RE_LEADING_DIGITS.is_match(name)
    }

    fn has_at_least_six_digits(s: &str) -> bool {
        s.chars().filter(char::is_ascii_digit).count() >= 6
    }

    /// Convert date with written month name to numeral date.
    ///
    /// For example:
    /// ```not_rust
    /// "Jan.3.2020" -> "2020.01.03"
    /// "December.6.2023" -> "2023.12.06"
    /// "23.May.2016" -> "2016.05.23"
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if a captured month name is not found in the month mapping.
    /// This should not happen in practice since the regex only matches valid month names.
    pub fn convert_written_date_format(name: &mut String) {
        // Replace Month.Day.Year
        *name = RE_WRITTEN_DATE_MDY
            .replace_all(name, |caps: &regex::Captures| {
                let year = &caps["year"];
                let month_raw = &caps["month"].to_lowercase();
                let month = WRITTEN_MONTHS_MAP.get(month_raw.as_str()).expect("Failed to map month");
                let day = format!("{:02}", caps["day"].parse::<u8>().expect("Failed to parse day"));
                format!("{year}.{month}.{day}")
            })
            .into_owned();

        // Replace Day.Month.Year
        *name = RE_WRITTEN_DATE_DMY
            .replace_all(name, |caps: &regex::Captures| {
                let year = &caps["year"];
                let month_raw = &caps["month"].to_lowercase();
                let month = WRITTEN_MONTHS_MAP.get(month_raw.as_str()).expect("Failed to map month");
                let day = format!("{:02}", caps["day"].parse::<u8>().expect("Failed to parse day"));
                format!("{year}.{month}.{day}")
            })
            .into_owned();
    }

    /// Only retain alphanumeric characters and a few common filename characters
    fn remove_special_characters(name: &mut String) {
        let cleaned: String = name
            // Split the string into graphemes (for handling emojis and complex characters)
            .graphemes(true)
            .filter(|g| {
                g.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '\'' || c == '&')
            })
            .collect();

        *name = cleaned;
    }

    fn remove_random_identifiers(name: &mut String) {
        *name = RE_IDENTIFIER
            .replace_all(name, |caps: &regex::Captures| {
                let matched_str = &caps[0];
                if Self::has_at_least_six_digits(matched_str)
                    && !RE_RESOLUTION.is_match(matched_str)
                    && !RESOLUTIONS.iter().any(|&number| matched_str.contains(number))
                    && !name.contains("hash2")
                {
                    String::new()
                } else {
                    matched_str.trim().to_string()
                }
            })
            .into_owned();
    }
}

/// Replace two or more consecutive dots with a single dot in place.
pub fn collapse_consecutive_dots_in_place(text: &mut String) {
    *text = RE_CONSECUTIVE_DOTS.replace_all(text, ".").into_owned();
}

/// Replace two or more consecutive dots with a single dot.
#[must_use]
pub fn collapse_consecutive_dots(text: &str) -> String {
    RE_CONSECUTIVE_DOTS.replace_all(text, ".").into_owned()
}

/// Replace two or more consecutive dots with a single dot,
/// then strip any leading or trailing dots.
pub fn remove_extra_dots(text: &mut String) {
    let result = RE_CONSECUTIVE_DOTS.replace_all(text, ".");
    *text = result.trim_start_matches('.').trim_end_matches('.').to_string();
}

#[cfg(test)]
mod collapse_consecutive_dots_tests {
    use super::*;

    #[test]
    fn no_dots() {
        assert_eq!(collapse_consecutive_dots("hello"), "hello");
    }

    #[test]
    fn single_dots_unchanged() {
        assert_eq!(collapse_consecutive_dots("a.b.c"), "a.b.c");
    }

    #[test]
    fn double_dots_collapsed() {
        assert_eq!(collapse_consecutive_dots("a..b"), "a.b");
    }

    #[test]
    fn triple_dots_collapsed() {
        assert_eq!(collapse_consecutive_dots("a...b"), "a.b");
    }

    #[test]
    fn many_consecutive_dots_collapsed() {
        assert_eq!(collapse_consecutive_dots("a.....b"), "a.b");
    }

    #[test]
    fn multiple_groups_of_dots() {
        assert_eq!(collapse_consecutive_dots("a..b...c..d"), "a.b.c.d");
    }

    #[test]
    fn empty_string() {
        assert_eq!(collapse_consecutive_dots(""), "");
    }

    #[test]
    fn only_dots() {
        assert_eq!(collapse_consecutive_dots("...."), ".");
    }

    #[test]
    fn filename_with_double_dots() {
        assert_eq!(collapse_consecutive_dots("video..1080p.mp4"), "video.1080p.mp4");
    }

    #[test]
    fn in_place_mutates_string() {
        let mut text = String::from("a..b...c");
        collapse_consecutive_dots_in_place(&mut text);
        assert_eq!(text, "a.b.c");
    }

    #[test]
    fn in_place_no_change_needed() {
        let mut text = String::from("a.b.c");
        collapse_consecutive_dots_in_place(&mut text);
        assert_eq!(text, "a.b.c");
    }

    #[test]
    fn trim_removes_leading_and_trailing_dots() {
        let mut text = String::from("..a..b..");
        remove_extra_dots(&mut text);
        assert_eq!(text, "a.b");
    }

    #[test]
    fn trim_only_dots_becomes_empty() {
        let mut text = String::from("....");
        remove_extra_dots(&mut text);
        assert_eq!(text, "");
    }

    #[test]
    fn trim_no_change_needed() {
        let mut text = String::from("a.b.c");
        remove_extra_dots(&mut text);
        assert_eq!(text, "a.b.c");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use regex::Regex;

    use super::*;

    static FORMATTER: LazyLock<DotFormat<'static>> = LazyLock::new(|| DotFormat::new(LazyLock::force(&DEFAULT_CONFIG)));

    static DEFAULT_CONFIG: LazyLock<DotRenameConfig> = LazyLock::new(DotRenameConfig::default);

    static REMOVE_RANDOM_CONFIG: LazyLock<DotRenameConfig> = LazyLock::new(|| DotRenameConfig {
        remove_random: true,
        ..Default::default()
    });

    static FORMATTER_REMOVE_RANDOM: LazyLock<DotFormat<'static>> =
        LazyLock::new(|| DotFormat::new(LazyLock::force(&REMOVE_RANDOM_CONFIG)));

    #[test]
    fn test_format_basic() {
        assert_eq!(FORMATTER.format_name("Some file"), "Some.File");
        assert_eq!(FORMATTER.format_name("some file"), "Some.File");
        assert_eq!(FORMATTER.format_name("word"), "Word");
        assert_eq!(FORMATTER.format_name("__word__"), "Word");
        assert_eq!(FORMATTER.format_name("testCAP CAP WORD GL"), "testCAP.CAP.WORD.GL");
        assert_eq!(FORMATTER.format_name("test CAP CAP WORD GL"), "Test.CAP.CAP.WORD.GL");
        assert_eq!(FORMATTER.format_name("CAP WORD GL"), "Cap.Word.Gl");
    }

    #[test]
    fn test_format_convert_case() {
        let config = DotRenameConfig {
            convert_case: true,
            ..Default::default()
        };
        let formatter = DotFormat::new(&config);
        assert_eq!(formatter.format_name("CAP WORD GL"), "Cap.Word.Gl");
        assert_eq!(formatter.format_name("testCAP CAP WORD GL"), "Testcap.Cap.Word.Gl");
        assert_eq!(formatter.format_name("test CAP CAP WORD GL"), "Test.Cap.Cap.Word.Gl");
    }

    #[test]
    fn test_format_name_with_newlines() {
        assert_eq!(
            FORMATTER.format_name("Meeting \tNotes \n(2023) - Draft\r\n"),
            "Meeting.Notes.2023.Draft"
        );
    }

    #[test]
    fn test_format_name_no_brackets() {
        assert_eq!(FORMATTER.format_name("John Doe - Document"), "John.Doe.Document");
    }

    #[test]
    fn test_format_name_with_brackets() {
        assert_eq!(
            FORMATTER.format_name("Project Report - [Final Version]"),
            "Project.Report.Final.Version"
        );
        assert_eq!(
            FORMATTER.format_name("Code {Snippet} (example)"),
            "Code.Snippet.Example"
        );
    }

    #[test]
    fn test_format_name_with_parentheses() {
        assert_eq!(
            FORMATTER.format_name("Meeting Notes (2023) - Draft"),
            "Meeting.Notes.2023.Draft"
        );
    }

    #[test]
    fn test_format_name_with_extra_dots() {
        assert_eq!(FORMATTER.format_name("file..with...dots"), "File.With.Dots");
        assert_eq!(
            FORMATTER.format_name("...leading.and.trailing.dots..."),
            "Leading.and.Trailing.Dots"
        );
    }

    #[test]
    fn test_format_name_with_exclamations() {
        assert_eq!(FORMATTER.format_name("Exciting!Document!!"), "Exciting.Document");
        assert_eq!(FORMATTER.format_name("Hello!!!World!!"), "Hello.World");
    }

    #[test]
    fn test_format_name_with_dotcom() {
        assert_eq!(
            FORMATTER.format_name("visit.website.com.for.details"),
            "Visit.Website.for.Details"
        );
        assert_eq!(
            FORMATTER.format_name("Contact us at email@domain.net"),
            "Contact.Us.at.Email.Domain"
        );
        assert_eq!(FORMATTER.format_name("Contact.company.test"), "Contact.Company.Test");
    }

    #[test]
    fn test_format_name_with_combined_cases() {
        assert_eq!(
            FORMATTER.format_name("Amazing [Stuff]!! Visit my.site.com..now"),
            "Amazing.Stuff.Visit.My.Site.Now"
        );
    }

    #[test]
    fn test_format_name_with_weird_characters() {
        assert_eq!(
            FORMATTER.format_name("Weird-Text-~File-Name-@Example#"),
            "Weird.Text.File.Name.Example"
        );
    }

    #[test]
    fn test_format_name_empty_string() {
        assert_eq!(FORMATTER.format_name(""), "");
    }

    #[test]
    fn test_format_name_no_changes() {
        assert_eq!(FORMATTER.format_name("SingleWord"), "SingleWord");
    }

    #[test]
    fn test_format_name_full_resolution() {
        assert_eq!(
            FORMATTER_REMOVE_RANDOM.format_name("test.string.with resolution. 1234x900"),
            "Test.String.With.Resolution.1234x900"
        );
        assert_eq!(
            FORMATTER_REMOVE_RANDOM.format_name("resolution 719x719"),
            "Resolution.719x719"
        );
        assert_eq!(
            FORMATTER_REMOVE_RANDOM.format_name("resolution 122225x719"),
            "Resolution"
        );
    }

    #[test]
    fn test_move_to_start() {
        let config = DotRenameConfig {
            move_to_start: vec!["Test".to_string()],
            ..Default::default()
        };
        let formatter = DotFormat::new(&config);
        assert_eq!(
            formatter.format_name("This is a test string test"),
            "Test.This.Is.a.String.Test"
        );
        assert_eq!(
            formatter.format_name("Test.This.Is.a.test.string.test"),
            "Test.This.Is.a.Test.String.Test"
        );
        assert_eq!(formatter.format_name("test"), "Test");
        assert_eq!(formatter.format_name("Test"), "Test");
        assert_eq!(
            formatter.format_name("TestOther should not be broken"),
            "TestOther.Should.Not.Be.Broken"
        );
        assert_eq!(formatter.format_name("Test-Something-else"), "Test.Something.Else");
    }

    #[test]
    fn test_move_to_end() {
        let config = DotRenameConfig {
            move_to_end: vec!["Test".to_string()],
            ..Default::default()
        };
        let formatter = DotFormat::new(&config);
        assert_eq!(
            formatter.format_name("This is a test string test"),
            "This.Is.a.String.Test"
        );
        assert_eq!(
            formatter.format_name("Test.This.Is.a.test.string.test"),
            "This.Is.a.String.Test"
        );
        assert_eq!(formatter.format_name("test"), "Test");
        assert_eq!(formatter.format_name("Test"), "Test");
    }

    #[test]
    fn test_remove_identifier() {
        assert_eq!(
            FORMATTER_REMOVE_RANDOM.format_name("This is a string test ^[640e54a564228]"),
            "This.Is.a.String.Test"
        );
        assert_eq!(
            FORMATTER_REMOVE_RANDOM.format_name("This.Is.a.test.string.65f09e4248e03..."),
            "This.Is.a.Test.String"
        );
        assert_eq!(FORMATTER_REMOVE_RANDOM.format_name("test Ph5d9473a841fe9"), "Test");
        assert_eq!(FORMATTER_REMOVE_RANDOM.format_name("Test-355989849"), "Test");
    }

    #[test]
    fn test_format_date() {
        assert_eq!(
            FORMATTER.format_name("This is a test string test 1.1.2014"),
            "This.Is.a.Test.String.Test.2014.01.01"
        );
        assert_eq!(
            FORMATTER.format_name("Test.This.Is.a.test.string.test.30.05.2020"),
            "Test.This.Is.a.Test.String.Test.2020.05.30"
        );
        assert_eq!(
            FORMATTER.format_name("Testing date 30.05.2020 in the middle"),
            "Testing.Date.2020.05.30.in.the.Middle"
        );
    }

    #[test]
    fn test_format_date_mm_dd_yyyy() {
        // MM.DD.YYYY format where day > 12 (unambiguous American date format)
        assert_eq!(FORMATTER.format_name("01.26.2019.2160p.x264"), "2019.01.26.2160p.x264");
        assert_eq!(
            FORMATTER.format_name("video 03.15.2020 1080p"),
            "Video.2020.03.15.1080p"
        );
        assert_eq!(
            FORMATTER.format_name("12.25.2021.holiday.video"),
            "2021.12.25.Holiday.Video"
        );
        assert_eq!(
            FORMATTER.format_name("clip 06.30.2022 720p x265"),
            "Clip.2022.06.30.720p.x265"
        );
        // Day 31 with resolution
        assert_eq!(FORMATTER.format_name("file 07.31.2019 480p"), "File.2019.07.31.480p");
        // Day 13 (minimum unambiguous day)
        assert_eq!(FORMATTER.format_name("test 01.13.2023"), "Test.2023.01.13");
    }

    #[test]
    fn test_format_date_with_adjacent_resolution() {
        // Resolution labels adjacent to dates should not interfere with date parsing
        assert_eq!(FORMATTER.format_name("01.26.2019.2160p.x264"), "2019.01.26.2160p.x264");
        assert_eq!(FORMATTER.format_name("02.14.2020.1440p.hevc"), "2020.02.14.1440p.Hevc");
        assert_eq!(
            FORMATTER.format_name("11.22.2018.1080p.bluray"),
            "2018.11.22.1080p.Bluray"
        );
        assert_eq!(FORMATTER.format_name("04.28.2017.360p.web"), "2017.04.28.360p.Web");
    }

    #[test]
    fn test_format_date_year_first() {
        let config = DotRenameConfig {
            date_starts_with_year: true,
            ..Default::default()
        };
        let formatter = DotFormat::new(&config);

        assert_eq!(
            formatter.format_name("This is a test string test 1.1.2014"),
            "This.Is.a.Test.String.Test.2014.01.01"
        );
        assert_eq!(
            formatter.format_name("Test.This.Is.a.test.string.test.30.05.2020"),
            "Test.This.Is.a.Test.String.Test.2020.05.30"
        );
        assert_eq!(
            formatter.format_name("Test.This.Is.a.test.string.test.24.02.20"),
            "Test.This.Is.a.Test.String.Test.2024.02.20"
        );
        assert_eq!(
            formatter.format_name("Testing date 16.10.20 in the middle"),
            "Testing.Date.2016.10.20.in.the.Middle"
        );
    }

    #[test]
    fn test_prefix_dir() {
        let config = DotRenameConfig {
            prefix: Some("Test.One.Two".to_string()),
            ..Default::default()
        };
        let formatter = DotFormat::new(&config);

        assert_eq!(formatter.format_name("example"), "Test.One.Two.Example");
        assert_eq!(formatter.format_name("two example"), "Test.One.Two.Example");
        assert_eq!(formatter.format_name("1"), "Test.One.Two.1");
        assert_eq!(formatter.format_name("Test one  two three"), "Test.One.Two.Three");
        assert_eq!(formatter.format_name("three"), "Test.One.Two.Three");
        assert_eq!(formatter.format_name("test.one.two"), "Test.One.Two");
        assert_eq!(formatter.format_name(" test one two "), "Test.One.Two");
        assert_eq!(formatter.format_name("Test.One.Two"), "Test.One.Two");
    }

    #[test]
    fn test_starts_with_five_or_more_digits() {
        assert!(
            DotFormat::starts_with_five_or_more_digits("12345 Content"),
            "5 digits with a space should match"
        );
        assert!(
            DotFormat::starts_with_five_or_more_digits("123456789.Content"),
            "9 digits should match"
        );
        assert!(
            DotFormat::starts_with_five_or_more_digits("37432195.Video"),
            "8 digits should match"
        );
        assert!(
            !DotFormat::starts_with_five_or_more_digits("1234.Content"),
            "4 digits should not match"
        );
        assert!(
            !DotFormat::starts_with_five_or_more_digits("Content.12345"),
            "digits not at start should not match"
        );
        assert!(
            !DotFormat::starts_with_five_or_more_digits("12345Content"),
            "digits without a boundary should not match"
        );
    }

    #[test]
    fn test_prefix_dir_with_leading_digits() {
        // Build regex patterns for prefix directory date reordering
        let escaped_name = regex::escape("Prefix");
        let prefix_regex_start_full_date = Regex::new(&format!(
            "^({escaped_name}\\.)(.{{1,32}}?\\.)((20(?:0[0-9]|1[0-9]|2[0-5]))\\.(?:1[0-2]|0?[1-9])\\.(?:[12]\\d|3[01]|0?[1-9])\\.)",
        ))
        .expect("Failed to compile prefix dir full date regex");

        let prefix_regex_start_year = Regex::new(&format!(
            "^({escaped_name}\\.)(.{{1,32}}?\\.)((20(?:0[0-9]|1[0-9]|2[0-5]))\\.)",
        ))
        .expect("Failed to compile prefix dir year regex");

        let prefix_regex_end_full_date = Regex::new(&format!(
            "({escaped_name}\\.)((20(?:0[0-9]|1[0-9]|2[0-5]))\\.(?:1[0-2]|0?[1-9])\\.(?:[12]\\d|3[01]|0?[1-9])\\.)(.{{1,32}}?)$",
        ))
        .expect("Failed to compile prefix dir end full date regex");

        let prefix_regex_end_year = Regex::new(&format!(
            "({escaped_name}\\.)((20(?:0[0-9]|1[0-9]|2[0-5]))\\.)(.{{1,32}}?)$",
        ))
        .expect("Failed to compile prefix dir end year regex");

        let regexes = vec![
            (prefix_regex_start_full_date, "$2$3$1".to_string()),
            (prefix_regex_start_year, "$2$3$1".to_string()),
            (prefix_regex_end_full_date, "$2$1$4".to_string()),
            (prefix_regex_end_year, "$2$1$4".to_string()),
        ];

        let config = DotRenameConfig {
            prefix: Some("Prefix".to_string()),
            regex_replace_after: regexes,
            ..Default::default()
        };
        let formatter = DotFormat::new(&config);

        // 5+ digits at start should NOT be reordered (prefix stays at start)
        assert_eq!(
            formatter.format_name("12345 content 2024.01.15 rest"),
            "Prefix.12345.Content.2024.01.15.Rest",
            "5+ digits should keep prefix at start"
        );

        // Real-world example: 8-digit ID with date should keep prefix at start
        assert_eq!(
            formatter.format_name("37432195_video_2021-06-28_18-24"),
            "Prefix.37432195.Video.2021.06.28.18.24",
            "8-digit ID should keep prefix at start"
        );

        // 4 digits at start SHOULD be reordered (prefix moves after date)
        assert_eq!(
            formatter.format_name("1234 content 2024.01.15 rest"),
            "1234.Content.2024.01.15.Prefix.Rest",
            "4 digits should allow reordering"
        );

        // Content without leading digits should be reordered
        assert_eq!(
            formatter.format_name("content 2024.01.15 rest"),
            "Content.2024.01.15.Prefix.Rest",
            "No leading digits should allow reordering"
        );
    }

    #[test]
    fn test_prefix_dir_start_option() {
        // With prefix_dir_start = true, no reordering regexes are added
        let config = DotRenameConfig {
            prefix: Some("Prefix".to_string()),
            prefix_dir_start: true,
            ..Default::default()
        };
        let formatter = DotFormat::new(&config);

        assert_eq!(
            formatter.format_name("content 2024.01.15 rest"),
            "Prefix.Content.2024.01.15.Rest",
            "prefix_dir_start should keep prefix at start"
        );

        assert_eq!(
            formatter.format_name("1234 content 2024.01.15 rest"),
            "Prefix.1234.Content.2024.01.15.Rest",
            "prefix_dir_start should keep prefix at start even with 4 digits"
        );

        assert_eq!(
            formatter.format_name("37432195_video_2021-06-28_18-24"),
            "Prefix.37432195.Video.2021.06.28.18.24",
            "prefix_dir_start should keep prefix at start for numeric IDs"
        );
    }

    #[test]
    fn test_prefix_with_explicit_name() {
        let config = DotRenameConfig {
            prefix: Some("My.Prefix".to_string()),
            ..Default::default()
        };
        let formatter = DotFormat::new(&config);

        assert_eq!(formatter.format_name("some file name"), "My.Prefix.Some.File.Name");
        assert_eq!(formatter.format_name("another_file"), "My.Prefix.Another.File");
    }

    #[test]
    fn test_suffix_with_explicit_name() {
        let config = DotRenameConfig {
            suffix: Some("My.Suffix".to_string()),
            ..Default::default()
        };
        let formatter = DotFormat::new(&config);

        assert_eq!(formatter.format_name("some file name"), "Some.File.Name.My.Suffix");
        assert_eq!(formatter.format_name("another_file"), "Another.File.My.Suffix");
    }

    #[test]
    fn test_format_name_without_prefix_suffix() {
        let formatter = DotFormat::new(&DEFAULT_CONFIG);

        assert_eq!(
            formatter.format_name_without_prefix_suffix("Some_File_Name"),
            "Some.File.Name"
        );
        assert_eq!(
            formatter.format_name_without_prefix_suffix("test 2024-01-15 video"),
            "Test.2024.01.15.Video"
        );
    }

    #[test]
    fn test_prefix_and_suffix_together() {
        let config = DotRenameConfig {
            prefix: Some("Prefix".to_string()),
            suffix: Some("Suffix".to_string()),
            ..Default::default()
        };
        let formatter = DotFormat::new(&config);

        let result = formatter.format_name("file name");
        assert_eq!(result, "Prefix.File.Name.Suffix");
    }

    #[test]
    fn test_prefix_already_present_in_filename() {
        let config = DotRenameConfig {
            prefix: Some("Artist.Name".to_string()),
            ..Default::default()
        };
        let formatter = DotFormat::new(&config);

        assert_eq!(
            formatter.format_name("Artist Name - Song Title"),
            "Artist.Name.Song.Title"
        );
        assert_eq!(
            formatter.format_name("artist.name.song.title"),
            "Artist.Name.Song.Title"
        );
    }

    #[test]
    fn test_suffix_already_present_in_filename() {
        let config = DotRenameConfig {
            suffix: Some("2024".to_string()),
            ..Default::default()
        };
        let formatter = DotFormat::new(&config);

        assert_eq!(formatter.format_name("song title 2024"), "Song.Title.2024");
    }
}

#[cfg(test)]
mod written_date_tests {
    use super::*;

    #[test]
    fn test_single_date() {
        let mut input = "Mar.23.2016".to_string();
        DotFormat::convert_written_date_format(&mut input);
        assert_eq!(input, "2016.03.23");

        let mut input = "23.mar.2016".to_string();
        DotFormat::convert_written_date_format(&mut input);
        assert_eq!(input, "2016.03.23");

        let mut input = "March.1.2011".to_string();
        DotFormat::convert_written_date_format(&mut input);
        assert_eq!(input, "2011.03.01");

        let mut input = "1.March.2011".to_string();
        DotFormat::convert_written_date_format(&mut input);
        assert_eq!(input, "2011.03.01");

        let mut input = "December.20.2023".to_string();
        DotFormat::convert_written_date_format(&mut input);
        assert_eq!(input, "2023.12.20");

        let mut input = "20.December.2023".to_string();
        DotFormat::convert_written_date_format(&mut input);
        assert_eq!(input, "2023.12.20");
    }

    #[test]
    fn test_multiple_dates() {
        let mut input = "Mar.23.2016 Jun.17.2015".to_string();
        DotFormat::convert_written_date_format(&mut input);
        assert_eq!(input, "2016.03.23 2015.06.17");
    }

    #[test]
    fn test_mixed_text() {
        let mut input = "Event on Apr.5.2021 at noon".to_string();
        DotFormat::convert_written_date_format(&mut input);
        assert_eq!(input, "Event on 2021.04.05 at noon");
    }

    #[test]
    fn test_edge_case_single_digit_day() {
        let mut input = "Jan.03.2020".to_string();
        DotFormat::convert_written_date_format(&mut input);
        assert_eq!(input, "2020.01.03");
    }

    #[test]
    fn test_no_date_in_text() {
        let mut input = "This text has no date".to_string();
        DotFormat::convert_written_date_format(&mut input);
        assert_eq!(input, "This text has no date");
    }

    #[test]
    fn test_leading_and_trailing_spaces() {
        let mut input = "Something.Feb.Jun.09.2022".to_string();
        DotFormat::convert_written_date_format(&mut input);
        assert_eq!(input, "Something.Feb.2022.06.09");
    }
}

#[cfg(test)]
mod move_date_tests {
    use std::sync::LazyLock;

    use super::*;

    static MOVE_DATE_CONFIG: LazyLock<DotRenameConfig> = LazyLock::new(|| DotRenameConfig {
        move_date_after_prefix: vec!["Test".to_string(), "Prefix".to_string()],
        date_starts_with_year: true,
        ..Default::default()
    });

    static FORMATTER: LazyLock<DotFormat<'static>> =
        LazyLock::new(|| DotFormat::new(LazyLock::force(&MOVE_DATE_CONFIG)));

    #[test]
    fn test_valid_date() {
        assert_eq!(
            FORMATTER.format_name("Test something 2010.11.16"),
            "Test.2010.11.16.Something"
        );
        assert_eq!(
            FORMATTER.format_name("Test something 1080p 2010.11.16"),
            "Test.2010.11.16.Something.1080p"
        );
    }

    #[test]
    fn test_short_date() {
        assert_eq!(
            FORMATTER.format_name("Test something else 25.05.30"),
            "Test.2025.05.30.Something.Else"
        );
    }

    #[test]
    fn test_no_match_with_valid_date() {
        assert_eq!(
            FORMATTER.format_name("something else 2024.01.01"),
            "Something.Else.2024.01.01"
        );
        assert_eq!(
            FORMATTER.format_name("something else 2160p 24.05.28"),
            "Something.Else.2160p.2024.05.28"
        );
    }
}

#[cfg(test)]
mod test_remove_from_start {
    use std::sync::LazyLock;

    use super::*;

    static REMOVE_START_CONFIG: LazyLock<DotRenameConfig> = LazyLock::new(|| DotRenameConfig {
        remove_from_start: vec!["Test".to_string(), "test".to_string()],
        ..Default::default()
    });

    static FORMATTER: LazyLock<DotFormat<'static>> =
        LazyLock::new(|| DotFormat::new(LazyLock::force(&REMOVE_START_CONFIG)));

    #[test]
    fn test_no_patterns() {
        assert_eq!(FORMATTER.format_name("test.string.test"), "String.Test");
    }

    #[test]
    fn test_single_occurrence() {
        assert_eq!(FORMATTER.format_name("test.string"), "Test.String");
    }

    #[test]
    fn test_multiple_occurrences() {
        assert_eq!(FORMATTER.format_name("test.string.test.test"), "String.Test");
        assert_eq!(
            FORMATTER.format_name("test.string.test.something.test"),
            "String.Something.Test"
        );
    }

    #[test]
    fn test_partial_word_match() {
        assert_eq!(FORMATTER.format_name("testing.test.contest"), "Testing.Test.Contest");
    }

    #[test]
    fn test_consecutive_patterns() {
        assert_eq!(FORMATTER.format_name("test.test.test.test"), "Test");
    }
}

#[cfg(test)]
mod test_deduplicate_patterns {
    use std::sync::LazyLock;

    use regex::Regex;

    use super::*;

    static DEDUP_CONFIG: LazyLock<DotRenameConfig> = LazyLock::new(|| DotRenameConfig {
        replace: vec![("SomeName.".to_string(), "Some.Name.".to_string())],
        deduplicate_patterns: vec![(
            Regex::new(r"(Some\.Name\.){2,}").expect("valid regex"),
            "Some.Name.".to_string(),
        )],
        ..Default::default()
    });

    static FORMATTER: LazyLock<DotFormat<'static>> = LazyLock::new(|| DotFormat::new(LazyLock::force(&DEDUP_CONFIG)));

    #[test]
    fn test_no_duplicates() {
        assert_eq!(FORMATTER.format_name("Some.Name.File"), "Some.Name.File");
    }

    #[test]
    fn test_double_duplicate() {
        assert_eq!(FORMATTER.format_name("SomeName.SomeName.File"), "Some.Name.File");
        assert_eq!(FORMATTER.format_name("SomeName.Some.Name.File"), "Some.Name.File");
        assert_eq!(
            FORMATTER.format_name("Some.SomeName.Some.Name.File"),
            "Some.Some.Name.File"
        );
        assert_eq!(
            FORMATTER.format_name("Something.SomeName.Some.Name.File"),
            "Something.Some.Name.File"
        );
    }

    #[test]
    fn test_triple_duplicate() {
        assert_eq!(
            FORMATTER.format_name("SomeName.SomeName.SomeName.File"),
            "Some.Name.File"
        );
        assert_eq!(
            FORMATTER.format_name("SomeName.SomeName.Some.Name.File"),
            "Some.Name.File"
        );
        assert_eq!(FORMATTER.format_name("SomeName.File"), "Some.Name.File");
        assert_eq!(FORMATTER.format_name("Some.Name.File"), "Some.Name.File");
    }

    #[test]
    fn test_substitute_creates_duplicate() {
        assert_eq!(FORMATTER.format_name("Some.Name.SomeName.File"), "Some.Name.File");
    }

    #[test]
    fn test_mixed_case_duplicates() {
        let config = DotRenameConfig {
            deduplicate_patterns: vec![(Regex::new(r"(Test\.){2,}").expect("valid regex"), "Test.".to_string())],
            ..Default::default()
        };
        let formatter = DotFormat::new(&config);
        assert_eq!(formatter.format_name("Test.Test.File"), "Test.File");
    }
}
