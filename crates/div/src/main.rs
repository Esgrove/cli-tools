//! The `div` binary.
//!
//! Prints a divider comment line with optional centered or aligned text.

use clap::{Parser, Subcommand};
use cli_tools_common::CompletionArgs;

/// Subcommands for div.
#[derive(Subcommand)]
enum DividerCommand {
    /// Generate shell completion script
    #[command(name = "completion")]
    Completion(CompletionArgs),
}

#[derive(Parser)]
#[command(
    author,
    version = cli_tools_version::VERSION_INFO,
    name = env!("CARGO_BIN_NAME"),
    about = "Print divider comment with centered text"
)]
struct Args {
    #[command(subcommand)]
    command: Option<DividerCommand>,

    /// Optional divider texts
    text: Vec<String>,

    /// Divider length in number of characters
    #[arg(short, long, default_value_t = 120)]
    length: usize,

    /// Divider character to use
    #[arg(short, long = "char", default_value_t = '%')]
    character: char,

    /// Align multiple divider texts to same start position
    #[arg(short, long)]
    align: bool,

    /// Print verbose output
    #[arg(short, long, global = true)]
    verbose: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(DividerCommand::Completion(completion)) = &args.command {
        return completion.generate::<Args>(args.verbose, env!("CARGO_BIN_NAME"));
    }
    for line in divider_lines(&args.text, args.length, args.character, args.align) {
        println!("{line}");
    }
    Ok(())
}

/// Build the divider lines for the given texts.
///
/// No texts gives one plain divider.
/// With `align` and more than one text, the texts that fit start at the same column,
/// otherwise each one is centered.
fn divider_lines(texts: &[String], length: usize, character: char, align: bool) -> Vec<String> {
    if texts.is_empty() {
        return vec![format_centered_divider("", length, character)];
    }

    if texts.len() > 1 && align {
        // Texts are printed uppercased, and uppercasing can make a text longer, such as "ß" becoming "SS",
        // so the longest fitting text is measured in the form it is printed in.
        let longest_fitting: Option<usize> = texts
            .iter()
            .map(|text| text.trim().to_uppercase().chars().count())
            .filter(|&width| width > 0 && width + 2 < length)
            .max();

        let aligned_length: usize = longest_fitting.map_or(0, |longest| length.saturating_sub(longest + 2) / 2);
        return texts
            .iter()
            .map(|text| {
                if aligned_length == 0 {
                    format_centered_divider(text, length, character)
                } else {
                    format_aligned_divider(text, length, character, aligned_length)
                }
            })
            .collect();
    }

    texts
        .iter()
        .map(|text| format_centered_divider(text, length, character))
        .collect()
}

/// Format a divider line with `text` centered inside `count` characters.
fn format_centered_divider(text: &str, count: usize, character: char) -> String {
    let text = text.trim().to_uppercase();
    let div = format!("{character}");
    if text.is_empty() {
        return div.repeat(count);
    }

    let message_length: usize = text.chars().count() + 2;
    if message_length >= count {
        text
    } else {
        let total_padding: usize = count - message_length;
        let padding_side: usize = total_padding / 2;
        format_div_with_text(padding_side, &text, padding_side + total_padding % 2, &div)
    }
}

/// Format a divider line with `text` aligned at `padding_left` divider characters from the left.
fn format_aligned_divider(text: &str, count: usize, character: char, padding_left: usize) -> String {
    let text = text.trim().to_uppercase();
    let div = format!("{character}");
    if text.is_empty() {
        return div.repeat(count);
    }

    let message_length: usize = text.chars().count() + 2;
    if message_length >= count {
        text
    } else {
        let total_padding: usize = count - message_length;
        let clamped_left = padding_left.min(total_padding);
        let padding_right: usize = total_padding - clamped_left;
        format_div_with_text(clamped_left, &text, padding_right, &div)
    }
}

/// Join left and right divider padding around uppercase `text`, omitting dangling spaces when a side has zero padding.
fn format_div_with_text(num_left: usize, text: &str, num_right: usize, divider: &str) -> String {
    match (num_left, num_right) {
        (0, 0) => text.to_string(),
        (0, right) => format!("{text} {}", divider.repeat(right)),
        (left, 0) => format!("{} {text}", divider.repeat(left)),
        (left, right) => format!("{} {text} {}", divider.repeat(left), divider.repeat(right)),
    }
}

#[cfg(test)]
mod div_tests {
    use super::*;

    #[test]
    fn the_command_definition_is_valid() {
        use clap::CommandFactory;
        Args::command().debug_assert();
    }

    #[test]
    fn test_centered_divider_empty() {
        let count = 12usize;
        let result = format_centered_divider("", count, '%');
        assert_eq!(result, "%".repeat(count));
        let count = 13usize;
        let result = format_centered_divider("", count, '#');
        assert_eq!(result, "#".repeat(count));
        let count = 14usize;
        let result = format_centered_divider("", count, '-');
        assert_eq!(result, "-".repeat(count));
        let count = 100usize;
        let result = format_centered_divider("", count, '/');
        assert_eq!(result, "/".repeat(count));
    }

