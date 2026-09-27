//! The `div` binary.
//!
//! Prints a divider comment line with optional centered or aligned text.

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

#[derive(Parser)]
#[command(
    author,
    version,
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

/// Subcommands for div.
#[derive(Subcommand)]
enum DividerCommand {
    /// Generate shell completion script
    #[command(name = "completion")]
    Completion {
        /// Shell to generate completion for
        #[arg(value_enum)]
        shell: Shell,

        /// Install completion script to the shell's completion directory
        #[arg(short = 'I', long)]
        install: bool,
    },
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(DividerCommand::Completion { shell, install }) = &args.command {
        return cli_tools::generate_shell_completion(
            *shell,
            Args::command(),
            *install,
            args.verbose,
            env!("CARGO_BIN_NAME"),
        );
    }
    for line in divider_lines(&args.text, args.length, args.character, args.align) {
        println!("{line}");
    }
    Ok(())
}

/// Build the divider lines for the given texts.
///
/// No texts gives one plain divider.
/// With `align` and more than one text, the texts start at the same column, otherwise each one is centered.
fn divider_lines(texts: &[String], length: usize, character: char, align: bool) -> Vec<String> {
    if texts.is_empty() {
        return vec![format_centered_divider("", length, character)];
    }

    if texts.len() > 1 && align {
        let longest: usize = texts
            .iter()
            .map(|text| text.trim().chars().count())
            .max()
            .unwrap_or_default();

        let aligned_length: usize = length.saturating_sub(longest + 2) / 2;
        return texts
            .iter()
            .map(|text| format_aligned_divider(text, length, character, aligned_length))
            .collect();
    }

    texts
        .iter()
        .map(|text| format_centered_divider(text, length, character))
        .collect()
}

fn format_centered_divider(text: &str, count: usize, character: char) -> String {
    let text = text.trim().to_uppercase();
    let div = format!("{character}");
    if text.is_empty() {
        return div.repeat(count);
    }

    let message_length: usize = text.chars().count() + 2;
    if message_length > count {
        text
    } else {
        let total_padding: usize = count - message_length;
        let padding_side: usize = total_padding / 2;
        format_div_with_text(padding_side, &text, padding_side + total_padding % 2, &div)
    }
}

fn format_aligned_divider(text: &str, count: usize, character: char, padding_left: usize) -> String {
    let text = text.trim().to_uppercase();
    let div = format!("{character}");
    if text.is_empty() {
        return div.repeat(count);
    }

    let message_length: usize = text.chars().count() + 2;
    if message_length > count {
        text
    } else {
        let total_padding: usize = count - message_length;
        let padding_right: usize = total_padding - padding_left;
        format_div_with_text(padding_left, &text, padding_right, &div)
    }
}

fn format_div_with_text(num_left: usize, text: &str, num_right: usize, divider: &str) -> String {
    format!("{} {} {}", divider.repeat(num_left), text, divider.repeat(num_right))
}

#[cfg(test)]
mod div_tests {
    use super::*;

    #[test]
    fn the_command_definition_is_valid() {
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
        assert_eq!(result, "--- TEXTMORE ");

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
}
