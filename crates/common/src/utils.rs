//! Path and text helpers shared by the `cli-tools` binaries.
//!
//! Holds the conversions from paths to display strings with invalid Unicode handling,
//! the helpers that shorten a path for printing by making it relative to a root,
//! the small text helpers several tools need,
//! such as reading the leading whitespace of a line and translating a glob into a regular expression,
//! and the lookup of executables on `PATH`.

use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use regex::Regex;
use walkdir::WalkDir;

/// Case-insensitive include and exclude matcher supporting both globs and substring patterns.
///
/// Patterns containing glob metacharacters (`*`, `?`, `[`, `{`) are compiled with [`glob_to_regex`].
/// Plain patterns without metacharacters match as case-insensitive substrings anywhere in the name.
/// Multiple `include` patterns are combined with OR, and any matching `exclude` pattern rejects the name.
#[derive(Debug, Default, Clone)]
pub struct NameFilter {
    include: Vec<Regex>,
    exclude: Vec<Regex>,
}

/// Configuration for collecting entries from a directory tree with [`collect_entries`].
#[derive(Debug, Clone, Copy)]
pub struct WalkOptions {
    /// Recurse into subdirectories when `true`.
    /// Otherwise visit only immediate children.
    pub recurse: bool,
    /// Optional explicit maximum depth override when recursing.
    pub max_depth: Option<usize>,
    /// Collect directories instead of regular files when `true`.
    pub include_directories: bool,
    /// Skip hidden entries and system directories below the walk root.
    pub skip_hidden_and_system: bool,
}

impl NameFilter {
    /// Compile include and exclude patterns into a [`NameFilter`].
    ///
    /// Empty or whitespace-only patterns are ignored.
    ///
    /// # Errors
    /// Returns an error if any glob pattern has invalid syntax.
    pub fn new(include: &[String], exclude: &[String]) -> Result<Self> {
        let include = compile_filter_patterns(include)?;
        let exclude = compile_filter_patterns(exclude)?;
        Ok(Self { include, exclude })
    }

    /// Return whether neither include nor exclude patterns are configured.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    /// Return whether `name` passes the include and exclude filters.
    #[must_use]
    pub fn matches_name(&self, name: &str) -> bool {
        if !self.include.is_empty() && !self.include.iter().any(|pattern| pattern.is_match(name)) {
            return false;
        }
        !self.exclude.iter().any(|pattern| pattern.is_match(name))
    }

    /// Return whether the file name component of `path` passes the include and exclude filters.
    #[must_use]
    pub fn matches_path(&self, path: &Path) -> bool {
        let name = path_to_filename_string(path);
        self.matches_name(&name)
    }
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            recurse: false,
            max_depth: None,
            include_directories: false,
            skip_hidden_and_system: true,
        }
    }
}

impl WalkOptions {
    /// Create walk options for files with the given recursion setting.
    #[must_use]
    pub const fn files(recurse: bool) -> Self {
        Self {
            recurse,
            max_depth: None,
            include_directories: false,
            skip_hidden_and_system: true,
        }
    }

    /// Create walk options for directories with the given recursion setting.
    #[must_use]
    pub const fn directories(recurse: bool) -> Self {
        Self {
            recurse,
            max_depth: None,
            include_directories: true,
            skip_hidden_and_system: true,
        }
    }
}

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
    let mut brace_depth: usize = 0;
    let mut in_class = false;
    let mut class_start = 0;
    let mut class_negated = false;

    while let Some(&character) = chars.get(index) {
        if in_class {
            let at_class_open = index == class_start;
            let at_negated_first = class_negated && index == class_start + 1;
            if at_class_open && (character == '!' || character == '^') {
                class_negated = true;
                pattern.push_str("^/");
            } else if character == ']' && !at_class_open && !at_negated_first {
                in_class = false;
                class_negated = false;
                pattern.push(']');
            } else if character == '\\' {
                let escaped = *chars.get(index + 1)?;
                pattern.push_str(&regex::escape(&escaped.to_string()));
                index += 2;
                continue;
            } else if matches!(character, '[' | ']' | '^' | '&' | '~')
                || (character == '-' && chars.get(index + 1) == Some(&'-'))
            {
                pattern.push('\\');
                pattern.push(character);
            } else {
                pattern.push(character);
            }
            index += 1;
            continue;
        }

        match character {
            '\\' => {
                let escaped = *chars.get(index + 1)?;
                pattern.push_str(&regex::escape(&escaped.to_string()));
                index += 2;
                continue;
            }
            '*' if chars.get(index + 1) == Some(&'*') => {
                if chars.get(index + 2) == Some(&'/') {
                    pattern.push_str("(?:(?s:.*)/)?");
                    index += 3;
                } else {
                    pattern.push_str("(?s:.*)");
                    index += 2;
                }
                continue;
            }
            '*' => pattern.push_str("[^/]*"),
            '?' => pattern.push_str("[^/]"),
            '{' => {
                brace_depth += 1;
                pattern.push_str("(?:");
            }
            '}' => {
                if brace_depth == 0 {
                    return None;
                }
                brace_depth -= 1;
                pattern.push(')');
            }
            ',' if brace_depth > 0 => pattern.push('|'),
            '[' => {
                in_class = true;
                class_start = index + 1;
                class_negated = false;
                pattern.push('[');
            }
            _ => pattern.push_str(&regex::escape(&character.to_string())),
        }
        index += 1;
    }

    if in_class || brace_depth > 0 {
        return None;
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
/// On Windows, extensions from `PATHEXT` (defaulting to `.exe`, `.cmd`, `.bat`, and `.com`) are checked.
#[must_use]
pub fn find_command(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        return find_executable_with_extensions(candidate);
    }
    let path_variable = env::var_os("PATH")?;
    for directory in env::split_paths(&path_variable) {
        let full = directory.join(name);
        if let Some(found) = find_executable_with_extensions(&full) {
            return Some(found);
        }
    }
    None
}

