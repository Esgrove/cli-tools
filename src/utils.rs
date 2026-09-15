//! Path and text helpers shared by the `cli-tools` binaries.
//!
//! Holds the conversions from paths to display strings with invalid Unicode handling,
//! the helpers that shorten a path for printing by making it relative to a root,
//! and the small text helpers several tools need,
//! such as reading the leading whitespace of a line and translating a glob into a regular expression.

use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use regex::Regex;

/// Convert `OsStr` to String with invalid Unicode handling.
pub fn os_str_to_string(name: &OsStr) -> String {
    name.to_str().map_or_else(
        || name.to_string_lossy().replace('\u{FFFD}', ""),
        std::string::ToString::to_string,
    )
}

/// Convert given path to string with invalid Unicode handling.
pub fn path_to_string(path: &Path) -> String {
    path.to_str().map_or_else(
        || path.to_string_lossy().to_string().replace('\u{FFFD}', ""),
        std::string::ToString::to_string,
    )
}

/// Convert given path to filename string with invalid Unicode handling.
#[must_use]
pub fn path_to_filename_string(path: &Path) -> String {
    os_str_to_string(path.file_name().unwrap_or_default())
}

/// Convert given path to file stem string with invalid Unicode handling.
#[must_use]
pub fn path_to_file_stem_string(path: &Path) -> String {
    os_str_to_string(path.file_stem().unwrap_or_default())
}

/// Convert given path to file extension lowercase string with invalid Unicode handling.
#[must_use]
pub fn path_to_file_extension_string(path: &Path) -> String {
    os_str_to_string(path.extension().unwrap_or_default()).to_lowercase()
}

/// Get the file extension from a string path and return it in lowercase.
#[must_use]
pub fn lowercase_extension(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
}

/// Get relative path and convert to string with invalid unicode handling.
#[must_use]
pub fn path_to_string_relative(path: &Path) -> String {
    path_to_string(&get_relative_path_from_current_working_directory(path))
}

/// Path relative to the given working directory for display.
///
/// Taking the directory as an argument keeps a run from asking the system for it once per file,
/// which is what [`path_to_string_relative`] does.
#[must_use]
pub fn path_to_string_relative_to(path: &Path, working_directory: Option<&Path>) -> String {
    path_to_string(
        working_directory
            .and_then(|directory| path.strip_prefix(directory).ok())
            .unwrap_or(path),
    )
}

/// Gets the relative path or filename from a full path based on a root directory.
///
/// If the full path is within the root directory, the function returns the relative path.
/// Otherwise, it returns just the filename. If the filename cannot be determined, the full path is returned.
///
/// ```rust
/// use std::path::Path;
/// use cli_tools::get_relative_path_or_filename;
///
/// let root = Path::new("/root/dir");
/// let full_path = root.join("subdir/file.txt");
/// let relative_path = get_relative_path_or_filename(&full_path, root);
/// assert_eq!(relative_path, "subdir/file.txt");
///
/// let outside_path = Path::new("/root/dir/another.txt");
/// let relative_or_filename = get_relative_path_or_filename(&outside_path, root);
/// assert_eq!(relative_or_filename, "another.txt");
/// ```
#[must_use]
pub fn get_relative_path_or_filename(full_path: &Path, root: &Path) -> String {
    if full_path == root {
        return os_str_to_string(full_path.file_name().unwrap_or_default());
    }
    full_path.strip_prefix(root).map_or_else(
        |_| {
            full_path.file_name().map_or_else(
                || full_path.display().to_string(),
                |name| name.to_string_lossy().to_string(),
            )
        },
        |relative_path| relative_path.display().to_string(),
    )
}

/// Convert the given path to be relative to the current working directory.
/// Returns the original path if the relative path cannot be created.
#[must_use]
pub fn get_relative_path_from_current_working_directory(path: &Path) -> PathBuf {
    env::current_dir().map_or_else(
        |_| path.to_path_buf(),
        |current_dir| path.strip_prefix(&current_dir).unwrap_or(path).to_path_buf(),
    )
}

/// Leading whitespace of a line.
#[must_use]
pub fn leading_whitespace(line: &str) -> &str {
    let trimmed = line.trim_start();
    line.get(..line.len() - trimmed.len()).unwrap_or_default()
}

/// Whether the text starts with the prefix, ignoring ASCII case.
#[must_use]
pub fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.as_bytes()
        .get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix.as_bytes()))
}

