use clap::Parser;

#[derive(Parser)]
#[command(
    author,
    version,
    name = env!("CARGO_BIN_NAME"),
    about = "Print divider comment with centered text"
)]
struct Args {
    /// Optional divider text(s)
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
}

fn main() {
    let args = Args::parse();
    if args.text.is_empty() {
        println!("{}", format_centered_divider("", args.length, args.character));
    } else if args.text.len() > 1 && args.align {
        let longest: usize = args
            .text
            .iter()
            .map(|s| s.trim().chars().count())
            .max()
            .unwrap_or_default();

        let aligned_length: usize = args.length.saturating_sub(longest + 2) / 2;
        for text in &args.text {
            println!(
                "{}",
                format_aligned_divider(text, args.length, args.character, aligned_length)
            );
        }
    } else {
        for text in &args.text {
            println!("{}", format_centered_divider(text, args.length, args.character));
        }
    }
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