/// Resolve an executable path, checking Windows `PATHEXT` extensions before extensionless files.
#[cfg(windows)]
fn find_executable_with_extensions(base: &Path) -> Option<PathBuf> {
    let extensions: Vec<String> = env::var("PATHEXT").map_or_else(
        |_| vec![".exe".into(), ".cmd".into(), ".bat".into(), ".com".into()],
        |raw| {
            let parsed: Vec<String> = raw
                .split(';')
                .map(str::trim)
                .filter(|ext| !ext.is_empty())
                .map(str::to_ascii_lowercase)
                .collect();
            if parsed.is_empty() {
                vec![".exe".into(), ".cmd".into(), ".bat".into(), ".com".into()]
            } else {
                parsed
            }
        },
    );

    if base.extension().is_some() && is_executable_file(base) {
        return Some(base.to_path_buf());
    }
    let base_os = base.as_os_str();
    for extension in &extensions {
        let mut with_extension = base_os.to_os_string();
        with_extension.push(extension);
        let candidate = PathBuf::from(with_extension);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    is_executable_file(base).then(|| base.to_path_buf())
}

/// Resolve an executable path on non-Windows platforms.
#[cfg(not(windows))]
fn find_executable_with_extensions(base: &Path) -> Option<PathBuf> {
    is_executable_file(base).then(|| base.to_path_buf())
}

/// Count the regular files under a directory, recursively.
///
/// Symlinks are not followed, and entries that cannot be read are not counted.
#[must_use]
pub fn count_files(directory: &Path) -> usize {
    WalkDir::new(directory)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .count()
}

/// Resolve a list of CLI input paths to canonical absolute paths, deduplicated in order.
///
/// When `paths` is empty, resolves the current working directory.
/// When `paths` is non-empty, every entry must be non-empty and exist on disk.
///
/// # Errors
/// Returns an error if any path is empty, missing, or inaccessible.
pub fn resolve_input_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if paths.is_empty() {
        return Ok(vec![crate::resolve_input_path(None)?]);
    }
    let mut resolved = Vec::with_capacity(paths.len());
    let mut seen = HashSet::with_capacity(paths.len());
    for path in paths {
        let canonical = crate::resolve_required_input_path(path)?;
        if seen.insert(canonical.clone()) {
            resolved.push(canonical);
        }
    }
    Ok(resolved)
}

/// Normalize file extensions by trimming whitespace, stripping leading dots, lowercasing, and deduplicating.
#[must_use]
pub fn normalize_extensions<I, S>(extensions: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for raw in extensions {
        let normalized = raw.as_ref().trim().trim_start_matches('.').to_ascii_lowercase();
        if !normalized.is_empty() && seen.insert(normalized.clone()) {
            result.push(normalized);
        }
    }
    result
}

/// Return the platform-specific local data directory for `cli-tools`.
///
/// - macOS: `~/Library/Application Support/cli-tools`
/// - Windows: `%LOCALAPPDATA%\cli-tools`
/// - Linux: `~/.local/share/cli-tools`
///
/// # Errors
/// Returns an error if the operating system local data directory cannot be determined.
pub fn data_dir() -> Result<PathBuf> {
    let base = dirs::data_local_dir().context("Failed to determine local data directory")?;
    Ok(base.join("cli-tools"))
}

