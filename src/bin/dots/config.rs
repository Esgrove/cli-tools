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
    let config_regex = DotsConfig::compile_regex_patterns(user_config.regex_replace.clone())?;

    let include: Vec<String> = user_config
        .include
        .clone()
        .into_iter()
        .chain(replace.iter().map(|(pattern, _)| pattern.clone()))
        .chain(cli.include.clone())
        .unique()
        .collect();

    replace.extend(removes);
    replace.extend(user_config.replace.clone());
    let replace: Vec<(String, String)> = replace.into_iter().unique().collect();

    regex_replace.extend(config_regex);

    let move_date_after_prefix = user_config
        .move_date_after_prefix
        .clone()
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
        exclude: cli.exclude.clone(),
        increment_name: cli.increment || user_config.increment,
        move_date_after_prefix,
        move_to_end: user_config.move_to_end.clone(),
        move_to_start: user_config.move_to_start.clone(),
        overwrite: cli.force || user_config.overwrite,
        pre_replace: user_config.pre_replace.clone(),
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
        remove_from_start: user_config.remove_from_start.clone(),
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
