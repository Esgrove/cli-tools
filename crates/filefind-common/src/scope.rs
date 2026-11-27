//! Shared path ownership, inclusion, exclusion, and UTF-8 validation rules for filefind.
//!
//! All index writers (the daemon scanner, file watcher, CLI mover, and external tool updater)
//! share these rules so every writer agrees on which volume owns a path,
//! which paths fall inside configured roots, and which entries are excluded.

use std::cmp::Reverse;
use std::path::Path;

use anyhow::{Result, bail};

use crate::config::DaemonConfig;
use crate::extract_drive_letter_from_str;
use crate::types::IndexedVolume;

/// Combined root and exclusion policy shared across all filefind index writers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexScope {
    /// Configured index roots. An empty list accepts any indexed volume.
    pub roots: Vec<String>,
    /// Directory and path exclusions from `[daemon].exclude`.
    pub exclude: Vec<String>,
    /// Filename glob exclusions from `[daemon].exclude_patterns`.
    pub exclude_patterns: Vec<String>,
}

impl IndexScope {
    /// Create a scope with explicit roots, directory exclusions, and filename glob patterns.
    #[must_use]
    pub const fn new(roots: Vec<String>, exclude: Vec<String>, exclude_patterns: Vec<String>) -> Self {
        Self {
            roots,
            exclude,
            exclude_patterns,
        }
    }

    /// Build an [`IndexScope`] from the daemon section of a user configuration.
    #[must_use]
    pub fn from_daemon_config(config: &DaemonConfig) -> Self {
        Self {
            roots: config.paths.clone(),
            exclude: config.exclude.clone(),
            exclude_patterns: config.exclude_patterns.clone(),
        }
    }

    /// Return whether `path` is excluded by hidden-component rules, directory exclusions, or filename globs.
    #[must_use]
    pub fn is_excluded(&self, path: &str, matched_root: Option<&str>) -> bool {
        has_hidden_component_within_root(path, matched_root)
            || matches_exclude_rules(path, &self.exclude)
            || matches_exclude_globs(path, &self.exclude_patterns)
    }

    /// Return whether `path` is inside configured roots (when non-empty) and not excluded.
    #[must_use]
    pub fn is_included(&self, path: &str, volume_mount: Option<&str>) -> bool {
        let matched_root = if self.roots.is_empty() {
            volume_mount
        } else {
            let Some(root) = self.most_specific_root(path) else {
                return false;
            };
            Some(root)
        };
        !self.is_excluded(path, matched_root)
    }

    /// Return the most specific configured root containing `path`, if any.
    #[must_use]
    pub fn most_specific_root(&self, path: &str) -> Option<&str> {
        self.roots
            .iter()
            .filter(|root| is_path_within_root(path, root))
            .max_by_key(|root| (root_specificity(root), root.len()))
            .map(String::as_str)
    }
}

/// Convert `path` into a UTF-8 string slice, rejecting unrepresentable Unix byte sequences.
///
/// The index schema stores paths as UTF-8 `TEXT`, so callers skip and report non-UTF-8 paths
/// rather than acting on a lossy replacement key.
///
/// # Errors
/// Returns an error if `path` contains invalid UTF-8 bytes.
pub fn path_to_utf8(path: &Path) -> Result<&str> {
    match path.to_str() {
        Some(text) => Ok(text),
        None => bail!(
            "Path is not valid UTF-8 and cannot be stored in the index: {}",
            path.display()
        ),
    }
}

/// Return whether `path` uses a Windows drive letter or UNC prefix.
#[must_use]
pub fn is_windows_style_path(path: &str) -> bool {
    path.starts_with(r"\\") || extract_drive_letter_from_str(path).is_some()
}

/// Return whether `path` equals `root` or lies strictly inside it on a component boundary.
///
/// Windows-style roots compare case-insensitively and treat `/` and `\` equivalently.
/// Unix roots compare case-sensitively and never treat a literal `\` as a directory separator.
#[must_use]
pub fn is_path_within_root(path: &str, root: &str) -> bool {
    if root.is_empty() {
        return false;
    }

    if is_windows_style_path(root) || is_windows_style_path(path) {
        let normalized_path = path.replace('/', "\\").to_lowercase();
        let normalized_root = root.replace('/', "\\").to_lowercase();
        let trimmed_root = normalized_root.trim_end_matches('\\');
        if trimmed_root.is_empty() {
            return false;
        }
        return normalized_path == trimmed_root
            || normalized_path
                .strip_prefix(trimmed_root)
                .is_some_and(|remainder| remainder.starts_with('\\'));
    }

    if cfg!(windows) {
        let normalized_path = path.replace('/', "\\").to_lowercase();
        let normalized_root = root.replace('/', "\\").to_lowercase();
        let trimmed_root = normalized_root.trim_end_matches('\\');
        if trimmed_root.is_empty() {
            return normalized_path.starts_with('\\');
        }
        return normalized_path == trimmed_root
            || normalized_path
                .strip_prefix(trimmed_root)
                .is_some_and(|remainder| remainder.starts_with('\\'));
    }

    let trimmed_root = root.trim_end_matches('/');
    if trimmed_root.is_empty() {
        return root.starts_with('/') && path.starts_with('/');
    }
    path == trimmed_root
        || path
            .strip_prefix(trimmed_root)
            .is_some_and(|remainder| remainder.starts_with('/'))
}

