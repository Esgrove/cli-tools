//! Configuration module for dots.
//!
//! Handles reading configuration from CLI arguments and the user config file.

use anyhow::Result;
use itertools::Itertools;
use regex::Regex;

use cli_tools::dot_rename::{DotRenameConfig, DotsConfig};

use crate::DotsCli;

/// Create config from CLI arguments and user config file.
///
/// # Errors
/// Returns an error if regex patterns in the config are invalid.
pub fn build_config(cli: &DotsCli) -> Result<DotRenameConfig> {
    let user_config = DotsConfig::get_user_config()?;
    let substitutes = DotsConfig::parse_substitutes(&cli.substitute);
    let removes = DotsConfig::parse_removes(&cli.remove);

    // Compile regex patterns for deduplication (non-empty replacements from substitutes)
    let mut seen_replacements = std::collections::HashSet::new();
    let deduplicate_patterns: Vec<(Regex, String)> = substitutes
        .iter()
        .filter(|(_, replacement)| !replacement.is_empty() && seen_replacements.insert(replacement.as_str()))
        .filter_map(|(_, replacement)| {
            let escaped = regex::escape(replacement);
            Regex::new(&format!(r"({escaped}){{2,}}"))
                .ok()
                .map(|re| (re, replacement.clone()))
        })
        .collect();

    let mut replace = substitutes;
    let mut regex_replace = DotsConfig::parse_regex_substitutes(&cli.regex)?;
    let config_regex = DotsConfig::compile_regex_patterns(user_config.regex_replace)?;

    let include: Vec<String> = user_config
        .include
        .into_iter()
        .chain(cli.include.clone())
        .unique()
        .collect();

    // Substitute and remove patterns use OR logic for pre-filtering:
    // a file only needs to match at least one pattern to be processed.
    let include_any: Vec<String> = replace
        .iter()
        .chain(removes.iter())
        .map(|(pattern, _)| pattern.clone())
        .unique()
        .collect();

    replace.extend(removes);
    replace.extend(user_config.replace);
    let replace: Vec<(String, String)> = replace.into_iter().unique().collect();

    regex_replace.extend(config_regex);

    let move_date_after_prefix = user_config
        .move_date_after_prefix
        .into_iter()
        .map(|mut s| {
            if !s.ends_with('.') {
                s.push('.');
            }
            s
        })
        .collect::<Vec<_>>();

    Ok(DotRenameConfig {
        convert_case: cli.case,
        date_starts_with_year: !cli.year || user_config.date_starts_with_year,
        debug: cli.debug || user_config.debug,
        deduplicate_patterns,
        dryrun: cli.print || user_config.dryrun,
        include,
        include_any,
        exclude: cli.exclude.clone(),
        increment_name: cli.increment || user_config.increment,
        move_date_after_prefix,
        move_to_end: DotsConfig::compile_word_boundary_patterns(user_config.move_to_end)?,
        move_to_start: DotsConfig::compile_word_boundary_patterns(user_config.move_to_start)?,
        overwrite: cli.force || user_config.overwrite,
        pre_replace: user_config.pre_replace,
        prefix: cli.prefix.clone(),
        prefix_dir: cli.prefix_dir
            || cli.prefix_dir_start
            || cli.prefix_dir_recursive
            || user_config.prefix_dir
            || user_config.prefix_dir_start
            || user_config.prefix_dir_recursive,
        prefix_dir_recursive: cli.prefix_dir_recursive || user_config.prefix_dir_recursive,
        prefix_dir_start: cli.prefix_dir_start || user_config.prefix_dir_start,
        recurse: cli.recurse
            || cli.prefix_dir_recursive
            || cli.suffix_dir_recursive
            || user_config.recurse
            || user_config.prefix_dir_recursive
            || user_config.suffix_dir_recursive,
        regex_replace,
        regex_replace_after: Vec::default(),
        remove_from_start: user_config.remove_from_start,
        remove_random: cli.random || user_config.remove_random,
        rename_directories: cli.directory || user_config.directory,
        replace,
        suffix: cli.suffix.clone(),
        suffix_dir: cli.suffix_dir
            || cli.suffix_dir_recursive
            || user_config.suffix_dir
            || user_config.suffix_dir_recursive,
        suffix_dir_recursive: cli.suffix_dir_recursive || user_config.suffix_dir_recursive,
        verbose: cli.verbose || user_config.verbose,
    })
}