/// Return the path to a named file inside the `cli-tools` local data directory.
///
/// # Errors
/// Returns an error if the local data directory cannot be determined.
pub fn data_file_path(filename: &str) -> Result<PathBuf> {
    Ok(data_dir()?.join(filename))
}

/// Map a byte length in `original.to_lowercase()` back to the corresponding byte offset in `original`.
///
/// Unicode lowercasing can expand or shrink UTF-8 byte lengths (for example `'İ'` expands from 2 bytes to 3).
/// Walking characters in lockstep avoids slicing `original` at an invalid UTF-8 boundary or wrong offset.
#[must_use]
pub fn original_end_for_lowercase_prefix(original: &str, lowercase_prefix_len: usize) -> usize {
    let mut lower_consumed = 0usize;
    for (byte_index, character) in original.char_indices() {
        if lower_consumed >= lowercase_prefix_len {
            return byte_index;
        }
        lower_consumed += character.to_lowercase().map(char::len_utf8).sum::<usize>();
    }
    original.len()
}

/// Strip ANSI CSI escape sequences from `text`.
#[must_use]
pub fn strip_ansi_codes(text: &str) -> String {
    let mut cleaned = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\x1b' && chars.peek() == Some(&'[') {
            let _ = chars.next();
            for next_character in chars.by_ref() {
                if ('@'..='~').contains(&next_character) {
                    break;
                }
            }
        } else {
            cleaned.push(character);
        }
    }
    cleaned
}

/// Collect regular files or directories under `root` without following symlinks.
///
/// Walk errors are passed to `on_error` rather than silently discarded or aborting the entire scan.
pub fn collect_entries(root: &Path, options: &WalkOptions, mut on_error: impl FnMut(walkdir::Error)) -> Vec<PathBuf> {
    let mut walker = WalkDir::new(root).follow_links(false).min_depth(1);
    if !options.recurse {
        walker = walker.max_depth(1);
    } else if let Some(max_depth) = options.max_depth {
        walker = walker.max_depth(max_depth);
    }

    let mut entries = Vec::new();
    let skip_hidden = options.skip_hidden_and_system;
    for entry_result in walker
        .into_iter()
        .filter_entry(move |entry| !skip_hidden || !crate::should_skip_entry(entry))
    {
        match entry_result {
            Ok(entry) => {
                let file_type = entry.file_type();
                if options.include_directories {
                    if file_type.is_dir() {
                        entries.push(entry.into_path());
                    }
                } else if file_type.is_file() {
                    entries.push(entry.into_path());
                }
            }
            Err(error) => on_error(error),
        }
    }
    entries
}