    #[test]
    fn test_centered_divider_basic() {
        let result = format_centered_divider("hello", 10, '%');
        assert_eq!(result, "% HELLO %%");
        let result = format_centered_divider("HELLO", 11, '%');
        assert_eq!(result, "%% HELLO %%");
        let result = format_centered_divider("Hello", 12, '%');
        assert_eq!(result, "%% HELLO %%%");
        let result = format_centered_divider("Hello", 20, '%');
        assert_eq!(result, "%%%%%% HELLO %%%%%%%");
    }

    #[test]
    fn test_centered_divider_no_text() {
        let result = format_centered_divider("", 8, '#');
        assert_eq!(result, "########");
    }

    #[test]
    fn test_centered_divider_long_text() {
        let result = format_centered_divider("This is a long text", 10, '%');
        assert_eq!(result, "THIS IS A LONG TEXT");
        let result = format_centered_divider("this is a long text", 40, '%');
        assert_eq!(result, "%%%%%%%%% THIS IS A LONG TEXT %%%%%%%%%%");
    }

    #[test]
    fn test_aligned_divider_basic() {
        let result = format_aligned_divider("Hello", 10, '%', 1);
        assert_eq!(result, "% HELLO %%");
    }

    #[test]
    fn test_aligned_divider_no_text() {
        let result = format_aligned_divider("", 10, '%', 1);
        assert_eq!(result, "%%%%%%%%%%");
    }

    #[test]
    fn test_aligned_divider_with_padding() {
        let result = format_aligned_divider("Text", 13, '-', 3);
        assert_eq!(result, "--- TEXT ----");
        let result = format_aligned_divider("Text2", 13, '-', 3);
        assert_eq!(result, "--- TEXT2 ---");
        let result = format_aligned_divider("Textmore", 13, '-', 3);
        assert_eq!(result, "--- TEXTMORE");

        let result = format_aligned_divider("something", 29, '#', 9);
        assert_eq!(result, "######### SOMETHING #########");
        let result = format_aligned_divider("another", 29, '#', 9);
        assert_eq!(result, "######### ANOTHER ###########");
        let result = format_aligned_divider("text", 29, '#', 9);
        assert_eq!(result, "######### TEXT ##############");
    }
}

#[cfg(test)]
mod test_divider_lines {
    use super::*;

    #[test]
    fn aligned_text_that_grows_when_uppercased_does_not_panic() {
        let texts = vec!["ßßßßßßßß".to_string(), "a".to_string()];

        let lines = divider_lines(&texts, 20, '=', true);

        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("SSSSSSSSSSSSSSSS"), "{lines:?}");
        assert_eq!(lines[1].chars().count(), 20, "{lines:?}");
    }

    fn texts(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn no_texts_gives_one_plain_divider() {
        assert_eq!(divider_lines(&[], 10, '%', false), vec!["%".repeat(10)]);
        assert_eq!(divider_lines(&[], 6, '-', true), vec!["-".repeat(6)]);
    }

    #[test]
    fn each_text_is_centered_without_align() {
        let lines = divider_lines(&texts(&["one", "three"]), 13, '#', false);

        assert_eq!(lines, vec!["#### ONE ####", "### THREE ###"]);
    }

    #[test]
    fn aligned_texts_start_at_the_same_column() {
        let lines = divider_lines(&texts(&["one", "three"]), 13, '#', true);

        assert_eq!(lines, vec!["### ONE #####", "### THREE ###"]);
    }

    #[test]
    fn align_with_a_single_text_centers_it() {
        let lines = divider_lines(&texts(&["one"]), 13, '#', true);

        assert_eq!(lines, vec!["#### ONE ####"]);
    }

    #[test]
    fn parses_texts_and_options_from_the_command_line() {
        let args = Args::try_parse_from(["div", "-l", "20", "-c", "=", "-a", "first", "second"])
            .expect("arguments should parse");

        assert_eq!(args.text, texts(&["first", "second"]));
        assert_eq!(args.length, 20);
        assert_eq!(args.character, '=');
        assert!(args.align);
    }

    #[test]
    fn exact_message_width_omits_blank_padding_and_aligned_padding_is_clamped() {
        assert_eq!(format_centered_divider("hello", 7, '%'), "HELLO");
        let clamped = format_aligned_divider("Textmore", 13, '-', 10);
        assert_eq!(clamped, "--- TEXTMORE");

        let lines = divider_lines(&texts(&["very long header text", "short", "longer"]), 15, '#', true);
        assert_eq!(lines[0], "VERY LONG HEADER TEXT");
        assert_eq!(lines[1], "### SHORT #####");
        assert_eq!(lines[2], "### LONGER ####");
    }
}