/// Rank a root's specificity as `(component_count, trimmed_length)` for deterministic ownership.
///
/// Deeper roots always win over shallower ancestors
/// (for example `/data/sub` over `/data` over `/`, and `C:\Users` over `C:`).
#[must_use]
pub fn root_specificity(root: &str) -> (usize, usize) {
    if is_windows_style_path(root) || cfg!(windows) {
        let normalized = root.replace('/', "\\");
        let trimmed = normalized.trim_end_matches('\\');
        if trimmed.is_empty() {
            return (0, 0);
        }
        let components = trimmed.split('\\').filter(|part| !part.is_empty()).count();
        return (components, trimmed.len());
    }

    let trimmed = root.trim_end_matches('/');
    if trimmed.is_empty() {
        return (0, 0);
    }
    let components = trimmed.split('/').filter(|part| !part.is_empty()).count();
    (components, trimmed.len())
}

/// Find the most specific volume ID and mount point containing `path`.
///
/// Overlapping roots resolve deterministically to the deepest mount point,
/// breaking exact ties by the lowest volume ID.
#[must_use]
pub fn find_most_specific_volume<'a>(path: &str, volumes: &'a [(i64, String)]) -> Option<(i64, &'a str)> {
    volumes
        .iter()
        .filter(|(_, mount_point)| is_path_within_root(path, mount_point))
        .max_by_key(|(volume_id, mount_point)| (root_specificity(mount_point), Reverse(*volume_id)))
        .map(|(volume_id, mount_point)| (*volume_id, mount_point.as_str()))
}

/// Find the most specific volume ID for `path` from a slice of [`IndexedVolume`] records.
#[must_use]
pub fn find_volume_id_for_path(path: &str, volumes: &[IndexedVolume]) -> Option<i64> {
    volumes
        .iter()
        .filter_map(|volume| {
            let id = volume.id?;
            is_path_within_root(path, &volume.mount_point).then_some((id, volume.mount_point.as_str()))
        })
        .max_by_key(|(volume_id, mount_point)| (root_specificity(mount_point), Reverse(*volume_id)))
        .map(|(volume_id, _)| volume_id)
}

/// Check whether `path` matches any directory or path exclusion rule from `[daemon].exclude`.
///
/// Supports `*suffix`, `prefix*`, `*contains*`, and whole-component or multi-component paths.
/// Literal backslashes inside Unix filenames are preserved when matching non-Windows rules.
#[must_use]
pub fn matches_exclude_rules(path: &str, exclude: &[String]) -> bool {
    if exclude.is_empty() {
        return false;
    }
    exclude.iter().any(|pattern| matches_single_exclude_rule(path, pattern))
}

/// Check whether `path` matches any glob pattern from `[daemon].exclude_patterns`.
///
/// Patterns without path separators match against the entry's filename (for example `*.tmp`, `~$*`, `Thumbs.db`).
/// Patterns containing a path separator match against the full path.
#[must_use]
pub fn matches_exclude_globs(path: &str, exclude_patterns: &[String]) -> bool {
    if exclude_patterns.is_empty() {
        return false;
    }
    let file_name = extract_entry_name(path);
    let unix_path = path.starts_with('/') && !is_windows_style_path(path);

    exclude_patterns.iter().any(|pattern| {
        if pattern.is_empty() {
            return false;
        }
        let has_separator = pattern.contains('/') || pattern.contains('\\');
        if has_separator {
            let (haystack, needle) = if unix_path && !is_windows_style_path(pattern) && !pattern.contains('\\') {
                (path.to_lowercase(), pattern.to_lowercase())
            } else {
                (
                    path.replace('/', "\\").to_lowercase(),
                    pattern.replace('/', "\\").to_lowercase(),
                )
            };
            glob_matches(&haystack, &needle)
        } else {
            glob_matches(&file_name.to_lowercase(), &pattern.to_lowercase())
        }
    })
}

