//! Interactive prompts and match highlighting for the `dirmove` binary.
//!
//! Asks for a directory name when creating a directory for a group of files,
//! reads prompt answers, and highlights the part of a file name that matched a directory name.

use std::io::Write;
use std::path::{Path, PathBuf};

use colored::Colorize;

use cli_tools_common::{format_text_with_highlight, path_to_filename_string};
use cli_tools_dirmove::NormalizedFilename;

use crate::report;

/// Result of prompting the user for a directory name in create mode.
#[derive(Debug)]
pub enum PromptResult {
    /// User confirmed: move files to this directory path.
    Confirmed(PathBuf),
    /// User skipped this group.
    Skipped,
    /// User chose to skip and save the group name to `ignored_group_names` in the config file.
    SaveToIgnored,
}

/// One answer to the create directory prompt.
#[derive(Debug, PartialEq, Eq)]
enum Answer<'a> {
    /// Use the suggested directory name.
    Confirm,
    /// Skip the group.
    Skip,
    /// Skip the group and save its name to `ignored_group_names` in the config file.
    SaveToIgnored,
    /// Use this directory name instead of the suggestion.
    CustomName(&'a str),
}

/// Prompt the user for directory name confirmation with optional database formatting.
/// Returns a confirmed path, a skipped result, or a request to save the group as ignored.
///
/// If a database suggestion is provided as an exact match with different formatting,
/// it is used silently as the default.
/// New directories are created under `output_root`.
/// See `parse_answer` for the accepted answers.
/// A custom directory name is confirmed separately before it is used.
pub fn prompt_for_directory_name(
    output_root: &Path,
    suggested_name: &str,
    database_suggestion: Option<&str>,
) -> anyhow::Result<PromptResult> {
    // If the database has an exact match with the same letters and different formatting, use it silently.
    // This maps names like "JaneDoe" to "Jane Doe" without showing confusing messages.
    let effective_suggestion = database_suggestion.unwrap_or(suggested_name);

    print!(
        "{}",
        format!("Create directory and move files? [Y/n/s or custom name] [{effective_suggestion}]: ").magenta()
    );
    std::io::stdout().flush()?;

    // Without an answer, such as when the input is not a terminal, nothing is moved.
    let Some(input) = read_prompt_answer(std::io::stdin().lock())? else {
        println!("  Skipped, no answer");
        return Ok(PromptResult::Skipped);
    };

    match parse_answer(&input) {
        Answer::Confirm => Ok(PromptResult::Confirmed(output_root.join(effective_suggestion))),
        Answer::Skip => {
            report::print_skipped();
            Ok(PromptResult::Skipped)
        }
        Answer::SaveToIgnored => Ok(PromptResult::SaveToIgnored),
        Answer::CustomName(custom_name) => {
            let custom_path = output_root.join(custom_name);
            report::print_target_directory_status(custom_path.exists(), custom_name);
            if cli_tools_common::get_user_confirmation(&format!("Confirm using '{custom_name}'?"), true)? {
                Ok(PromptResult::Confirmed(custom_path))
            } else {
                report::print_skipped();
                Ok(PromptResult::Skipped)
            }
        }
    }
}

/// Format a file name with the part matching an already normalized name highlighted.
pub fn format_filename_with_normalized_highlight(file_path: &Path, normalized_name: &str) -> String {
    let filename = path_to_filename_string(file_path);
    let range = NormalizedFilename::new(&filename).find_match(normalized_name);
    format_text_with_highlight(&filename, range)
}

/// Parse one answer to the create directory prompt, ignoring surrounding whitespace.
///
/// Accepts:
/// - Nothing, "y", or "yes" to confirm the suggested name.
/// - "n" or "no" to skip.
/// - "s" to skip and save the group name to `ignored_group_names` in the config file.
/// - Any other text as a custom directory name.
fn parse_answer(input: &str) -> Answer<'_> {
    let input = input.trim();
    if input.is_empty() || input.eq_ignore_ascii_case("y") || input.eq_ignore_ascii_case("yes") {
        Answer::Confirm
    } else if input.eq_ignore_ascii_case("n") || input.eq_ignore_ascii_case("no") {
        Answer::Skip
    } else if input.eq_ignore_ascii_case("s") {
        Answer::SaveToIgnored
    } else {
        Answer::CustomName(input)
    }
}

/// Read one prompt answer, or `None` when the input has ended without one.
fn read_prompt_answer(mut reader: impl std::io::BufRead) -> std::io::Result<Option<String>> {
    let mut input = String::new();
    let bytes_read = reader.read_line(&mut input)?;
    Ok((bytes_read > 0).then_some(input))
}

#[cfg(test)]
mod test_read_prompt_answer {
    use super::*;

    #[test]
    fn ended_input_gives_no_answer() {
        assert_eq!(
            read_prompt_answer(std::io::Cursor::new("")).expect("reading should work"),
            None
        );
    }

    #[test]
    fn an_empty_line_is_an_empty_answer() {
        let answer = read_prompt_answer(std::io::Cursor::new("\n")).expect("reading should work");
        assert_eq!(answer.as_deref().map(str::trim), Some(""));
    }
}

#[cfg(test)]
mod test_parse_answer {
    use super::*;

    #[test]
    fn empty_and_yes_answers_confirm() {
        for input in ["", "\n", "y", "Y", "yes", " YES \n"] {
            assert_eq!(parse_answer(input), Answer::Confirm, "{input:?}");
        }
    }

    #[test]
    fn no_answers_skip() {
        for input in ["n", "N", "no", "No\n"] {
            assert_eq!(parse_answer(input), Answer::Skip, "{input:?}");
        }
    }

    #[test]
    fn s_saves_the_group_name_as_ignored() {
        assert_eq!(parse_answer("s"), Answer::SaveToIgnored);
        assert_eq!(parse_answer(" S \n"), Answer::SaveToIgnored);
    }

    #[test]
    fn other_text_is_a_trimmed_custom_name() {
        assert_eq!(parse_answer("  Jane Doe \n"), Answer::CustomName("Jane Doe"));
        assert_eq!(parse_answer("nope"), Answer::CustomName("nope"));
        assert_eq!(parse_answer("save"), Answer::CustomName("save"));
    }
}
