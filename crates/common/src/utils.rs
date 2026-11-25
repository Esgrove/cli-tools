//! Path and text helpers shared by the `cli-tools` binaries.
//!
//! Holds the conversions from paths to display strings with invalid Unicode handling,
//! the helpers that shorten a path for printing by making it relative to a root,
//! the small text helpers several tools need,
//! such as reading the leading whitespace of a line and translating a glob into a regular expression,
//! and the lookup of executables on `PATH`.

use std::env;
use std::ffi::OsStr;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
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
/// use cli_tools_common::get_relative_path_or_filename;
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

/// Remove repeated values, keeping the first occurrence of each in the original order.
#[must_use]
pub fn unique_in_order<I>(values: I) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let mut seen = std::collections::HashSet::new();
    values.into_iter().filter(|value| seen.insert(value.clone())).collect()
}

/// Return `path` relative to `root`, using filesystem identity when the textual prefixes differ.
///
/// The root itself gives an empty path, and a path outside the root gives `None`.
#[must_use]
pub fn relative_path_within(path: &Path, root: &Path) -> Option<PathBuf> {
    if crate::paths_refer_to_same_file(path, root) {
        return Some(PathBuf::new());
    }

    if let Ok(relative_path) = path.strip_prefix(root) {
        return Some(relative_path.to_path_buf());
    }

    path.ancestors()
        .find(|ancestor| crate::paths_refer_to_same_file(ancestor, root))
        .and_then(|matching_root| path.strip_prefix(matching_root).ok())
        .map(Path::to_path_buf)
}

/// Return the root that most closely contains `path` textually, preferring the deepest matching root.
#[must_use]
pub fn nearest_containing_root<'a>(path: &Path, roots: &'a [PathBuf]) -> Option<&'a Path> {
    roots
        .iter()
        .filter_map(|root| path.strip_prefix(root).ok().map(|relative| (root.as_path(), relative)))
        .min_by_key(|(_, relative)| relative.components().count())
        .map(|(root, _)| root)
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

/// Return whether `name` resolves to an executable on `PATH`, or is an executable path itself.
#[must_use]
pub fn command_exists(name: &str) -> bool {
    find_command(name).is_some()
}

/// Locate an executable by name on `PATH`, or check it directly when the name contains a path separator.
///
/// On Windows the `.exe`, `.cmd`, and `.bat` extensions are also tried.
#[must_use]
pub fn find_command(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        return is_executable_file(candidate).then(|| candidate.to_path_buf());
    }
    let path_variable = env::var_os("PATH")?;
    for directory in env::split_paths(&path_variable) {
        let full = directory.join(name);
        if is_executable_file(&full) {
            return Some(full);
        }
        #[cfg(windows)]
        {
            for extension in [".exe", ".cmd", ".bat"] {
                let with_extension = directory.join(format!("{name}{extension}"));
                if is_executable_file(&with_extension) {
                    return Some(with_extension);
                }
            }
        }
    }
    None
}

/// Count the regular files under a directory, recursively.
///
/// Symlinks are not followed, and entries that cannot be read are not counted.
#[must_use]
pub fn count_files(directory: &Path) -> usize {
    walkdir::WalkDir::new(directory)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .count()
}

/// Whether the path is a regular file with an executable permission bit on Unix,
/// or any regular file elsewhere.
fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
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
        assert_ne!(result, "");
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
        assert_eq!(strings_from(&[]), [] as [String; 0]);
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

#[cfg(test)]
mod test_find_command {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn finds_shell_on_path() {
        assert!(command_exists("sh"));
        assert!(find_command("sh").is_some_and(|path| path.is_absolute()));
    }