/// Convert a static string slice list into owned strings.
#[must_use]
pub fn strings_from(values: &[&str]) -> Vec<String> {
    values.iter().map(std::string::ToString::to_string).collect()
}

/// Convert an editorconfig glob into an anchored regular expression.
#[must_use]
pub fn glob_to_regex(glob: &str) -> Option<Regex> {
    let mut pattern = String::from("^");
    let chars: Vec<char> = glob.chars().collect();
    let mut index = 0;
    let mut in_braces = false;
    while let Some(&character) = chars.get(index) {
        match character {
            '*' if chars.get(index + 1) == Some(&'*') => {
                if chars.get(index + 2) == Some(&'/') {
                    pattern.push_str("(?:.*/)?");
                    index += 3;
                } else {
                    pattern.push_str(".*");
                    index += 2;
                }
                continue;
            }
            '*' => pattern.push_str("[^/]*"),
            '?' => pattern.push_str("[^/]"),
            '{' => {
                in_braces = true;
                pattern.push_str("(?:");
            }
            '}' if in_braces => {
                in_braces = false;
                pattern.push(')');
            }
            ',' if in_braces => pattern.push('|'),
            '[' | ']' => pattern.push(character),
            _ => pattern.push_str(&regex::escape(&character.to_string())),
        }
        index += 1;
    }
    pattern.push('$');
    Regex::new(&pattern).ok()
}

#[cfg(test)]
mod test_path_strings {
    use super::*;

    #[test]
    fn path_to_string_basic() {
        let path = Path::new("test/path/file.txt");
        assert_eq!(path_to_string(path), "test/path/file.txt");
    }

    #[test]
    fn path_to_filename_string_basic() {
        let path = Path::new("test/path/file.txt");
        assert_eq!(path_to_filename_string(path), "file.txt");
    }

    #[test]
    fn path_to_file_stem_string_basic() {
        let path = Path::new("test/path/file.txt");
        assert_eq!(path_to_file_stem_string(path), "file");
    }

    #[test]
    fn path_to_file_extension_string_basic() {
        let path = Path::new("test/path/file.TXT");
        assert_eq!(path_to_file_extension_string(path), "txt");
    }

    #[test]
    fn path_to_file_extension_string_no_extension() {
        let path = Path::new("README");
        assert_eq!(path_to_file_extension_string(path), "");
    }

    #[test]
    fn lowercase_extension_basic() {
        assert_eq!(lowercase_extension("test/path/file.TXT"), Some("txt".to_string()));
    }

    #[test]
    fn lowercase_extension_no_extension() {
        assert_eq!(lowercase_extension("README"), None);
    }

    #[test]
    fn os_str_to_string_basic() {
        let os_str = OsStr::new("test.txt");
        assert_eq!(os_str_to_string(os_str), "test.txt");
    }
}

#[cfg(test)]
mod test_relative_paths {
    use super::*;

    #[test]
    fn get_relative_path_or_filename_within_root() {
        let root = Path::new("/root/dir");
        let full_path = root.join("subdir/file.txt");
        let result = get_relative_path_or_filename(&full_path, root);
        assert_eq!(result, "subdir/file.txt");
    }

    #[test]
    fn get_relative_path_or_filename_same_as_root() {
        let root = Path::new("/root/dir");
        let result = get_relative_path_or_filename(root, root);
        assert_eq!(result, "dir");
    }

    #[test]
    fn get_relative_path_or_filename_outside_root() {
        let root = Path::new("/some/root");
        let path = Path::new("/different/path/file.txt");
        let result = get_relative_path_or_filename(path, root);
        assert_eq!(result, "file.txt");
    }

    #[test]
    fn get_relative_path_or_filename_no_filename() {
        let root = Path::new("/some/root");
        let path = Path::new("/");
        let result = get_relative_path_or_filename(path, root);
        // Should return the full path display when no filename
        assert!(!result.is_empty());
    }

    #[test]
    fn get_relative_path_from_cwd_returns_path() {
        let path = Path::new("some/relative/path.txt");
        let result = get_relative_path_from_current_working_directory(path);
        // Should return the same path since it's already relative
        assert_eq!(result, path);
    }

    #[test]
    fn path_to_string_relative_converts() {
        let path = Path::new("test/file.txt");
        let result = path_to_string_relative(path);
        assert!(result.contains("file.txt"));
    }

