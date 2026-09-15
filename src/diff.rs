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
        // Find the starting index of the first matching sequence for a nicer visual alignment.
        // For example:
        //   Constantine - Onde As Satisfaction (Club Tool).aif
        //        Darude - Onde As Satisfaction (Constantine Club Tool).aif
        // Instead of:
        //   Constantine - Onde As Satisfaction (Club Tool).aif
        //   Darude - Onde As Satisfaction (Constantine Club Tool).aif
        for diff in &changeset.diffs {
            if let Difference::Same(x) = diff {
                if x.chars().all(char::is_whitespace) || x.chars().count() < 3 {
                    continue;
                }

                // Add leading whitespace so that the first matching sequence lines up.
                if let (Some(old_index), Some(new_index)) = (old.find(x), new.find(x)) {
                    match old_index.cmp(&new_index) {
                        Ordering::Greater => {
                            new_diff = " ".repeat(old_index.saturating_sub(new_index));
                        }
                        Ordering::Less => {
                            old_diff = " ".repeat(new_index.saturating_sub(old_index));
                        }
                        Ordering::Equal => {}
                    }
                    break;
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
                lines.extend(removed.lines().map(|line| format!("- {line}").red().to_string()));
            }
            Difference::Add(added) => {
                lines.extend(added.lines().map(|line| format!("+ {line}").green().to_string()));
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
}