/// Compile case-insensitive include or exclude patterns into regular expressions.
fn compile_filter_patterns(patterns: &[String]) -> Result<Vec<Regex>> {
    let mut compiled = Vec::with_capacity(patterns.len());
    for raw in patterns {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let has_glob_meta = trimmed.contains(['*', '?', '[', '{']);
        let regex_source = if has_glob_meta {
            let Some(regex) = glob_to_regex(trimmed) else {
                bail!("Invalid glob pattern: '{trimmed}'");
            };
            format!("(?i){}", regex.as_str())
        } else {
            format!("(?i){}", regex::escape(trimmed))
        };
        let regex = Regex::new(&regex_source).with_context(|| format!("Invalid filter pattern: '{trimmed}'"))?;
        compiled.push(regex);
    }
    Ok(compiled)
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
mod test_unification_helpers {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn name_filter_supports_globs_substrings_and_or_includes() {
        let filter = NameFilter::new(
            &["*.mp4".to_string(), "clip".to_string()],
            &["*sample*".to_string(), "draft".to_string()],
        )
        .unwrap();

        assert!(filter.matches_name("Movie.MP4"));
        assert!(filter.matches_name("My.Clip.mkv"));
        assert!(!filter.matches_name("Other.mkv"));
        assert!(!filter.matches_name("Movie.Sample.mp4"));
        assert!(!filter.matches_name("Clip.Draft.mkv"));
        assert!(filter.matches_path(Path::new("/Users/draft/videos/Movie.mp4")));
    }

    #[test]
    fn name_filter_rejects_invalid_glob() {
        assert!(NameFilter::new(&["[unclosed".to_string()], &[]).is_err());
    }

    #[test]
    fn normalize_extensions_strips_dots_lowercases_and_deduplicates() {
        let normalized = normalize_extensions([" .MP4 ", "mkv", ".mp4", "", "  ", ".MKV"]);
        assert_eq!(normalized, vec!["mp4", "mkv"]);
    }

    #[test]
    fn original_end_for_lowercase_prefix_handles_expanding_unicode() {
        let original = "İstanbul.Video";
        let lower_prefix_len = "i\u{307}st".len();
        let end = original_end_for_lowercase_prefix(original, lower_prefix_len);
        assert_eq!(original.get(..end), Some("İst"));
    }

    #[test]
    fn strip_ansi_codes_removes_color_sequences() {
        let colored = "\x1b[32mgreen\x1b[0m plain \x1b[1;31mred\x1b[0m";
        assert_eq!(strip_ansi_codes(colored), "green plain red");
    }

    #[test]
    fn resolve_input_paths_deduplicates_preserving_order() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();

        let resolved = resolve_input_paths(&[b.clone(), a.clone(), b]).unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0], dunce::canonicalize(dir.path().join("b")).unwrap());
        assert_eq!(resolved[1], dunce::canonicalize(&a).unwrap());
    }

    #[test]
    fn collect_entries_skips_hidden_and_symlinks() {
        let dir = TempDir::new().unwrap();
        let visible = dir.path().join("movie.mp4");
        let hidden = dir.path().join(".secret.mp4");
        let nested_dir = dir.path().join("sub");
        let nested_file = nested_dir.join("deep.mp4");
        std::fs::write(&visible, b"a").unwrap();
        std::fs::write(&hidden, b"b").unwrap();
        std::fs::create_dir(&nested_dir).unwrap();
        std::fs::write(&nested_file, b"c").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&visible, dir.path().join("link.mp4")).unwrap();

        let shallow = collect_entries(dir.path(), &WalkOptions::files(false), |_| {});
        assert_eq!(shallow, vec![visible.clone()]);

        let mut deep = collect_entries(dir.path(), &WalkOptions::files(true), |_| {});
        deep.sort();
        let mut expected = vec![visible, nested_file];
        expected.sort();
        assert_eq!(deep, expected);
    }
}

#[cfg(test)]
mod test_glob_escapes_and_boundaries {
    use super::*;

    #[test]
    fn backslash_escapes_glob_metacharacters() {
        let regex = glob_to_regex(r"file\*name\?.txt").expect("valid glob");
        assert!(regex.is_match("file*name?.txt"));
        assert!(!regex.is_match("fileXnameY.txt"));
    }

    #[test]
    fn negated_character_class_does_not_cross_directory_separator() {
        let regex = glob_to_regex("[!a]*.txt").expect("valid glob");
        assert!(regex.is_match("b.txt"));
        assert!(!regex.is_match("/b.txt"));
    }

    #[test]
    fn character_class_escapes_regex_set_operators() {
        let regex = glob_to_regex("[a&&b].txt").expect("valid glob");
        assert!(regex.is_match("&.txt"));
        assert!(regex.is_match("a.txt"));
    }

    #[test]
    fn unmatched_closing_brace_is_rejected() {
        assert!(glob_to_regex("foo}.txt").is_none());
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

    #[test]
    fn character_ranges_negation_and_class_literals_work() {
        let range = glob_to_regex("[a-z].rs").expect("range glob");
        assert!(range.is_match("m.rs"));
        assert!(!range.is_match("M.rs"));
        assert!(!range.is_match("-.rs"));

        let negated_bang = glob_to_regex("[!abc].rs").expect("negated ! glob");
        assert!(negated_bang.is_match("d.rs"));
        assert!(!negated_bang.is_match("a.rs"));

        let negated_caret = glob_to_regex("[^abc].rs").expect("negated ^ glob");
        assert!(negated_caret.is_match("d.rs"));
        assert!(!negated_caret.is_match("b.rs"));

        let class_literals = glob_to_regex("[*?,{].txt").expect("literal glob chars inside class");
        assert!(class_literals.is_match("*.txt"));
        assert!(class_literals.is_match(",.txt"));
        assert!(!class_literals.is_match("a.txt"));
    }

    #[test]
    fn nested_braces_match_all_alternatives() {
        let regex = glob_to_regex("*.{rs,{md,txt}}").expect("nested brace glob");
        assert!(regex.is_match("main.rs"));
        assert!(regex.is_match("notes.md"));
        assert!(regex.is_match("readme.txt"));
        assert!(!regex.is_match("image.png"));
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