/// Return whether `path` has a dot-prefixed hidden component inside `matched_root`.
///
/// Components above `matched_root` are ignored so roots under a hidden parent directory stay indexable.
/// Names starting with `.tmp` are permitted so temporary test directories are not skipped.
#[must_use]
pub fn has_hidden_component_within_root(path: &str, matched_root: Option<&str>) -> bool {
    let relative = match matched_root {
        Some(root) if is_path_within_root(path, root) => relative_suffix_after_root(path, root),
        _ => extract_entry_name(path),
    };
    if relative.is_empty() {
        return false;
    }

    let separator = if is_windows_style_path(path) || cfg!(windows) {
        '\\'
    } else {
        '/'
    };
    let normalized = if separator == '\\' {
        relative.replace('/', "\\")
    } else {
        relative.to_string()
    };

    normalized
        .split(separator)
        .filter(|component| !component.is_empty())
        .any(|component| component.starts_with('.') && !component.starts_with(".tmp"))
}

/// Check a single `[daemon].exclude` pattern against `path`.
fn matches_single_exclude_rule(path: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }

    let preserve_unix_backslash = path.starts_with('/') && !is_windows_style_path(pattern) && !pattern.contains('\\');
    let separator = if preserve_unix_backslash { '/' } else { '\\' };

    let normalized_path = if preserve_unix_backslash {
        path.to_lowercase()
    } else {
        path.replace('/', "\\").to_lowercase()
    };
    let normalized_pattern = if preserve_unix_backslash {
        pattern.to_lowercase()
    } else {
        pattern.replace('/', "\\").to_lowercase()
    };

    if let Some(rest) = normalized_pattern.strip_prefix('*') {
        return rest.strip_suffix('*').map_or_else(
            || normalized_path.ends_with(rest),
            |inner| normalized_path.contains(inner),
        );
    }

    if let Some(prefix) = normalized_pattern.strip_suffix('*') {
        return normalized_path.starts_with(prefix);
    }

    let bounded_path = format!("{separator}{}{separator}", normalized_path.trim_matches(separator));
    let bounded_pattern = format!("{separator}{}{separator}", normalized_pattern.trim_matches(separator));
    bounded_path.contains(&bounded_pattern)
}

/// Extract the final entry name from `path`, respecting platform separators.
#[must_use]
pub fn extract_entry_name(path: &str) -> &str {
    if is_windows_style_path(path) || cfg!(windows) {
        path.rsplit(['\\', '/']).next().unwrap_or(path)
    } else {
        path.rsplit('/').next().unwrap_or(path)
    }
}

/// Extract the relative portion of `path` strictly after `root`.
fn relative_suffix_after_root<'a>(path: &'a str, root: &str) -> &'a str {
    if is_windows_style_path(root) || is_windows_style_path(path) || cfg!(windows) {
        let trimmed_root = root.trim_end_matches(['\\', '/']);
        let path_len = trimmed_root.len();
        return path.get(path_len..).unwrap_or_default().trim_start_matches(['\\', '/']);
    }

    let trimmed_root = root.trim_end_matches('/');
    if trimmed_root.is_empty() {
        return path.trim_start_matches('/');
    }
    path.strip_prefix(trimmed_root)
        .unwrap_or_default()
        .trim_start_matches('/')
}

/// Match `text` against a simple `*` and `?` glob `pattern`.
fn glob_matches(text: &str, pattern: &str) -> bool {
    let text_chars: Vec<char> = text.chars().collect();
    let pattern_chars: Vec<char> = pattern.chars().collect();
    let mut text_index = 0;
    let mut pattern_index = 0;
    let mut star_pattern_index: Option<usize> = None;
    let mut star_text_index = 0;

    while text_index < text_chars.len() {
        if let Some(&pattern_char) = pattern_chars.get(pattern_index)
            && (pattern_char == '?' || pattern_chars.get(pattern_index) == text_chars.get(text_index))
        {
            text_index += 1;
            pattern_index += 1;
        } else if pattern_chars.get(pattern_index) == Some(&'*') {
            star_pattern_index = Some(pattern_index);
            star_text_index = text_index;
            pattern_index += 1;
        } else if let Some(star_index) = star_pattern_index {
            pattern_index = star_index + 1;
            star_text_index += 1;
            text_index = star_text_index;
        } else {
            return false;
        }
    }

    while pattern_chars.get(pattern_index) == Some(&'*') {
        pattern_index += 1;
    }

    pattern_index == pattern_chars.len()
}

