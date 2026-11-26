//! Coloured diff rendering for the `cli-tools` binaries.
//!
//! Holds the character level diff used to show a rename as an aligned pair of lines,
//! and the line level diff used to show the changes a formatter would make to a file.
//! The builders return strings so a caller can buffer the output,
//! and the printing helpers are kept separate from them.

use std::cmp::Ordering;

use colored::Colorize;
use difference::{Changeset, Difference};

/// Create a coloured diff for the given strings.
pub fn color_diff(old: &str, new: &str, stacked: bool) -> (String, String) {
    let changeset = Changeset::new(old, new, "");
    let mut old_diff = String::new();
    let mut new_diff = String::new();

    if stacked {
        // Track visible character columns across diffs up to the first matching sequence
        // so multi-byte UTF-8 prefixes and repeated substrings still align visually.
        let mut old_column = 0usize;
        let mut new_column = 0usize;
        for diff in &changeset.diffs {
            match diff {
                Difference::Same(segment) => {
                    let segment_len = segment.chars().count();
                    if !segment.chars().all(char::is_whitespace) && segment_len >= 3 {
                        match old_column.cmp(&new_column) {
                            Ordering::Greater => {
                                new_diff = " ".repeat(old_column.saturating_sub(new_column));
                            }
                            Ordering::Less => {
                                old_diff = " ".repeat(new_column.saturating_sub(old_column));
                            }
                            Ordering::Equal => {}
                        }
                        break;
                    }
                    old_column += segment_len;
                    new_column += segment_len;
                }
                Difference::Rem(segment) => {
                    old_column += segment.chars().count();
                }
                Difference::Add(segment) => {
                    new_column += segment.chars().count();
                }
            }
        }
    }

    for diff in changeset.diffs {
        match diff {
            Difference::Same(ref x) => {
                old_diff.push_str(x);
                new_diff.push_str(x);
            }
            Difference::Add(ref x) => {
                if x.chars().all(char::is_whitespace) {
                    new_diff.push_str(&x.on_green().to_string());
                } else {
                    new_diff.push_str(&x.green().to_string());
                }
            }
            Difference::Rem(ref x) => {
                if x.chars().all(char::is_whitespace) {
                    old_diff.push_str(&x.on_red().to_string());
                } else {
                    old_diff.push_str(&x.red().to_string());
                }
            }
        }
    }

    (old_diff, new_diff)
}

/// Print a stacked diff of the changes.
pub fn show_diff(old: &str, new: &str) {
    let (old_diff, new_diff) = color_diff(old, new, true);
    println!("{old_diff}");
    if old_diff != new_diff {
        println!("{new_diff}");
    }
}

/// Lines of a line based diff between the original and fixed text, including the file header.
#[must_use]
pub fn diff_lines(path: &str, original: &str, fixed: &str) -> Vec<String> {
    let mut lines = vec![format!("--- {path}").bold().to_string()];
    let changeset = Changeset::new(original, fixed, "\n");
    for difference in &changeset.diffs {
        match difference {
            Difference::Same(_) => {}
            Difference::Rem(removed) => {
                lines.extend(removed.split('\n').map(|line| format!("- {line}").red().to_string()));
            }
            Difference::Add(added) => {
                lines.extend(added.split('\n').map(|line| format!("+ {line}").green().to_string()));
            }
        }
    }
    lines
}

#[cfg(test)]
mod test_color_diff {
    use super::*;

    #[test]
    fn identical_strings() {
        let (old, new) = color_diff("hello", "hello", false);
        assert_eq!(old, "hello");
        assert_eq!(new, "hello");
    }

    #[test]
    fn completely_different_strings() {
        let (old, new) = color_diff("abc", "xyz", false);
        assert!(old.contains("abc"));
        assert!(new.contains("xyz"));
    }

    #[test]
    fn partial_change() {
        let (old, new) = color_diff("hello world", "hello there", false);
        assert!(old.contains("hello"));
        assert!(new.contains("hello"));
    }

    #[test]
    fn stacked_mode() {
        let (old, new) = color_diff("prefix.name", "different.name", true);
        assert!(old.contains("name"));
        assert!(new.contains("name"));
    }

    #[test]
    fn empty_strings() {
        let (old, new) = color_diff("", "", false);
        assert_eq!(old, "");
        assert_eq!(new, "");
    }

    #[test]
    fn addition_only() {
        let (old, new) = color_diff("test", "testing", false);
        assert!(old.contains("test"));
        assert!(new.contains("test"));
    }

    #[test]
    fn removal_only() {
        let (old, new) = color_diff("testing", "test", false);
        assert!(old.contains("test"));
        assert!(new.contains("test"));
    }
}

#[cfg(test)]
mod test_show_diff {
    use super::*;

    #[test]
    fn show_diff_does_not_panic_on_identical() {
        // Just ensure it doesn't panic
        show_diff("same text", "same text");
    }

    #[test]
    fn show_diff_does_not_panic_on_different() {
        show_diff("old text", "new text");
    }

    #[test]
    fn show_diff_does_not_panic_on_empty() {
        show_diff("", "");
        show_diff("text", "");
        show_diff("", "text");
    }
}

#[cfg(test)]
mod test_diff_lines {
    use super::*;

    #[test]
    fn lists_removed_and_added_lines_with_a_header() {
        let lines = diff_lines(
            "a.rs",
            "// one sentence. And another.\n",
            "// one sentence.\n// And another.\n",
        );
        assert!(lines.first().is_some_and(|header| header.contains("--- a.rs")));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("- // one sentence. And another."))
        );
        assert!(lines.iter().any(|line| line.contains("+ // one sentence.")));
        assert!(lines.iter().any(|line| line.contains("+ // And another.")));
    }

    #[test]
    fn unchanged_text_produces_only_the_header() {
        let lines = diff_lines("a.rs", "// same\n", "// same\n");
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn includes_added_and_removed_blank_lines() {
        let added = diff_lines("a.rs", "first\nsecond\n", "first\n\nsecond\n");
        assert!(
            added.iter().any(|line| line.contains("+ ")),
            "expected added blank line in diff: {added:?}"
        );
        let removed = diff_lines("a.rs", "first\n\nsecond\n", "first\nsecond\n");
        assert!(
            removed.iter().any(|line| line.contains("- ")),
            "expected removed blank line in diff: {removed:?}"
        );
    }

    #[test]
    fn stacked_alignment_uses_character_columns_for_utf8() {
        colored::control::set_override(false);
        let (old, new) = color_diff("äää.shared.txt", "a.shared.txt", true);
        let old_pos = old.find(".shared.txt").unwrap();
        let new_pos = new.find(".shared.txt").unwrap();
        // Leading chars before ".shared.txt": "äää" is 3 chars, "  a" is 3 chars.
        assert_eq!(
            old.get(..old_pos).unwrap().chars().count(),
            new.get(..new_pos).unwrap().chars().count(),
            "old={old:?}, new={new:?}"
        );
    }
}