    #[test]
    fn rejects_missing_command_and_directories() {
        assert!(!command_exists("cli-tools-definitely-missing-command"));
        let directory = env::temp_dir();
        assert!(find_command(&directory.to_string_lossy()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn checks_direct_paths_for_the_executable_bit() {
        let temp = tempfile::tempdir().expect("should create temp dir");
        let script = temp.path().join("tool");
        std::fs::write(&script, "#!/bin/sh\n").expect("should write");
        assert!(find_command(&script.to_string_lossy()).is_none());
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("should chmod");
        assert_eq!(find_command(&script.to_string_lossy()), Some(script));
    }
}

#[cfg(test)]
mod test_count_files {
    use super::*;

    /// Create an empty file, creating its parent directories.
    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().expect("path should have a parent")).expect("should create parents");
        std::fs::write(path, "").expect("should write file");
    }

    #[test]
    fn counts_files_in_nested_directories() {
        let temp = tempfile::tempdir().expect("should create temp dir");
        touch(&temp.path().join("a.txt"));
        touch(&temp.path().join("nested/b.txt"));
        touch(&temp.path().join("nested/deeper/c.txt"));
        std::fs::create_dir(temp.path().join("empty")).expect("should create directory");

        assert_eq!(count_files(temp.path()), 3);
    }

    #[test]
    fn missing_directory_has_no_files() {
        let temp = tempfile::tempdir().expect("should create temp dir");
        assert_eq!(count_files(&temp.path().join("missing")), 0);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_counted() {
        let temp = tempfile::tempdir().expect("should create temp dir");
        touch(&temp.path().join("outside/target.txt"));
        touch(&temp.path().join("inside/real.txt"));
        std::os::unix::fs::symlink(temp.path().join("outside"), temp.path().join("inside/linked"))
            .expect("should create symlink");

        assert_eq!(count_files(&temp.path().join("inside")), 1);
    }
}

#[cfg(test)]
mod test_unique_in_order {
    use super::*;

    #[test]
    fn keeps_the_first_occurrence_in_order() {
        let values = ["b", "a", "b", "c", "a"].map(String::from);
        assert_eq!(unique_in_order(values), ["b", "a", "c"]);
    }

    #[test]
    fn compares_values_exactly() {
        let values = ["Name", "name", "name "].map(String::from);
        assert_eq!(unique_in_order(values), ["Name", "name", "name "]);
    }

    #[test]
    fn empty_input_gives_empty_output() {
        assert_eq!(unique_in_order(Vec::new()), Vec::<String>::new());
    }
}

#[cfg(test)]
mod test_root_relative_paths {
    use super::*;

    #[test]
    fn relative_path_within_strips_the_root() {
        let path = Path::new("/media/root/shows/file.mp4");
        assert_eq!(
            relative_path_within(path, Path::new("/media/root")),
            Some(PathBuf::from("shows/file.mp4"))
        );
    }

    #[test]
    fn relative_path_within_gives_none_outside_the_root() {
        assert_eq!(
            relative_path_within(Path::new("/other/file.mp4"), Path::new("/media/root")),
            None
        );
    }

    #[test]
    fn relative_path_within_follows_a_symlinked_root() {
        let temp = tempfile::tempdir().expect("should create temp dir");
        let real_root = temp.path().join("real");
        std::fs::create_dir_all(real_root.join("nested")).expect("should create directories");
        let canonical_root = real_root.canonicalize().expect("should canonicalize");

        assert_eq!(
            relative_path_within(&canonical_root.join("nested"), &real_root),
            Some(PathBuf::from("nested"))
        );
        assert_eq!(relative_path_within(&real_root, &real_root), Some(PathBuf::new()));
    }

    #[test]
    fn nearest_containing_root_prefers_the_deepest_root() {
        let roots = [PathBuf::from("/media"), PathBuf::from("/media/shows")];
        assert_eq!(
            nearest_containing_root(Path::new("/media/shows/a/file.mp4"), &roots),
            Some(Path::new("/media/shows"))
        );
        assert_eq!(
            nearest_containing_root(Path::new("/media/movies/file.mp4"), &roots),
            Some(Path::new("/media"))
        );
        assert_eq!(nearest_containing_root(Path::new("/other/file.mp4"), &roots), None);
    }
}