#[cfg(test)]
mod test_build_config {
    use clap::Parser;

    use super::*;

    #[test]
    fn merges_cli_arguments_with_fixture_config() {
        let cli = DotsCli::try_parse_from([
            "dots", "-c", "-D", "-d", "-f", "-i", "-p", "-r", "-m", "-v", "-n", "*.avi", "-e", "skip", "-s", "OLD",
            "new", "-z", "REMOVE", "-g", "x+", "y", "--prefix", "PRE", "--suffix", "SUF",
        ])
        .expect("combined CLI arguments should parse");

        let config = build_config(&cli).expect("config should build");

        assert!(config.convert_case);
        assert!(config.debug);
        assert!(config.rename_directories);
        assert!(config.overwrite);
        assert!(config.increment_name);
        assert!(config.dryrun);
        assert!(config.recurse);
        assert!(config.remove_random);
        assert!(config.verbose);
        assert_eq!(config.prefix.as_deref(), Some("PRE"));
        assert_eq!(config.suffix.as_deref(), Some("SUF"));
        assert_eq!(config.exclude, vec!["skip"]);
        assert_eq!(config.include, vec!["*.mkv", "*.mp4", "*.avi"]);
        assert_eq!(config.include_any, vec!["OLD", "REMOVE"]);
        assert_eq!(config.replace[0], ("OLD".to_string(), "new".to_string()));
        assert_eq!(config.replace[1], ("REMOVE".to_string(), String::new()));
        assert_eq!(config.regex_replace[0].0.as_str(), "x+");
        assert_eq!(config.regex_replace[0].1, "y");
    }

    #[test]
    fn builds_unique_deduplication_patterns_for_non_empty_replacements() {
        let cli = DotsCli::try_parse_from([
            "dots", "-s", "first", "same", "-s", "second", "same", "-s", "dot", ".", "-s", "removed", "",
        ])
        .expect("substitutions should parse");

        let config = build_config(&cli).expect("config should build");

        assert_eq!(config.deduplicate_patterns.len(), 2);
        assert!(config.deduplicate_patterns[0].0.is_match("samesamesame"));
        assert_eq!(config.deduplicate_patterns[0].1, "same");
        assert!(config.deduplicate_patterns[1].0.is_match("..."));
        assert!(!config.deduplicate_patterns[1].0.is_match("abc"));
    }

    #[test]
    fn recursive_directory_modes_imply_enabled_mode_and_recursion() {
        let prefix_cli = DotsCli::try_parse_from(["dots", "--prefix-dir-recursive"])
            .expect("recursive prefix argument should parse");
        let prefix_config = build_config(&prefix_cli).expect("prefix config should build");
        assert!(prefix_config.prefix_dir);
        assert!(prefix_config.prefix_dir_recursive);
        assert!(prefix_config.recurse);

        let suffix_cli = DotsCli::try_parse_from(["dots", "--suffix-dir-recursive"])
            .expect("recursive suffix argument should parse");
        let suffix_config = build_config(&suffix_cli).expect("suffix config should build");
        assert!(suffix_config.suffix_dir);
        assert!(suffix_config.suffix_dir_recursive);
        assert!(suffix_config.recurse);
    }

    #[test]
    fn invalid_cli_regex_returns_contextual_error() {
        let cli = DotsCli::try_parse_from(["dots", "--regex", "[invalid", "replacement"])
            .expect("clap should preserve regex text");

        let error = build_config(&cli).expect_err("invalid regex should fail config construction");

        assert!(error.to_string().contains("Invalid regex"));
    }
}
