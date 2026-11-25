//! Interactive prompts and match highlighting for the `dirmove` binary.
//!
//! Asks for a directory name when creating a directory for a group of files,
//! reads prompt answers, and highlights the part of a file name that matched a directory name.

use std::io::Write;
use std::path::Path;

use colored::Colorize;

use cli_tools_common::{format_text_with_highlight, path_to_filename_string};
use cli_tools_dirmove::{NormalizedFilename, PromptResult};

/// Prompt the user for directory name confirmation with optional database formatting.
/// Returns a confirmed path, a skipped result, or a request to save the group as ignored.
///
/// If a database suggestion is provided as an exact match with different formatting,
/// it is used silently as the default.
/// New directories are created under `output_root`.
///
/// Accepts:
/// - "y" or "yes" to confirm with the effective name, database formatting if available, otherwise suggested.
/// - "n" or "no" to skip.
/// - "s" to skip and save the group name to `ignored_group_names` in the config file.
/// - Any other non-empty string as a custom directory name, with confirmation.
pub fn prompt_for_directory_name(
    output_root: &Path,
    suggested_name: &str,
    database_suggestion: Option<&str>,
) -> anyhow::Result<PromptResult> {
    // If the database has an exact match with the same letters and different formatting, use it silently.
    // This maps names like "JaneDoe" to "Jane Doe" without showing confusing messages.
    let effective_suggestion = database_suggestion.unwrap_or(suggested_name);
    let effective_path = output_root.join(effective_suggestion);

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
    let input = input.trim();

    // Empty input, "y", or "yes" confirms the effective suggestion.
    if input.is_empty() || input.eq_ignore_ascii_case("y") || input.eq_ignore_ascii_case("yes") {
        return Ok(PromptResult::Confirmed(effective_path));
    }

    // "n" or "no" skips the group.
    if input.eq_ignore_ascii_case("n") || input.eq_ignore_ascii_case("no") {
        println!("  Skipped");
        return Ok(PromptResult::Skipped);
    }

    // "s" skips the group and saves it to ignored_group_names in config.
    if input.eq_ignore_ascii_case("s") {
        return Ok(PromptResult::SaveToIgnored);
    }

    // Any other non empty input is treated as a custom directory name.
    let custom_name = input;
    let custom_path = output_root.join(custom_name);

    // Show what will happen and ask for confirmation.
    if custom_path.exists() {
        println!("  {} Directory '{}' already exists", "→".green(), custom_name.cyan());
    } else {
        println!("  {} Will create directory: {}", "→".yellow(), custom_name.cyan());
    }

    if cli_tools_common::get_user_confirmation(&format!("Confirm using '{custom_name}'?"), true)? {
        Ok(PromptResult::Confirmed(custom_path))
    } else {
        println!("  Skipped");
        Ok(PromptResult::Skipped)
    }
}

/// Read one prompt answer, or `None` when the input has ended without one.
pub fn read_prompt_answer(mut reader: impl std::io::BufRead) -> std::io::Result<Option<String>> {
    let mut input = String::new();
    let bytes_read = reader.read_line(&mut input)?;
    Ok((bytes_read > 0).then_some(input))
}

/// Format a file name with the part matching an already normalized name highlighted.
pub fn format_filename_with_normalized_highlight(file_path: &Path, normalized_name: &str) -> String {
    let filename = path_to_filename_string(file_path);
    let range = NormalizedFilename::new(&filename).find_match(normalized_name);
    format_text_with_highlight(&filename, range)
}

/// Return the confirmation prompt for a whole directory merge offer.
pub const fn directory_merge_confirmation_prompt(source_directory_count: usize) -> &'static str {
    if source_directory_count == 1 {
        "Merge directory to this directory?"
    } else {
        "Merge directories to this directory?"
    }
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
mod test_directory_merge_confirmation_prompt {
    use super::*;

    #[test]
    fn directory_merge_confirmation_prompt_uses_singular_for_one_source() {
        assert_eq!(
            directory_merge_confirmation_prompt(1),
            "Merge directory to this directory?"
        );
    }

    #[test]
    fn directory_merge_confirmation_prompt_uses_plural_for_multiple_sources() {
        assert_eq!(
            directory_merge_confirmation_prompt(2),
            "Merge directories to this directory?"
        );
    }
}