#[cfg(test)]
mod test_scope_roots {
    use super::*;

    #[test]
    fn unix_roots_respect_component_boundaries_case_and_literal_backslashes() {
        assert!(is_path_within_root("/data/a/file.txt", "/data/a"));
        assert!(is_path_within_root("/data/a/file.txt", "/data/a/"));
        assert!(is_path_within_root("/data/a", "/data/a"));
        assert!(!is_path_within_root("/data/a-extra/file.txt", "/data/a"));
        assert!(!is_path_within_root("/data/A/file.txt", "/data/a"));
        assert!(is_path_within_root("/data/foo\\bar", "/data"));
        assert!(!is_path_within_root("/data/foo\\bar", "/data/foo"));
        assert!(is_path_within_root("/anything", "/"));
    }

    #[test]
    fn windows_roots_handle_drives_and_unc_shares_case_insensitively() {
        assert!(is_path_within_root(r"C:\Users\file.txt", "C:"));
        assert!(is_path_within_root(r"c:\users\file.txt", r"C:\Users"));
        assert!(!is_path_within_root(r"C:\UsersExtra\file.txt", r"C:\Users"));
        assert!(is_path_within_root(r"\\nas\share\a.mkv", r"\\NAS\SHARE"));
        assert!(!is_path_within_root(r"\\nas\shared\a.mkv", r"\\nas\share"));
    }

    #[test]
    fn overlapping_roots_resolve_deterministically_to_deepest_owner() {
        let volumes = vec![
            (1, "/".to_string()),
            (2, "/data".to_string()),
            (3, "/data/sub".to_string()),
            (4, "/data/other".to_string()),
        ];

        assert_eq!(
            find_most_specific_volume("/data/sub/a.txt", &volumes),
            Some((3, "/data/sub"))
        );
        assert_eq!(find_most_specific_volume("/data/top.txt", &volumes), Some((2, "/data")));
        assert_eq!(
            find_most_specific_volume("/data/other/b.txt", &volumes),
            Some((4, "/data/other"))
        );
        assert_eq!(
            find_most_specific_volume("/data/sub-sibling/c.txt", &volumes),
            Some((2, "/data"))
        );
        assert_eq!(find_most_specific_volume("/etc/hosts", &volumes), Some((1, "/")));
    }

    #[test]
    fn two_unc_shares_do_not_collide() {
        let volumes = vec![(10, r"\\nas\media".to_string()), (20, r"\\nas\backup".to_string())];

        assert_eq!(
            find_most_specific_volume(r"\\nas\media\movie.mkv", &volumes),
            Some((10, r"\\nas\media"))
        );
        assert_eq!(
            find_most_specific_volume(r"\\nas\backup\archive.tar", &volumes),
            Some((20, r"\\nas\backup"))
        );
        assert_eq!(find_most_specific_volume(r"\\nas\other\x.txt", &volumes), None);
    }
}

#[cfg(test)]
mod test_scope_exclusions {
    use super::*;

    #[test]
    fn applies_exclude_globs_hidden_components_and_unix_literal_backslashes() {
        let scope = IndexScope::new(
            vec!["/Users/me/.hidden_parent/work".to_string()],
            vec!["node_modules".to_string(), "foo".to_string()],
            vec!["*.tmp".to_string(), "~$*".to_string(), "Thumbs.db".to_string()],
        );

        assert!(scope.is_included("/Users/me/.hidden_parent/work/main.rs", None));
        assert!(scope.is_included("/Users/me/.hidden_parent/work/foo\\bar", None));
        assert!(!scope.is_included("/Users/me/.hidden_parent/work/draft.tmp", None));
        assert!(!scope.is_included("/Users/me/.hidden_parent/work/~$report.docx", None));
        assert!(!scope.is_included("/Users/me/.hidden_parent/work/Thumbs.db", None));
        assert!(!scope.is_included("/Users/me/.hidden_parent/work/.git/config", None));
        assert!(!scope.is_included("/Users/me/.hidden_parent/work/.hidden.nfo", None));
        assert!(!scope.is_included("/Users/me/.hidden_parent/work/node_modules/pkg.js", None));
        assert!(!scope.is_included("/Users/me/outside/main.rs", None));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_unix_paths() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let bad_path = Path::new(OsStr::from_bytes(b"/data/bad-\xff-name.txt"));
        assert!(path_to_utf8(bad_path).is_err());
        assert_eq!(path_to_utf8(Path::new("/data/valid.txt")).unwrap(), "/data/valid.txt");
    }
}