    #[test]
    fn relative_path_strips_current_working_directory() -> anyhow::Result<()> {
        let current_directory = env::current_dir()?;
        let absolute_path = current_directory.join("nested").join("file.txt");

        assert_eq!(
            get_relative_path_from_current_working_directory(&absolute_path),
            PathBuf::from("nested").join("file.txt")
        );
        Ok(())
    }

    #[test]
    fn a_path_in_the_given_working_directory_is_shown_relative() {
        let directory = Path::new("/root/dir");
        let path = directory.join("src").join("lib.rs");
        assert_eq!(
            path_to_string_relative_to(&path, Some(directory)),
            format!("src{}lib.rs", std::path::MAIN_SEPARATOR)
        );
    }

    #[test]
    fn a_path_outside_the_given_working_directory_is_shown_as_it_is() {
        let path = Path::new("/elsewhere/outside.rs");
        assert_eq!(
            path_to_string_relative_to(path, Some(Path::new("/root/dir"))),
            path_to_string(path)
        );
    }

    #[test]
    fn without_a_working_directory_the_path_is_shown_as_it_is() {
        let path = Path::new("/root/dir/src/lib.rs");
        assert_eq!(path_to_string_relative_to(path, None), path_to_string(path));
    }
}

#[cfg(test)]
mod test_text_helpers {
    use super::*;

    #[test]
    fn leading_whitespace_keeps_tabs_and_spaces() {
        assert_eq!(leading_whitespace("    text"), "    ");
        assert_eq!(leading_whitespace("\t\ttext"), "\t\t");
        assert_eq!(leading_whitespace("text"), "");
        assert_eq!(leading_whitespace("   "), "   ");
        assert_eq!(leading_whitespace(""), "");
    }

    #[test]
    fn starts_with_ignore_case_ignores_ascii_case_only() {
        assert!(starts_with_ignore_case("NOQA: E501", "noqa"));
        assert!(starts_with_ignore_case("noqa", "NOQA"));
        assert!(!starts_with_ignore_case("no", "noqa"));
        assert!(!starts_with_ignore_case("nope", "noqa"));
        assert!(starts_with_ignore_case("anything", ""));
    }

    #[test]
    fn strings_from_owns_every_value_in_order() {
        assert_eq!(strings_from(&["rs", "md"]), vec!["rs".to_string(), "md".to_string()]);
        assert!(strings_from(&[]).is_empty());
    }
}

#[cfg(test)]
mod test_glob_to_regex {
    use super::*;

    #[test]
    fn single_star_does_not_cross_directories() {
        let regex = glob_to_regex("*.rs").expect("glob should convert");
        assert!(regex.is_match("a.rs"));
        assert!(!regex.is_match("a/b.rs"));
        assert!(!regex.is_match("a.rst"));
    }

    #[test]
    fn double_star_crosses_directories() {
        let regex = glob_to_regex("**/*.rs").expect("glob should convert");
        assert!(regex.is_match("a/b.rs"));
        assert!(regex.is_match("a/b/c.rs"));
        assert!(regex.is_match("a.rs"));
    }

    #[test]
    fn a_double_star_inside_a_name_matches_anything() {
        let regex = glob_to_regex("a**b.rs").expect("glob should convert");
        assert!(regex.is_match("ab.rs"));
        assert!(regex.is_match("a-middle-b.rs"));
        assert!(!regex.is_match("a.rs"));
    }

    #[test]
    fn a_character_class_is_kept_as_it_is() {
        let regex = glob_to_regex("[abc].rs").expect("glob should convert");
        assert!(regex.is_match("a.rs"));
        assert!(regex.is_match("c.rs"));
        assert!(!regex.is_match("d.rs"));
    }

    #[test]
    fn question_mark_matches_single_character() {
        let regex = glob_to_regex("?.md").expect("glob should convert");
        assert!(regex.is_match("a.md"));
        assert!(!regex.is_match("ab.md"));
        assert!(!regex.is_match(".md"));
    }

    #[test]
    fn brace_alternation_matches_both_extensions() {
        let regex = glob_to_regex("*.{md,markdown}").expect("glob should convert");
        assert!(regex.is_match("a.md"));
        assert!(regex.is_match("a.markdown"));
        assert!(!regex.is_match("a.mdx"));
    }

    #[test]
    fn literal_characters_are_escaped() {
        let regex = glob_to_regex("a.b").expect("glob should convert");
        assert!(regex.is_match("a.b"));
        assert!(!regex.is_match("axb"));
    }
}
