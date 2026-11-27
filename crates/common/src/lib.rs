//! Shared helpers for the `cli-tools` binaries.
//!
//! This crate provides reusable path handling, text formatting, coloured printing, date parsing,
//! diff rendering (behind the `diff` feature), user config lookup,
//! and shell completion generation (behind the `completion` feature).

#![cfg_attr(test, allow(clippy::panic_in_result_fn))]

pub mod batch_rename;
#[cfg(feature = "completion")]
pub mod completion;
#[cfg(feature = "config")]
pub mod config;
pub mod date;
#[cfg(feature = "diff")]
pub mod diff;
pub mod file_handling;
pub mod simd;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod utils;

pub use batch_rename::{BatchRenameEntry, BatchRenameOutcome, BatchRenameStatus, BatchRenamer};
#[cfg(feature = "completion")]
pub use completion::{CompletionArgs, generate_shell_completion};
#[cfg(feature = "config")]
pub use config::{load_user_config_section, load_user_config_section_at, parse_user_config_section};
#[cfg(feature = "diff")]
pub use diff::{color_diff, diff_lines, show_diff};
pub use file_handling::{
    CopyProgress, CopyVerifier, MoveResult, OverwritePolicy, PathChange, RenameOutcome, TargetReservations,
    check_destination_parents, copy_dir_recursive, copy_file_with_times, directory_is_case_sensitive,
    is_case_only_change, move_file_by_copy, paths_refer_to_same_entry, paths_refer_to_same_file,
    remove_empty_directories, safe_move_file, safe_move_file_with_progress, safe_rename, unique_sibling_path,
    write_atomically,
};
pub use utils::{
    NameFilter, WalkOptions, collect_entries, command_exists, count_files, data_dir, data_file_path, find_command,
    get_relative_path_from_current_working_directory, get_relative_path_or_filename, glob_to_regex, leading_whitespace,
    lowercase_extension, nearest_containing_root, normalize_extensions, original_end_for_lowercase_prefix,
    os_str_to_string, path_to_file_extension_string, path_to_file_stem_string, path_to_filename_string, path_to_string,
    path_to_string_relative, path_to_string_relative_to, relative_path_within, resolve_input_paths,
    starts_with_ignore_case, strings_from, strip_ansi_codes, unique_in_order,
};

use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
#[cfg(feature = "async")]
use std::sync::Arc;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use colored::{ColoredString, Colorize};
use jiff::{Timestamp, tz::TimeZone};
use regex::Regex;
#[cfg(feature = "async")]
use tokio::sync::Semaphore;
use unicode_normalization::UnicodeNormalization;

/// Shared regex to match video resolutions like 1080p, 2160p, or 1920x1080.
pub static RE_RESOLUTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(\d{3,4}p|\d{3,4}x\d{3,4})\b").expect("Invalid resolution regex"));

/// Shared regex to match repeated filename separators.
pub static RE_REPEATED_SEPARATORS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[.\-_\s]{2,}").expect("Invalid repeated separators regex"));

/// Environment variable that overrides the user config file path.
///
/// An empty value disables reading a user config file altogether.
pub const CONFIG_PATH_ENV: &str = "CLI_TOOLS_CONFIG";

/// Base name of the shared user config file.
const PROJECT_NAME: &str = "cli-tools";

/// Bytes per kilobyte.
const KB: u64 = 1024;
/// Bytes per megabyte.
const MB: u64 = KB * 1024;
/// Bytes per gigabyte.
const GB: u64 = MB * 1024;
/// Bytes per terabyte.
const TB: u64 = GB * 1024;

/// Windows API constant for remote/network drive type.
#[cfg(windows)]
const DRIVE_REMOTE: u32 = 4;

/// System directories that should be skipped when iterating files.
const SYSTEM_DIRECTORIES: &[&str] = &[
    // Windows
    "$RECYCLE.BIN",
    "System Volume Information",
    // macOS
    ".Spotlight-V100",
    ".fseventsd",
    ".Trashes",
    // Linux
    "lost+found",
    // NAS
    "#Recycle",
    // Serato DJ library metadata
    "_Serato_",
];

/// Classification of a Windows path prefix for network drive detection.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsPathPrefix {
    /// A UNC network share path such as `\\server\share`, `//server/share`, or `\\?\UNC\server\share`.
    Unc,
    /// A drive letter path such as `C:\...` or `\\?\C:\...`, whose drive type must be queried.
    DriveLetter(u8),
    /// A local or device path that is not a UNC share or drive letter.
    Other,
}

/// Byte range of a highlighted match in text.
#[derive(Debug, Clone, Copy)]
pub struct MatchRange {
    /// Start position of the match, inclusive.
    pub start: usize,
    /// End position of the match, exclusive.
    pub end: usize,
}

impl MatchRange {
    /// Extract the matched substring from the given text.
    #[must_use]
    pub fn extract_from<'a>(&self, text: &'a str) -> Option<&'a str> {
        text.get(self.start..self.end)
    }
}

/// Return the path to the user config file.
///
/// Checks `CLI_TOOLS_CONFIG` first so tests and callers can point to a custom config file,
/// or disable config loading by setting it to an empty string.
/// During workspace tests, returns the sample fixture at `crates/common/tests/fixtures/sample_config.toml`.
/// Otherwise, returns `$HOME/.config/cli-tools.toml`.
///
/// Returns `None` if config loading is explicitly disabled or the home directory cannot be determined.
#[must_use]
pub fn config_path() -> Option<PathBuf> {
    if let Some(override_path) = env::var_os(CONFIG_PATH_ENV) {
        return (!override_path.is_empty()).then(|| PathBuf::from(override_path));
    }

    if cfg!(test) {
        return Some(sample_config_fixture_path());
    }

    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from);
    let target_tmpdir = env::var_os("CARGO_TARGET_TMPDIR").map(PathBuf::from);
    let current_exe = env::current_exe().ok();
    if is_workspace_test_context(
        manifest_dir.as_deref(),
        target_tmpdir.as_deref(),
        current_exe.as_deref(),
    ) {
        return Some(sample_config_fixture_path());
    }

    let home_dir = dirs::home_dir()?;
    Some(home_dir.join(".config").join(format!("{PROJECT_NAME}.toml")))
}

/// Append an extension to `PathBuf`, stripping any trailing path separators first.
#[must_use]
pub fn append_extension_to_path(path: &Path, extension: impl AsRef<OsStr>) -> PathBuf {
    let normalized: PathBuf = if path.as_os_str().is_empty() {
        PathBuf::new()
    } else {
        path.components().collect()
    };
    let mut os_string: OsString = normalized.into();
    os_string.push(".");
    os_string.push(extension);
    os_string.into()
}

/// Helper method to assert floating point equality in test cases.
///
/// # Panics
/// Panics if the difference between `a` and `b` exceeds scaled `f64::EPSILON`.
#[inline]
pub fn assert_f64_eq(a: f64, b: f64) {
    let epsilon = f64::EPSILON * a.abs().max(b.abs()).max(1.0);
    assert!(
        (a - b).abs() <= epsilon,
        "Values are not equal: {a} and {b} (epsilon = {epsilon})"
    );
}

/// Format text with an optional match highlighted in green.
#[must_use]
pub fn format_text_with_highlight(text: &str, match_range: Option<MatchRange>) -> String {
    match_range.map_or_else(
        || text.to_string(),
        |range| {
            let Some(before) = text.get(..range.start) else {
                return text.to_string();
            };
            let Some(matched) = text.get(range.start..range.end) else {
                return text.to_string();
            };
            let Some(after) = text.get(range.end..) else {
                return text.to_string();
            };
            format!("{before}{}{after}", matched.green())
        },
    )
}

/// Format bool value as a coloured string.
#[must_use]
pub fn colorize_bool(value: bool) -> ColoredString {
    if value { "true".green() } else { "false".red() }
}

/// Collapse repeated filename separators and trim separators from both ends.
///
/// This is useful when producing a cleaned display filename
/// after removing tokens such as codecs, resolutions, or release tags.
/// Consecutive separators made from dots, hyphens, underscores,
/// or whitespace are reduced to the first separator in each sequence,
/// then any remaining separator is trimmed from both ends.
///
/// # Examples
///
/// ```
/// let cleaned = cli_tools_common::collapse_repeated_separators("Movie..Title__x265--1080p ");
/// assert_eq!(cleaned, "Movie.Title_x265-1080p");
/// ```
#[must_use]
pub fn collapse_repeated_separators(text: &str) -> String {
    const SEPARATORS: simd::ByteSet = simd::ByteSet::new(b".-_ \t\n\x0b\x0c\r");
    let trim = |character: char| matches!(character, '.' | '-' | '_') || character.is_whitespace();
    // Unicode whitespace also counts as a separator, so only ASCII text can skip the regex.
    if text.is_ascii() && !simd::has_adjacent_bytes_in(text.as_bytes(), &SEPARATORS) {
        return text.trim_matches(trim).to_string();
    }
    let collapsed = RE_REPEATED_SEPARATORS.replace_all(text, |captures: &regex::Captures<'_>| {
        captures[0]
            .chars()
            .next()
            .map_or_else(String::new, |character| character.to_string())
    });

    collapsed.trim_matches(trim).to_string()
}

/// Format a number with comma thousands separators, for example 1234567 as "1,234,567".
///
/// # Examples
///
/// ```
/// use cli_tools_common::format_number;
///
/// assert_eq!(format_number(999), "999");
/// assert_eq!(format_number(1_234_567), "1,234,567");
/// ```
#[must_use]
pub fn format_number(number: u64) -> String {
    let digits = number.to_string();
    let length = digits.len();
    let mut formatted = String::with_capacity(length + length.saturating_sub(1) / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (length - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(digit);
    }
    formatted
}

/// Format bytes as human-readable size.
///
/// # Examples
///
/// ```
/// use cli_tools_common::format_size;
///
/// assert_eq!(format_size(0), "0 B");
/// assert_eq!(format_size(512), "512 B");
/// assert_eq!(format_size(1024), "1.00 KB");
/// assert_eq!(format_size(1_048_576), "1.00 MB");
/// assert_eq!(format_size(1_073_741_824), "1.00 GB");
/// ```
#[must_use]
pub fn format_size(bytes: u64) -> String {
    if bytes >= TB {
        format!("{:.2} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Format duration as a human-readable string.
#[must_use]
pub fn format_duration(duration: std::time::Duration) -> String {
    let secs = duration.as_secs();
    if secs >= 3600 {
        format!("{}h {:02}m {:02}s", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Format duration from seconds as a human-readable string.
/// Negative and non-finite values are treated as zero.
#[must_use]
pub fn format_duration_seconds(seconds: f64) -> String {
    if !seconds.is_finite() {
        return "0.0s".to_string();
    }
    // Ensure non-negative value before converting to avoid sign loss
    let seconds = seconds.max(0.0);
    // Round sub-minute values that would otherwise format as "60.0s" up into the minute branch.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let secs = if (59.95..60.0).contains(&seconds) {
        60
    } else {
        seconds as u64
    };
    if secs >= 3600 {
        format!("{}h {:02}m {:02}s", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{seconds:.1}s")
    }
}

/// Prompt the user for confirmation with a yes or no question.
///
/// Returns `true` if the user answers "y" or "yes".
/// Returns `false` if the user answers anything else or if stdin reaches EOF.
/// The default value is used when the user presses Enter without input.
///
/// # Errors
/// Returns an error if reading from stdin fails.
pub fn get_user_confirmation(message: &str, default: bool) -> std::io::Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    print!("{message} {hint} ");
    io::stdout().flush()?;
    read_confirmation(io::stdin().lock(), default)
}

/// Read one answer from the reader and decide what it means.
///
/// End of input returns `false` so closed stdin never auto-confirms a prompt.
/// An empty line takes the default, "y" and "yes" are yes, and anything else is no.
/// Taking the reader as an argument keeps the decision testable.
fn read_confirmation(mut reader: impl io::BufRead, default: bool) -> std::io::Result<bool> {
    let mut input = String::new();
    if reader.read_line(&mut input)? == 0 {
        return Ok(false);
    }

    let trimmed = input.trim().to_lowercase();
    if trimmed.is_empty() {
        Ok(default)
    } else {
        Ok(trimmed == "y" || trimmed == "yes")
    }
}

/// Return the singular noun for a count of one and the plural for any other count, zero included.
#[must_use]
pub const fn pluralize(count: usize, singular: &'static str, plural: &'static str) -> &'static str {
    if count == 1 { singular } else { plural }
}

/// Format a count and noun, with the singular only for a count of one.
#[must_use]
pub fn count_label(count: usize, singular: &'static str, plural: &'static str) -> String {
    format!("{count} {}", pluralize(count, singular, plural))
}

/// Get filename from Path with special characters retained instead of decomposed.
///
/// # Errors
/// Returns an error if the file stem cannot be extracted from the path.
pub fn get_normalized_file_name_and_extension(path: &Path) -> Result<(String, String)> {
    let file_stem = os_str_to_string(path.file_stem().context("Failed to get file stem")?);
    let file_extension = os_str_to_string(path.extension().unwrap_or_default());

    // Rust uses Unicode NFD (Normalization Form Decomposed) by default,
    // which converts special chars like "å" to "a\u{30a}",
    // which then get printed as a regular "a".
    // Use NFC (Normalization Form Composed)
    // from unicode_normalization crate to retain the correct format and not cause issues later on.
    // https://github.com/unicode-rs/unicode-normalization

    Ok((into_nfc(file_stem), into_nfc(file_extension)))
}

/// Get the normalized directory name from a Path with special characters retained.
///
/// # Errors
/// Returns an error if the directory name cannot be extracted from the path.
pub fn get_normalized_dir_name(path: &Path) -> Result<String> {
    let dir_name = os_str_to_string(path.file_name().context("Failed to get directory name")?);

    Ok(into_nfc(dir_name))
}

/// The text in Unicode NFC, returned as is when it already is, which every ASCII name is.
fn into_nfc(text: String) -> String {
    if text.is_ascii() || unicode_normalization::is_nfc_quick(text.chars()) == unicode_normalization::IsNormalized::Yes
    {
        text
    } else {
        text.nfc().collect()
    }
}

/// Check if entry is a hidden file or directory (starts with '.')
#[must_use]
pub fn is_hidden(entry: &walkdir::DirEntry) -> bool {
    let name_bytes = entry.file_name().as_encoded_bytes();
    name_bytes.first() == Some(&b'.')
}

/// Check if entry is a hidden file or directory (starts with '.')
#[cfg(feature = "async")]
#[must_use]
pub fn is_hidden_tokio(entry: &tokio::fs::DirEntry) -> bool {
    let name = entry.file_name();
    let name_bytes = name.as_encoded_bytes();
    name_bytes.first() == Some(&b'.')
}

/// Check if a path is a hidden file or directory (name starts with '.')
#[must_use]
pub fn is_hidden_path(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.as_encoded_bytes().first() == Some(&b'.'))
}

/// Check if entry is a system directory that should be skipped.
/// Returns true for OS-specific directories like `$RECYCLE.BIN`, `.Spotlight-V100`, or `lost+found`.
#[must_use]
pub fn is_system_directory(entry: &walkdir::DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    SYSTEM_DIRECTORIES.iter().any(|dir| name.eq_ignore_ascii_case(dir))
}

/// Check if entry is a system directory that should be skipped.
/// Returns true for OS-specific directories like `$RECYCLE.BIN`, `.Spotlight-V100`, or `lost+found`.
#[cfg(feature = "async")]
#[must_use]
pub fn is_system_directory_tokio(entry: &tokio::fs::DirEntry) -> bool {
    if !entry.path().is_dir() {
        return false;
    }
    let file_name = entry.file_name();
    let name = file_name.to_string_lossy();
    SYSTEM_DIRECTORIES.iter().any(|dir| name.eq_ignore_ascii_case(dir))
}

/// Check if a path is a system directory that should be skipped.
/// Returns true for OS-specific directories like `$RECYCLE.BIN`, `.Spotlight-V100`, or `lost+found`.
#[must_use]
pub fn is_system_directory_path(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        let name = name.to_string_lossy();
        SYSTEM_DIRECTORIES.iter().any(|dir| name.eq_ignore_ascii_case(dir))
    })
}

/// Classify a Windows path string as a UNC share, a drive letter, or another local path.
///
/// Verbatim local paths (`\\?\C:\...`) and device paths (`\\.\...`)
/// are distinguished from UNC network shares (`\\server\share`, `//server/share`, and `\\?\UNC\server\share`).
#[cfg(any(windows, test))]
fn classify_windows_path_prefix(path_str: &str) -> WindowsPathPrefix {
    let bytes = path_str.as_bytes();
    let starts_with_double_sep = bytes.get(..2).is_some_and(|prefix| prefix == br"\\" || prefix == b"//");

    if starts_with_double_sep {
        let rest = path_str.get(2..).unwrap_or_default();
        if let Some(after_prefix) = rest
            .strip_prefix(r"?\")
            .or_else(|| rest.strip_prefix("?/"))
            .or_else(|| rest.strip_prefix(r".\"))
            .or_else(|| rest.strip_prefix("./"))
        {
            if after_prefix
                .get(..4)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(r"unc\") || prefix.eq_ignore_ascii_case("unc/"))
            {
                return WindowsPathPrefix::Unc;
            }
            if let Some(drive) = extract_drive_letter(after_prefix) {
                return WindowsPathPrefix::DriveLetter(drive);
            }
            return WindowsPathPrefix::Other;
        }
        return WindowsPathPrefix::Unc;
    }

    if let Some(drive) = extract_drive_letter(path_str) {
        return WindowsPathPrefix::DriveLetter(drive);
    }

    WindowsPathPrefix::Other
}

/// Extract an ASCII drive letter from a path beginning with `X:`.
#[cfg(any(windows, test))]
fn extract_drive_letter(text: &str) -> Option<u8> {
    let bytes = text.as_bytes();
    let first = *bytes.first()?;
    let second = *bytes.get(1)?;
    (first.is_ascii_alphabetic() && second == b':').then_some(first.to_ascii_uppercase())
}

/// Check if a path is on a network drive.
/// On Windows, detects mapped network drives and UNC paths.
/// On other platforms, always returns false.
#[cfg(windows)]
#[must_use]
pub fn is_network_path(path: &Path) -> bool {
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;

    // A relative path lies on the drive of the current directory, which may itself be a network drive.
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let path_str = absolute.to_string_lossy();
    match classify_windows_path_prefix(&path_str) {
        WindowsPathPrefix::Unc => true,
        WindowsPathPrefix::DriveLetter(drive) => {
            let root: [u16; 4] = [u16::from(drive), u16::from(b':'), u16::from(b'\\'), 0];
            // SAFETY: GetDriveTypeW reads a valid null-terminated UTF-16 drive root string.
            #[allow(unsafe_code)]
            let drive_type = unsafe { GetDriveTypeW(root.as_ptr()) };
            drive_type == DRIVE_REMOTE
        }
        WindowsPathPrefix::Other => false,
    }
}

/// Check if a path is on a network drive.
/// On Windows, detects mapped network drives and UNC paths.
/// On other platforms, always returns false.
#[cfg(not(windows))]
#[must_use]
pub const fn is_network_path(_path: &Path) -> bool {
    false
}

/// Walk up from `path` to find the nearest existing directory on disk.
fn nearest_existing_directory(path: &Path) -> PathBuf {
    let mut current = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
    };

    while !current.is_dir() {
        match current.parent() {
            Some(parent) if !parent.as_os_str().is_empty() && parent != current => {
                current = parent.to_path_buf();
            }
            _ => return PathBuf::from("."),
        }
    }
    current
}

/// Get the available disk space in bytes for the volume containing the given path.
///
/// When `path` is a file or does not exist yet,
/// the space is measured for the volume of the nearest existing ancestor directory.
/// Returns `None` if the available space could not be determined.
#[cfg(windows)]
#[must_use]
pub fn available_disk_space(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let directory = nearest_existing_directory(path);
    let mut wide: Vec<u16> = directory.as_os_str().encode_wide().collect();
    // null terminator
    wide.push(0);

    let mut free_bytes_available: u64 = 0;
    // SAFETY: GetDiskFreeSpaceExW reads the null-terminated directory path
    // and writes the number of free bytes available to the caller into the out-parameter.
    // The remaining out-parameters are null, which the API explicitly allows.
    #[allow(unsafe_code)]
    let result = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &raw mut free_bytes_available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };

    (result != 0).then_some(free_bytes_available)
}

/// Get the available disk space in bytes for the volume containing the given path.
///
/// When `path` is a file or does not exist yet,
/// the space is measured for the volume of the nearest existing ancestor directory.
/// Returns `None` if the available space could not be determined.
#[cfg(unix)]
#[must_use]
pub fn available_disk_space(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let directory = nearest_existing_directory(path);
    let path_cstr = CString::new(directory.as_os_str().as_bytes()).ok()?;
    let mut volume_info = std::mem::MaybeUninit::<libc::statvfs>::uninit();

    // SAFETY: `path_cstr` is a valid null-terminated path and `volume_info` points to writable `statvfs` storage.
    #[allow(unsafe_code)]
    let call_result = unsafe { libc::statvfs(path_cstr.as_ptr(), volume_info.as_mut_ptr()) };
    if call_result != 0 {
        return None;
    }

    // SAFETY: `statvfs` returned 0, so `volume_info` has been initialized by the operating system.
    #[allow(unsafe_code)]
    let volume_info = unsafe { volume_info.assume_init() };
    // Field widths differ across Unix targets (u32 vs u64), so widen explicitly.
    #[allow(clippy::useless_conversion)]
    let block_size = u64::from(volume_info.f_frsize).max(u64::from(volume_info.f_bsize));
    #[allow(clippy::useless_conversion)]
    u64::from(volume_info.f_bavail).checked_mul(block_size)
}

/// Get the available disk space in bytes for the volume containing the given path.
///
/// Returns `None` on platforms that are neither Unix nor Windows.
#[cfg(not(any(unix, windows)))]
#[must_use]
pub const fn available_disk_space(_path: &Path) -> Option<u64> {
    None
}

/// Check if entry should be skipped (hidden or system directory).
/// Combines `is_hidden` and `is_system_directory` checks.
/// The walk root itself is never skipped, since the user asked for it explicitly.
#[must_use]
pub fn should_skip_entry(entry: &walkdir::DirEntry) -> bool {
    entry.depth() > 0 && (is_hidden(entry) || is_system_directory(entry))
}

/// Check if a path should be skipped (hidden or system directory).
/// Combines `is_hidden_path` and `is_system_directory_path` checks,
/// for walkers that do not hand out a [`walkdir::DirEntry`].
#[must_use]
pub fn should_skip_path(path: &Path) -> bool {
    is_hidden_path(path) || is_system_directory_path(path)
}

/// Check if directory is empty (contains no files or subdirectories).
///
/// Returns `false` when the path does not exist, is not a directory, or cannot be read.
#[must_use]
pub fn is_directory_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none())
}

/// Insert a suffix before the file extension.
///
/// Takes a path and inserts the given suffix string between the file stem and the file extension.
/// If the file has no extension, the suffix is appended to the end.
///
/// ```rust
/// use std::path::Path;
/// use cli_tools_common::insert_suffix_before_extension;
///
/// // Basic usage with extension
/// let path = Path::new("video.1080p.mp4");
/// let result = insert_suffix_before_extension(path, ".x265");
/// assert_eq!(result.to_str().unwrap(), "video.1080p.x265.mp4");
///
/// // With directory path
/// let path = Path::new("subdir/video.mp4");
/// let result = insert_suffix_before_extension(path, ".converted");
/// assert_eq!(result, Path::new("subdir/video.converted.mp4"));
///
/// // Without extension
/// let path = Path::new("README");
/// let result = insert_suffix_before_extension(path, ".backup");
/// assert_eq!(result.to_str().unwrap(), "README.backup");
/// ```
#[must_use]
pub fn insert_suffix_before_extension(path: &Path, suffix: &str) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let mut new_name = path.file_stem().map_or_default(OsStr::to_os_string);
    new_name.push(suffix);
    if let Some(extension) = path.extension()
        && !extension.is_empty()
    {
        new_name.push(".");
        new_name.push(extension);
    }

    if parent.as_os_str().is_empty() {
        PathBuf::from(new_name)
    } else {
        parent.join(new_name)
    }
}

/// Resolves the provided input path to a directory or file to an absolute path.
///
/// If `path` is `None`, the current working directory is used.
/// The function verifies that the provided path exists and is accessible,
/// returning an error if it does not.
/// ```rust
/// use std::path::Path;
/// use cli_tools_common::resolve_input_path;
///
/// let path = Path::new("src");
/// let absolute_path = resolve_input_path(Some(path)).unwrap();
/// ```
/// # Errors
/// Returns an error if:
/// - The current working directory cannot be determined
/// - The provided path does not exist or is not accessible
/// - Path canonicalization fails
#[inline]
pub fn resolve_input_path(path: Option<&Path>) -> Result<PathBuf> {
    match path.map(trim_path) {
        Some(input_path) if !input_path.as_os_str().is_empty() => resolve_existing_path(input_path),
        _ => resolve_existing_path(env::current_dir().context("Failed to get current working directory")?),
    }
}

/// Resolves the provided input path to a directory or file to an absolute path.
///
/// If `path` is `None` or an empty string, the current working directory is used.
/// The function verifies that the provided path exists and is accessible,
/// returning an error if it does not.
///
/// ```rust
/// use std::path::PathBuf;
/// use cli_tools_common::resolve_input_path_str;
///
/// let path = Some("src");
/// let absolute_path = resolve_input_path_str(path).unwrap();
/// ```
/// # Errors
/// Returns an error if:
/// - The current working directory cannot be determined
/// - The provided path does not exist or is not accessible
/// - Path canonicalization fails
#[inline]
pub fn resolve_input_path_str(path: Option<&str>) -> Result<PathBuf> {
    resolve_input_path(path.map(Path::new))
}

/// Resolves a required input path to an absolute path.
///
/// Unlike `resolve_input_path`,
/// this function does not fall back to the current working directory when the path is empty.
/// It requires a valid, non-empty path.
/// ```rust
/// use std::path::Path;
/// use cli_tools_common::resolve_required_input_path;
///
/// let path = Path::new("src");
/// let absolute_path = resolve_required_input_path(path).unwrap();
/// ```
/// # Errors
/// Returns an error if:
/// - The provided path is empty
/// - The path does not exist or is not accessible
/// - Path canonicalization fails
pub fn resolve_required_input_path(path: &Path) -> Result<PathBuf> {
    let input_path = trim_path(path);

    if input_path.as_os_str().is_empty() {
        anyhow::bail!("Input path cannot be empty");
    }

    resolve_existing_path(input_path)
}

/// Resolves a required input path to an absolute path.
///
/// Unlike `resolve_input_path`,
/// this function does not fall back to the current working directory when the path is empty.
/// It requires a valid, non-empty path.
/// ```rust
/// use cli_tools_common::resolve_required_input_path_str;
///
/// let path = "src";
/// let absolute_path = resolve_required_input_path_str(path).unwrap();
/// ```
/// # Errors
/// Returns an error if:
/// - The provided path is empty
/// - The path does not exist or is not accessible
/// - Path canonicalization fails
pub fn resolve_required_input_path_str(path: &str) -> Result<PathBuf> {
    resolve_required_input_path(Path::new(path))
}

/// Trim surrounding whitespace from a UTF-8 path unless the literal path already exists on disk.
///
/// Existing paths with leading or trailing spaces are kept intact,
/// and paths that are not valid UTF-8 are returned unchanged.
fn trim_path(path: &Path) -> PathBuf {
    if !path.as_os_str().is_empty() && path.exists() {
        return path.to_path_buf();
    }
    path.to_str()
        .map_or_else(|| path.to_path_buf(), |text| PathBuf::from(text.trim()))
}

/// Check that a path exists and return its absolute form.
fn resolve_existing_path(filepath: PathBuf) -> Result<PathBuf> {
    if !filepath.exists() {
        anyhow::bail!(
            "Input path does not exist or is not accessible: '{}'",
            filepath.display()
        );
    }

    let absolute_input_path = dunce::canonicalize(&filepath)?;

    // Canonicalize fails for network drives on Windows :(
    if path_to_string(&absolute_input_path).starts_with(r"\\?") && !path_to_string(&filepath).starts_with(r"\\?") {
        if filepath.is_absolute() {
            Ok(filepath)
        } else {
            Ok(env::current_dir()?.join(filepath))
        }
    } else {
        Ok(absolute_input_path)
    }
}

/// Resolves the provided output path relative to an absolute input path.
///
/// If `path` is provided, it is used directly.
/// If `path` is `None` or an empty string, and the absolute input path is a file,
/// the parent directory of the input path is used.
/// Otherwise, the input directory is used as the output path.
/// # Errors
/// Returns an error if the parent directory cannot be determined when the input path is a file.
#[inline]
pub fn resolve_output_path(path: Option<&str>, absolute_input_path: &Path) -> Result<PathBuf> {
    let output_path = {
        let path = path.unwrap_or_default().trim().to_string();
        if path.is_empty() {
            if absolute_input_path.is_file() {
                absolute_input_path
                    .parent()
                    .context("Failed to get parent directory")?
                    .to_path_buf()
            } else {
                absolute_input_path.to_path_buf()
            }
        } else {
            dunce::simplified(Path::new(&path)).to_path_buf()
        }
    };
    Ok(dunce::simplified(&output_path).to_path_buf())
}

/// Get a unique file path, adding a counter suffix if the file already exists.
///
/// Given a directory and filename components,
/// this function returns a path that doesn't conflict with existing files.
/// If the initial path exists,
/// it appends an incrementing counter (`.1`, `.2`, etc.) before the extension until a unique path is found.
///
/// # Arguments
///
/// * `dir` - The directory where the file will be placed
/// * `filename` - The complete filename including extension (e.g., "document.txt")
/// * `stem` - The filename without extension (e.g., "document")
/// * `extension` - The file extension without the leading dot (e.g., "txt"), or empty string if none
///
/// # Returns
///
/// A `PathBuf` that is guaranteed not to exist at the time of the call.
///
/// # Example
///
/// ```
/// use std::path::Path;
/// use cli_tools_common::get_unique_path;
///
/// let dir = Path::new("/tmp/output");
/// let filename = "report.pdf";
/// let stem = "report";
/// let extension = "pdf";
///
/// // If "report.pdf" exists, returns "report.1.pdf"
/// // If "report.1.pdf" also exists, returns "report.2.pdf", etc.
/// let unique_path = get_unique_path(dir, filename, stem, extension);
/// ```
#[must_use]
pub fn get_unique_path(dir: &Path, filename: &str, stem: &str, extension: &str) -> PathBuf {
    get_unique_path_with_reservations(dir, filename, stem, extension, &TargetReservations::new())
}

/// Find a numbered destination that is free on disk and not already claimed in this batch.
#[must_use]
pub fn get_unique_path_with_reservations(
    dir: &Path,
    filename: &str,
    stem: &str,
    extension: &str,
    reservations: &TargetReservations,
) -> PathBuf {
    get_unique_path_with_reservations_from(dir, filename, stem, extension, 1, reservations)
}

/// Find a numbered destination starting at `start_index` that is free on disk and not claimed in `reservations`.
#[must_use]
pub fn get_unique_path_with_reservations_from(
    dir: &Path,
    filename: &str,
    stem: &str,
    extension: &str,
    start_index: u32,
    reservations: &TargetReservations,
) -> PathBuf {
    let mut path = dir.join(filename);
    let mut counter = start_index;
    while std::fs::symlink_metadata(&path).is_ok() || reservations.contains(&path) {
        let new_name = if extension.is_empty() {
            format!("{stem}.{counter}")
        } else {
            format!("{stem}.{counter}.{extension}")
        };
        path = dir.join(new_name);
        let Some(next) = counter.checked_add(1) else {
            break;
        };
        counter = next;
    }

    path
}

/// Format a Unix timestamp as a local datetime string.
#[must_use]
pub fn format_timestamp(timestamp: i64) -> String {
    Timestamp::from_second(timestamp).map_or_else(
        |_| "unknown".to_string(),
        |timestamp| {
            timestamp
                .to_zoned_datetime(TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        },
    )
}

/// Print an error message prefixed with "Error: " in red to stderr.
///
/// # Examples
///
/// ```
/// cli_tools_common::print_error("something went wrong");
/// ```
#[inline]
pub fn print_error(message: &str) {
    eprintln!("{}", format!("Error: {message}").red());
}

#[macro_export]
macro_rules! print_error {
    ($($arg:tt)*) => {
        $crate::print_error(&format!($($arg)*))
    };
}

/// Print a message in yellow to stderr.
///
/// # Examples
///
/// ```
/// cli_tools_common::print_yellow("this might be a problem");
/// ```
#[inline]
pub fn print_yellow(message: &str) {
    eprintln!("{}", message.yellow());
}

#[macro_export]
macro_rules! print_yellow {
    ($($arg:tt)*) => {
        $crate::print_yellow(&format!($($arg)*))
    };
}

/// Print a message in green to stdout.
///
/// # Examples
///
/// ```
/// cli_tools_common::print_green("operation completed");
/// ```
#[inline]
pub fn print_green(message: &str) {
    println!("{}", message.green());
}

#[macro_export]
macro_rules! print_green {
    ($($arg:tt)*) => {
        $crate::print_green(&format!($($arg)*))
    };
}

/// Print a message in magenta to stdout.
#[inline]
pub fn print_magenta(message: &str) {
    println!("{}", message.magenta());
}

#[macro_export]
macro_rules! print_magenta {
    ($($arg:tt)*) => {
        $crate::print_magenta(&format!($($arg)*))
    };
}

/// Print a message in bold magenta to stdout.
///
/// # Examples
///
/// ```
/// cli_tools_common::print_magenta_bold("highlighted info");
/// ```
#[inline]
pub fn print_magenta_bold(message: &str) {
    println!("{}", message.magenta().bold());
}

#[macro_export]
macro_rules! print_magenta_bold {
    ($($arg:tt)*) => {
        $crate::print_magenta_bold(&format!($($arg)*))
    };
}

/// Print a message in cyan to stdout.
///
/// # Examples
///
/// ```
/// cli_tools_common::print_cyan("indexing files...");
/// ```
#[inline]
pub fn print_cyan(message: &str) {
    println!("{}", message.cyan());
}

#[macro_export]
macro_rules! print_cyan {
    ($($arg:tt)*) => {
        $crate::print_cyan(&format!($($arg)*))
    };
}

/// Print a message in bold yellow to stdout.
///
/// # Examples
///
/// ```
/// cli_tools_common::print_yellow_bold("important notice");
/// ```
#[inline]
pub fn print_yellow_bold(message: &str) {
    println!("{}", message.yellow().bold());
}

#[macro_export]
macro_rules! print_yellow_bold {
    ($($arg:tt)*) => {
        $crate::print_yellow_bold(&format!($($arg)*))
    };
}

/// Print a message in bold red to stdout.
///
/// # Examples
///
/// ```
/// cli_tools_common::print_red_bold("critical error");
/// ```
#[inline]
pub fn print_red_bold(message: &str) {
    println!("{}", message.red().bold());
}

#[macro_export]
macro_rules! print_red_bold {
    ($($arg:tt)*) => {
        $crate::print_red_bold(&format!($($arg)*))
    };
}

/// Print a message in bold to stdout.
#[inline]
pub fn print_bold(message: &str) {
    println!("{}", message.bold());
}

#[macro_export]
macro_rules! print_bold {
    ($($arg:tt)*) => {
        $crate::print_bold(&format!($($arg)*))
    };
}

/// Print a message dimmed to stdout.
#[inline]
pub fn print_dimmed(message: &str) {
    println!("{}", message.dimmed());
}

#[macro_export]
macro_rules! print_dimmed {
    ($($arg:tt)*) => {
        $crate::print_dimmed(&format!($($arg)*))
    };
}

/// Move a file or directory to the system trash.
///
/// On macOS, uses `NSFileManager` directly instead of scripting Finder,
/// so trashing does not play Finder sounds, require Automation permission,
/// or leave `.DS_Store` files in the parent directory.
///
/// # Errors
/// Returns an error if the path cannot be moved to the system trash.
pub fn move_to_trash(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let context = {
        use trash::macos::{DeleteMethod, TrashContextExtMacos};

        let mut context = trash::TrashContext::new();
        context.set_delete_method(DeleteMethod::NsFileManager);
        context
    };
    #[cfg(not(target_os = "macos"))]
    let context = trash::TrashContext::new();

    context.delete(path).map_err(std::io::Error::other)
}

/// Delete a file or directory, moving it to the system trash when possible.
///
/// For Windows network paths, uses direct deletion since trash does not work there.
/// For local paths, moves the path to the system trash.
///
/// # Errors
/// Returns an error if the path cannot be deleted or trashed.
pub fn trash_or_delete(path: &Path) -> std::io::Result<()> {
    if is_network_path(path) {
        if std::fs::symlink_metadata(path)?.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        }
    } else {
        move_to_trash(path)
    }
}

/// Create a semaphore for I/O-bound work.
///
/// Uses `num_cpus * 2` permits, which allows enough concurrency for I/O-bound tasks
/// (like running ffprobe) without overwhelming the system.
#[cfg(feature = "async")]
#[must_use]
pub fn create_semaphore_for_io_bound() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(num_cpus::get_physical() * 2))
}

/// Path to the sample config fixture used as the user config during tests.
fn sample_config_fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample_config.toml")
}

/// Whether the directory is inside this cargo workspace, two levels above this crate's manifest directory.
fn is_inside_workspace(directory: &Path) -> bool {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .is_some_and(|workspace_root| directory.starts_with(workspace_root))
}

/// Return whether the current process is a test harness running inside this workspace.
///
/// Cargo sets `CARGO_MANIFEST_DIR` for both `cargo test` and `cargo run`,
/// so `cargo run` is excluded by also requiring either `CARGO_TARGET_TMPDIR`
/// (which Cargo sets for integration tests) or a hashed Cargo test executable path.
fn is_workspace_test_context(
    manifest_dir: Option<&Path>,
    target_tmpdir: Option<&Path>,
    current_exe: Option<&Path>,
) -> bool {
    if !manifest_dir.is_some_and(is_inside_workspace) {
        return false;
    }
    if target_tmpdir.is_some() {
        return true;
    }
    current_exe.is_some_and(exe_is_cargo_test_binary)
}

/// Return whether an executable path looks like a Cargo test binary (`<name>-<16 hex chars>` in `deps` or `build`).
fn exe_is_cargo_test_binary(exe_path: &Path) -> bool {
    let Some(stem) = exe_path.file_stem().and_then(|value| value.to_str()) else {
        return false;
    };
    let has_cargo_hash = stem
        .rsplit_once('-')
        .is_some_and(|(_, hash)| hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
    if !has_cargo_hash {
        return false;
    }
    exe_path
        .components()
        .any(|component| matches!(component.as_os_str().to_str(), Some("deps" | "build")))
}

#[cfg(test)]
mod resolve_path_tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    #[test]
    fn resolve_input_path_valid() {
        let dir = tempdir().unwrap();
        let path = dir.path();
        let resolved = resolve_input_path(Some(path));
        assert!(resolved.is_ok());
    }

    #[test]
    fn resolve_input_path_nonexistent() {
        let path = Path::new("nonexistent");
        let resolved = resolve_input_path(Some(path));
        assert!(resolved.is_err());
    }

    #[test]
    fn resolve_input_path_empty() {
        let path = Path::new("  \n");
        let resolved = resolve_input_path(Some(path));
        assert!(resolved.is_ok());
        assert_eq!(resolved.unwrap(), env::current_dir().unwrap());
    }

    #[test]
    fn resolve_input_path_default() {
        let resolved = resolve_input_path(None);
        assert!(resolved.is_ok());
        assert_eq!(resolved.unwrap(), env::current_dir().unwrap());
    }

    #[test]
    fn resolve_required_input_path_valid() {
        let dir = tempdir().unwrap();
        let path = dir.path();
        let resolved = resolve_required_input_path(path);
        assert!(resolved.is_ok());
    }

    #[test]
    fn resolve_required_input_path_nonexistent() {
        let path = Path::new("nonexistent");
        let resolved = resolve_required_input_path(path);
        assert!(resolved.is_err());
    }

    #[test]
    fn resolve_required_input_path_empty() {
        let path = Path::new("");
        let resolved = resolve_required_input_path(path);
        assert!(resolved.is_err());
        assert!(resolved.unwrap_err().to_string().contains("cannot be empty"));
    }

    #[test]
    fn resolve_required_input_path_whitespace() {
        let path = Path::new("  \n");
        let resolved = resolve_required_input_path(path);
        assert!(resolved.is_err());
        assert!(resolved.unwrap_err().to_string().contains("cannot be empty"));
    }

    #[test]
    fn resolve_output_path_with_file() {
        let input_dir = tempdir().unwrap();
        let output_dir = tempdir().unwrap();
        let output_string = output_dir.path().to_str().unwrap().to_string();

        let input_file = input_dir.path().join("input.txt");
        File::create(&input_file).unwrap();

        let output_path = resolve_output_path(Some(output_string.as_str()), &input_file);
        assert!(output_path.is_ok());
        assert_eq!(output_path.unwrap(), dunce::simplified(output_dir.path()));
    }

    #[test]
    fn resolve_output_path_default() {
        let dir = tempdir().unwrap();
        let output_path = resolve_output_path(None, dir.path());
        assert!(output_path.is_ok());
        assert_eq!(output_path.unwrap(), dunce::simplified(dir.path()));
    }
}

#[cfg(all(test, unix))]
mod test_resolve_non_utf8_path {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn missing_non_utf8_path_is_an_error_not_the_current_directory() {
        let path = Path::new(OsStr::from_bytes(b"/nonexistent-cli-tools-test/\xff\xfe"));

        let resolved = resolve_input_path(Some(path));

        assert!(resolved.is_err(), "resolved to {resolved:?}");
    }

    #[test]
    fn missing_non_utf8_required_path_reports_missing_path() {
        let path = Path::new(OsStr::from_bytes(b"/nonexistent-cli-tools-test/\xff\xfe"));

        let error = resolve_required_input_path(path).expect_err("should fail");

        assert!(error.to_string().contains("does not exist"), "{error}");
    }
}

#[cfg(test)]
mod system_directory_tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;
    use walkdir::WalkDir;

    #[test]
    #[cfg(unix)]
    fn is_system_directory_path_recycle_bin_unix() {
        let path = Path::new("/mnt/d/$RECYCLE.BIN");
        assert!(is_system_directory_path(path));
    }

    #[test]
    #[cfg(unix)]
    fn is_system_directory_path_recycle_bin_case_insensitive_unix() {
        let path = Path::new("/mnt/e/$Recycle.Bin");
        assert!(is_system_directory_path(path));
    }

    #[test]
    #[cfg(unix)]
    fn is_system_directory_path_system_volume_information_unix() {
        let path = Path::new("/mnt/c/System Volume Information");
        assert!(is_system_directory_path(path));
    }

    #[test]
    #[cfg(unix)]
    fn is_system_directory_path_normal_directory_unix() {
        let path = Path::new("/home/user/Documents");
        assert!(!is_system_directory_path(path));
    }

    #[test]
    #[cfg(unix)]
    fn is_system_directory_path_similar_name_unix() {
        // Without the $ prefix, this should NOT match $RECYCLE.BIN
        let path = Path::new("/mnt/c/RECYCLE.BIN");
        assert!(!is_system_directory_path(path));
    }

    #[test]
    #[cfg(windows)]
    fn is_system_directory_path_recycle_bin_windows() {
        let path = Path::new("D:\\$RECYCLE.BIN");
        assert!(is_system_directory_path(path));
    }

    #[test]
    #[cfg(windows)]
    fn is_system_directory_path_recycle_bin_case_insensitive_windows() {
        let path = Path::new("E:\\$Recycle.Bin");
        assert!(is_system_directory_path(path));
    }

    #[test]
    #[cfg(windows)]
    fn is_system_directory_path_system_volume_information_windows() {
        let path = Path::new("C:\\System Volume Information");
        assert!(is_system_directory_path(path));
    }

    #[test]
    #[cfg(windows)]
    fn is_system_directory_path_normal_directory_windows() {
        let path = Path::new("C:\\Users\\Documents");
        assert!(!is_system_directory_path(path));
    }

    #[test]
    #[cfg(windows)]
    fn is_system_directory_path_similar_name_windows() {
        // Without the $ prefix, this should NOT match $RECYCLE.BIN
        let path = Path::new("C:\\RECYCLE.BIN");
        assert!(!is_system_directory_path(path));
    }

    #[test]
    fn is_system_directory_walkdir() {
        let dir = tempdir().unwrap();
        let recycle_bin = dir.path().join("$RECYCLE.BIN");
        std::fs::create_dir(&recycle_bin).unwrap();

        for entry in WalkDir::new(dir.path()).min_depth(1) {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy() == "$RECYCLE.BIN" {
                assert!(is_system_directory(&entry));
            }
        }
    }

    #[test]
    fn should_skip_entry_system_dir() {
        let dir = tempdir().unwrap();
        let recycle_bin = dir.path().join("$RECYCLE.BIN");
        std::fs::create_dir(&recycle_bin).unwrap();

        for entry in WalkDir::new(dir.path()).min_depth(1) {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy() == "$RECYCLE.BIN" {
                assert!(should_skip_entry(&entry));
            }
        }
    }

    #[test]
    fn should_skip_entry_hidden_file() {
        let dir = tempdir().unwrap();
        let hidden = dir.path().join(".hidden");
        File::create(&hidden).unwrap();

        for entry in WalkDir::new(dir.path()).min_depth(1) {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy() == ".hidden" {
                assert!(should_skip_entry(&entry));
            }
        }
    }

    #[test]
    fn should_skip_entry_normal_file() {
        let dir = tempdir().unwrap();
        let normal = dir.path().join("normal.txt");
        File::create(&normal).unwrap();

        for entry in WalkDir::new(dir.path()).min_depth(1) {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy() == "normal.txt" {
                assert!(!should_skip_entry(&entry));
            }
        }
    }

    #[test]
    fn should_skip_entry_keeps_hidden_walk_root() {
        let dir = tempdir().expect("Failed to create temp dir");
        let hidden_root = dir.path().join(".hidden_root");
        let hidden_child = hidden_root.join(".hidden_child");
        std::fs::create_dir_all(&hidden_child).expect("Failed to create hidden directories");
        File::create(hidden_root.join("visible.txt")).expect("Failed to create file");

        let visited: Vec<PathBuf> = WalkDir::new(&hidden_root)
            .into_iter()
            .filter_entry(|entry| !should_skip_entry(entry))
            .map(|entry| entry.expect("Failed to read entry").into_path())
            .collect();

        assert!(visited.contains(&hidden_root));
        assert!(visited.contains(&hidden_root.join("visible.txt")));
        assert!(!visited.contains(&hidden_child));
    }

    #[test]
    fn is_system_directory_macos_spotlight() {
        let dir = tempdir().unwrap();
        let spotlight = dir.path().join(".Spotlight-V100");
        std::fs::create_dir(&spotlight).unwrap();

        for entry in WalkDir::new(dir.path()).min_depth(1) {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy() == ".Spotlight-V100" {
                assert!(is_system_directory(&entry));
            }
        }
    }

    #[test]
    fn is_system_directory_macos_trashes() {
        let dir = tempdir().unwrap();
        let trashes = dir.path().join(".Trashes");
        std::fs::create_dir(&trashes).unwrap();

        for entry in WalkDir::new(dir.path()).min_depth(1) {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy() == ".Trashes" {
                assert!(is_system_directory(&entry));
            }
        }
    }

    #[test]
    fn is_system_directory_linux_lost_found() {
        let dir = tempdir().unwrap();
        let lost_found = dir.path().join("lost+found");
        std::fs::create_dir(&lost_found).unwrap();

        for entry in WalkDir::new(dir.path()).min_depth(1) {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy() == "lost+found" {
                assert!(is_system_directory(&entry));
            }
        }
    }

    #[test]
    fn is_system_directory_serato() {
        let dir = tempdir().unwrap();
        let serato = dir.path().join("_Serato_");
        std::fs::create_dir(&serato).unwrap();

        for entry in WalkDir::new(dir.path()).min_depth(1) {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy() == "_Serato_" {
                assert!(is_system_directory(&entry));
            }
        }
    }

    #[test]
    fn is_system_directory_path_serato() {
        let path = Path::new("/music/_Serato_");
        assert!(is_system_directory_path(path));
    }

    #[test]
    fn should_skip_entry_serato() {
        let dir = tempdir().unwrap();
        let serato = dir.path().join("_Serato_");
        std::fs::create_dir(&serato).unwrap();

        for entry in WalkDir::new(dir.path()).min_depth(1) {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy() == "_Serato_" {
                assert!(should_skip_entry(&entry));
            }
        }
    }

    #[test]
    fn is_system_directory_nas_recycle() {
        let dir = tempdir().unwrap();
        let recycle = dir.path().join("#recycle");
        std::fs::create_dir(&recycle).unwrap();

        for entry in WalkDir::new(dir.path()).min_depth(1) {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy() == "#recycle" {
                assert!(is_system_directory(&entry));
            }
        }
    }

    #[test]
    fn is_system_directory_nas_recycle_mixed_case() {
        let dir = tempdir().unwrap();
        let recycle = dir.path().join("#Recycle");
        std::fs::create_dir(&recycle).unwrap();

        for entry in WalkDir::new(dir.path()).min_depth(1) {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy() == "#Recycle" {
                assert!(is_system_directory(&entry));
            }
        }
    }

    #[test]
    fn is_system_directory_path_nas_recycle() {
        let path = Path::new("/volume1/share/#recycle");
        assert!(is_system_directory_path(path));
    }

    #[test]
    fn is_system_directory_path_nas_recycle_mixed_case() {
        let path = Path::new("/volume1/share/#Recycle");
        assert!(is_system_directory_path(path));
    }

    #[test]
    fn should_skip_entry_nas_recycle() {
        let dir = tempdir().unwrap();
        let recycle = dir.path().join("#recycle");
        std::fs::create_dir(&recycle).unwrap();

        for entry in WalkDir::new(dir.path()).min_depth(1) {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy() == "#recycle" {
                assert!(should_skip_entry(&entry));
            }
        }
    }
}

#[cfg(test)]
mod repeated_separator_tests {
    use super::*;

    #[test]
    fn leaves_empty_string_empty() {
        assert_eq!(collapse_repeated_separators(""), "");
    }

    #[test]
    fn leaves_string_without_separators_unchanged() {
        assert_eq!(collapse_repeated_separators("MovieTitle2024"), "MovieTitle2024");
    }

    #[test]
    fn collapses_repeated_dots() {
        assert_eq!(collapse_repeated_separators("movie..title"), "movie.title");
        assert_eq!(collapse_repeated_separators("movie....title"), "movie.title");
    }

    #[test]
    fn collapses_repeated_hyphens() {
        assert_eq!(collapse_repeated_separators("movie--title"), "movie-title");
        assert_eq!(collapse_repeated_separators("movie----title"), "movie-title");
    }

    #[test]
    fn collapses_repeated_underscores() {
        assert_eq!(collapse_repeated_separators("movie__title"), "movie_title");
        assert_eq!(collapse_repeated_separators("movie____title"), "movie_title");
    }

    #[test]
    fn collapses_repeated_spaces() {
        assert_eq!(collapse_repeated_separators("movie  title"), "movie title");
        assert_eq!(collapse_repeated_separators("movie    title"), "movie title");
    }

    #[test]
    fn collapses_repeated_tabs_and_newlines() {
        assert_eq!(collapse_repeated_separators("movie\t\ttitle"), "movie\ttitle");
        assert_eq!(collapse_repeated_separators("movie\n\n\ntitle"), "movie\ntitle");
    }

    #[test]
    fn collapses_mixed_separator_sequence_to_first_separator() {
        assert_eq!(collapse_repeated_separators("movie.-_ title"), "movie.title");
        assert_eq!(collapse_repeated_separators("movie_-. title"), "movie_title");
        assert_eq!(collapse_repeated_separators("movie -_.title"), "movie title");
    }

    #[test]
    fn collapses_multiple_sequences_independently() {
        assert_eq!(
            collapse_repeated_separators("movie..title__2024--1080p"),
            "movie.title_2024-1080p"
        );
    }

    #[test]
    fn trims_separators_from_ends() {
        assert_eq!(collapse_repeated_separators("--movie.title__"), "movie.title");
        assert_eq!(collapse_repeated_separators("  movie.title  "), "movie.title");
        assert_eq!(collapse_repeated_separators("..movie.title.."), "movie.title");
    }

    #[test]
    fn all_separators_becomes_empty() {
        assert_eq!(collapse_repeated_separators("..."), "");
        assert_eq!(collapse_repeated_separators("---___   "), "");
    }

    #[test]
    fn preserves_single_inner_separators() {
        assert_eq!(
            collapse_repeated_separators("movie-title_part one.two"),
            "movie-title_part one.two"
        );
    }

    #[test]
    fn preserves_unicode_text() {
        assert_eq!(collapse_repeated_separators("Tähtien..Sota"), "Tähtien.Sota");
        assert_eq!(collapse_repeated_separators("映画__タイトル"), "映画_タイトル");
    }

    #[test]
    fn handles_realistic_codec_label_removal_leftovers() {
        assert_eq!(collapse_repeated_separators("Movie..Title..1080p"), "Movie.Title.1080p");
        assert_eq!(collapse_repeated_separators("Movie.Title."), "Movie.Title");
        assert_eq!(collapse_repeated_separators(".Movie.Title"), "Movie.Title");
    }
}

#[cfg(test)]
mod format_size_tests {
    use super::*;

    #[test]
    fn bytes_only() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1), "1 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn kilobytes() {
        assert_eq!(format_size(1024), "1.00 KB");
        assert_eq!(format_size(1536), "1.50 KB");
        assert_eq!(format_size(10240), "10.00 KB");
    }

    #[test]
    fn megabytes() {
        assert_eq!(format_size(1024 * 1024), "1.00 MB");
        assert_eq!(format_size(1024 * 1024 + 512 * 1024), "1.50 MB");
        assert_eq!(format_size(100 * 1024 * 1024), "100.00 MB");
    }

    #[test]
    fn gigabytes() {
        assert_eq!(format_size(1024 * 1024 * 1024), "1.00 GB");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2.00 GB");
    }

    #[test]
    fn terabytes() {
        assert_eq!(format_size(1024 * 1024 * 1024 * 1024), "1.00 TB");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024 * 1024), "2.00 TB");
    }

    #[test]
    fn formats_bytes_below_one_kilobyte() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1), "1 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn formats_kilobytes() {
        assert_eq!(format_size(1024), "1.00 KB");
        assert_eq!(format_size(1536), "1.50 KB");
        assert_eq!(format_size(2048), "2.00 KB");
        assert_eq!(format_size(1024 * 1023), "1023.00 KB");
    }

    #[test]
    fn formats_megabytes() {
        assert_eq!(format_size(1_048_576), "1.00 MB");
        assert_eq!(format_size(1_572_864), "1.50 MB");
        assert_eq!(format_size(10_485_760), "10.00 MB");
        assert_eq!(format_size(104_857_600), "100.00 MB");
    }

    #[test]
    fn formats_gigabytes() {
        assert_eq!(format_size(1_073_741_824), "1.00 GB");
        assert_eq!(format_size(1_610_612_736), "1.50 GB");
        assert_eq!(format_size(10_737_418_240), "10.00 GB");
        assert_eq!(format_size(107_374_182_400), "100.00 GB");
    }

    #[test]
    fn formats_terabytes() {
        assert_eq!(format_size(1_099_511_627_776), "1.00 TB");
        assert_eq!(format_size(1_649_267_441_664), "1.50 TB");
        assert_eq!(format_size(10_995_116_277_760), "10.00 TB");
    }

    #[test]
    fn formats_petabyte_range_values_as_terabytes() {
        assert_eq!(format_size(1_125_899_906_842_624), "1024.00 TB");
        assert_eq!(format_size(u64::MAX), "16777216.00 TB");
    }

    #[test]
    fn formats_values_just_below_each_unit_boundary() {
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024 * 1024 - 1), "1024.00 KB");
        assert_eq!(format_size(1024 * 1024 * 1024 - 1), "1024.00 MB");
        assert_eq!(format_size(1024u64 * 1024 * 1024 * 1024 - 1), "1024.00 GB");
    }

    #[test]
    fn formats_two_decimal_places() {
        assert_eq!(format_size(1536), "1.50 KB");
        // 2047 / 1024 = 1.9990234375, which rounds to two decimals
        assert_eq!(format_size(2047), "2.00 KB");
    }
}

#[cfg(test)]
mod format_number_tests {
    use super::*;

    #[test]
    fn keeps_numbers_below_one_thousand() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(7), "7");
        assert_eq!(format_number(999), "999");
    }

    #[test]
    fn separates_thousands() {
        assert_eq!(format_number(1_000), "1,000");
        assert_eq!(format_number(12_345), "12,345");
        assert_eq!(format_number(123_456), "123,456");
        assert_eq!(format_number(1_234_567), "1,234,567");
    }

    #[test]
    fn formats_the_largest_value() {
        assert_eq!(format_number(u64::MAX), "18,446,744,073,709,551,615");
    }

    #[test]
    fn formats_zero() {
        assert_eq!(format_number(0), "0");
    }

    #[test]
    fn formats_small_numbers_without_separators() {
        assert_eq!(format_number(1), "1");
        assert_eq!(format_number(12), "12");
        assert_eq!(format_number(123), "123");
        assert_eq!(format_number(999), "999");
    }

    #[test]
    fn formats_thousands() {
        assert_eq!(format_number(1000), "1,000");
        assert_eq!(format_number(1234), "1,234");
        assert_eq!(format_number(12345), "12,345");
        assert_eq!(format_number(123_456), "123,456");
    }

    #[test]
    fn formats_millions() {
        assert_eq!(format_number(1_000_000), "1,000,000");
        assert_eq!(format_number(1_234_567), "1,234,567");
        assert_eq!(format_number(12_345_678), "12,345,678");
        assert_eq!(format_number(123_456_789), "123,456,789");
    }

    #[test]
    fn formats_billions() {
        assert_eq!(format_number(1_000_000_000), "1,000,000,000");
        assert_eq!(format_number(1_234_567_890), "1,234,567,890");
    }
}

#[cfg(test)]
mod format_duration_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn seconds_only() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(1)), "1s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn minutes_and_seconds() {
        assert_eq!(format_duration(Duration::from_mins(1)), "1m 00s");
        assert_eq!(format_duration(Duration::from_secs(90)), "1m 30s");
        assert_eq!(format_duration(Duration::from_secs(3599)), "59m 59s");
    }

    #[test]
    fn hours_minutes_seconds() {
        assert_eq!(format_duration(Duration::from_hours(1)), "1h 00m 00s");
        assert_eq!(format_duration(Duration::from_secs(3661)), "1h 01m 01s");
        assert_eq!(format_duration(Duration::from_secs(7325)), "2h 02m 05s");
    }

    #[test]
    fn format_duration_seconds_basic() {
        assert_eq!(format_duration_seconds(0.0), "0.0s");
        assert_eq!(format_duration_seconds(1.5), "1.5s");
        assert_eq!(format_duration_seconds(59.9), "59.9s");
    }

    #[test]
    fn format_duration_seconds_minutes() {
        assert_eq!(format_duration_seconds(60.0), "1m 00s");
        assert_eq!(format_duration_seconds(90.0), "1m 30s");
    }

    #[test]
    fn format_duration_seconds_hours() {
        assert_eq!(format_duration_seconds(3600.0), "1h 00m 00s");
        assert_eq!(format_duration_seconds(3661.0), "1h 01m 01s");
    }

    #[test]
    fn format_duration_seconds_negative() {
        assert_eq!(format_duration_seconds(-10.0), "0.0s");
    }
}

#[cfg(test)]
mod path_utility_tests {
    use super::*;

    use std::fs::File;
    use tempfile::tempdir;

    #[test]
    fn append_extension_to_path_basic() {
        let path = Path::new("file.txt");
        let result = append_extension_to_path(path, "bak");
        assert_eq!(result, PathBuf::from("file.txt.bak"));
    }

    #[test]
    fn append_extension_to_path_no_extension() {
        let path = Path::new("README");
        let result = append_extension_to_path(path, "md");
        assert_eq!(result, PathBuf::from("README.md"));
    }

    #[test]
    fn append_extension_to_path_with_directory() {
        let path = Path::new("dir/subdir/file.txt");
        let result = append_extension_to_path(path, "backup");
        assert_eq!(result, PathBuf::from("dir/subdir/file.txt.backup"));
    }

    #[test]
    fn insert_suffix_before_extension_basic() {
        let path = Path::new("video.mp4");
        let result = insert_suffix_before_extension(path, ".x265");
        assert_eq!(result, PathBuf::from("video.x265.mp4"));
    }

    #[test]
    fn insert_suffix_before_extension_no_extension() {
        let path = Path::new("README");
        let result = insert_suffix_before_extension(path, ".backup");
        assert_eq!(result, PathBuf::from("README.backup"));
    }

    #[test]
    fn insert_suffix_before_extension_with_directory() {
        let path = Path::new("subdir/video.mp4");
        let result = insert_suffix_before_extension(path, ".converted");
        assert_eq!(result, PathBuf::from("subdir/video.converted.mp4"));
    }

    #[test]
    fn insert_suffix_before_extension_multiple_dots() {
        let path = Path::new("video.1080p.mp4");
        let result = insert_suffix_before_extension(path, ".x265");
        assert_eq!(result, PathBuf::from("video.1080p.x265.mp4"));
    }

    #[test]
    fn get_unique_path_no_conflict() {
        let dir = tempdir().unwrap();
        let result = get_unique_path(dir.path(), "test.txt", "test", "txt");
        assert_eq!(result, dir.path().join("test.txt"));
    }

    #[test]
    fn get_unique_path_with_conflict() {
        let dir = tempdir().unwrap();
        File::create(dir.path().join("test.txt")).unwrap();

        let result = get_unique_path(dir.path(), "test.txt", "test", "txt");
        assert_eq!(result, dir.path().join("test.1.txt"));
    }

    #[test]
    fn get_unique_path_multiple_conflicts() {
        let dir = tempdir().unwrap();
        File::create(dir.path().join("test.txt")).unwrap();
        File::create(dir.path().join("test.1.txt")).unwrap();
        File::create(dir.path().join("test.2.txt")).unwrap();

        let result = get_unique_path(dir.path(), "test.txt", "test", "txt");
        assert_eq!(result, dir.path().join("test.3.txt"));
    }

    #[test]
    fn get_unique_path_no_extension() {
        let dir = tempdir().unwrap();
        File::create(dir.path().join("README")).unwrap();

        let result = get_unique_path(dir.path(), "README", "README", "");
        assert_eq!(result, dir.path().join("README.1"));
    }

    #[test]
    fn paths_refer_to_same_file_for_same_path() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "content").unwrap();

        assert!(paths_refer_to_same_file(&path, &path));
    }

    #[test]
    fn paths_refer_to_same_file_for_equivalent_paths() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "content").unwrap();
        let equivalent = dir.path().join(".").join("test.txt");

        assert!(paths_refer_to_same_file(&path, &equivalent));
    }

    #[test]
    fn paths_refer_to_same_file_returns_false_for_different_paths() {
        let dir = tempdir().unwrap();
        let left = dir.path().join("left.txt");
        let right = dir.path().join("right.txt");
        std::fs::write(&left, "left").unwrap();
        std::fs::write(&right, "right").unwrap();

        assert!(!paths_refer_to_same_file(&left, &right));
    }

    #[test]
    fn get_normalized_file_name_and_extension_basic() {
        let path = Path::new("test/file.txt");
        let (stem, ext) = get_normalized_file_name_and_extension(path).unwrap();
        assert_eq!(stem, "file");
        assert_eq!(ext, "txt");
    }

    #[test]
    fn get_normalized_file_name_and_extension_no_extension() {
        let path = Path::new("README");
        let (stem, ext) = get_normalized_file_name_and_extension(path).unwrap();
        assert_eq!(stem, "README");
        assert_eq!(ext, "");
    }

    #[test]
    fn get_normalized_dir_name_basic() {
        let path = Path::new("parent/subdir");
        let name = get_normalized_dir_name(path).unwrap();
        assert_eq!(name, "subdir");
    }
}

#[cfg(test)]
mod directory_utility_tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    #[test]
    fn is_directory_empty_true() {
        let dir = tempdir().unwrap();
        assert!(is_directory_empty(dir.path()));
    }

    #[test]
    fn is_directory_empty_with_file() {
        let dir = tempdir().unwrap();
        File::create(dir.path().join("file.txt")).unwrap();
        assert!(!is_directory_empty(dir.path()));
    }

    #[test]
    fn is_directory_empty_with_subdir() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        assert!(!is_directory_empty(dir.path()));
    }

    #[test]
    fn is_directory_empty_false_for_missing_path_and_regular_file() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("missing");
        let file_path = dir.path().join("file.txt");
        File::create(&file_path).unwrap();

        assert!(!is_directory_empty(&missing));
        assert!(!is_directory_empty(&file_path));
    }

    #[cfg(unix)]
    #[test]
    fn is_directory_empty_false_for_unreadable_non_empty_directory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        File::create(locked.join("secret.txt")).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let empty = is_directory_empty(&locked);

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!empty);
    }
}

#[cfg(test)]
mod colorize_bool_tests {
    use super::*;

    #[test]
    fn colorize_true() {
        let result = colorize_bool(true);
        assert!(result.to_string().contains("true"));
    }

    #[test]
    fn colorize_false() {
        let result = colorize_bool(false);
        assert!(result.to_string().contains("false"));
    }
}

#[cfg(test)]
mod assert_f64_eq_tests {
    use super::*;

    #[test]
    fn equal_values() {
        assert_f64_eq(1.0, 1.0);
        assert_f64_eq(0.0, 0.0);
        assert_f64_eq(-1.0, -1.0);
    }

    #[test]
    fn very_close_values() {
        assert_f64_eq(1.0, 1.0 + f64::EPSILON / 2.0);
    }

    #[test]
    #[should_panic(expected = "Values are not equal")]
    fn different_values() {
        assert_f64_eq(1.0, 2.0);
    }

    #[test]
    #[should_panic(expected = "Values are not equal")]
    fn close_values() {
        assert_f64_eq(1.0, 1.0 + f64::EPSILON + f64::EPSILON);
    }
}

#[cfg(test)]
mod hidden_file_tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;
    use walkdir::WalkDir;

    #[test]
    fn is_hidden_file() {
        let dir = tempdir().unwrap();
        let hidden_file_path = dir.path().join(".hidden");
        File::create(hidden_file_path).unwrap();

        let entry = WalkDir::new(dir.path())
            .into_iter()
            .filter_map(Result::ok)
            .find(|e| e.file_name().to_string_lossy().eq(".hidden"))
            .unwrap();

        assert!(is_hidden(&entry));
    }

    #[test]
    fn is_not_hidden_file() {
        let dir = tempdir().unwrap();
        let normal_file_path = dir.path().join("visible");
        File::create(normal_file_path).unwrap();

        let entry = WalkDir::new(dir.path())
            .into_iter()
            .filter_map(Result::ok)
            .find(|e| e.file_name().to_string_lossy().eq("visible"))
            .unwrap();

        assert!(!is_hidden(&entry));
    }

    #[test]
    fn is_hidden_directory() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".hidden_dir")).unwrap();

        let entry = WalkDir::new(dir.path())
            .into_iter()
            .filter_map(Result::ok)
            .find(|e| e.file_name().to_string_lossy().eq(".hidden_dir"))
            .unwrap();

        assert!(is_hidden(&entry));
    }
}

#[cfg(test)]
mod trash_or_delete_tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    #[test]
    fn deletes_local_file() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test_file.txt");
        File::create(&file_path).unwrap();

        assert!(file_path.exists());
        trash_or_delete(&file_path).unwrap();
        assert!(!file_path.exists());
        let remaining: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(
            remaining.is_empty(),
            "trashing a file must not leave extra files in the parent directory: {remaining:?}"
        );
    }

    #[test]
    fn deletes_local_directory_without_parent_side_effects() {
        let dir = tempdir().unwrap();
        let subdir = dir.path().join(".unwanted");
        std::fs::create_dir(&subdir).unwrap();
        File::create(subdir.join("junk.txt")).unwrap();

        trash_or_delete(&subdir).unwrap();
        assert!(!subdir.exists());
        let remaining: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(
            remaining.is_empty(),
            "trashing a directory must not leave extra files in the parent directory: {remaining:?}"
        );
    }

    #[test]
    fn returns_error_for_nonexistent_file() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("nonexistent.txt");

        let result = trash_or_delete(&file_path);
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod network_path_tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn detects_unc_path() {
        let path = Path::new(r"\\server\share\file.txt");
        assert!(is_network_path(path));
    }

    #[test]
    #[cfg(windows)]
    fn local_path_is_not_network() {
        let path = Path::new(r"C:\Users\test\file.txt");
        // Local drives should not be detected as network paths
        // (unless they are actually mapped network drives)
        assert!(!is_network_path(path));
    }

    #[test]
    fn classifies_verbatim_local_device_and_unc_prefixes() {
        assert_eq!(
            classify_windows_path_prefix(r"\\server\share\file.txt"),
            WindowsPathPrefix::Unc
        );
        assert_eq!(
            classify_windows_path_prefix("//server/share/file.txt"),
            WindowsPathPrefix::Unc
        );
        assert_eq!(
            classify_windows_path_prefix(r"\\?\UNC\server\share\file.txt"),
            WindowsPathPrefix::Unc
        );
        assert_eq!(
            classify_windows_path_prefix(r"\\?\C:\Users\test\file.txt"),
            WindowsPathPrefix::DriveLetter(b'C')
        );
        assert_eq!(
            classify_windows_path_prefix(r"\\.\C:\Users\test\file.txt"),
            WindowsPathPrefix::DriveLetter(b'C')
        );
        assert_eq!(
            classify_windows_path_prefix(r"\\.\PhysicalDrive0"),
            WindowsPathPrefix::Other
        );
        assert_eq!(
            classify_windows_path_prefix(r"d:\Media\video.mp4"),
            WindowsPathPrefix::DriveLetter(b'D')
        );
    }

    #[test]
    #[cfg(windows)]
    fn detects_forward_slash_and_verbatim_unc_paths() {
        assert!(is_network_path(Path::new("//server/share/file.txt")));
        assert!(is_network_path(Path::new(r"\\?\UNC\server\share\file.txt")));
    }

    #[test]
    #[cfg(windows)]
    fn local_relative_and_temporary_paths_are_not_network() {
        let temp = tempfile::tempdir().unwrap();
        assert!(!is_network_path(temp.path()));
        assert!(!is_network_path(&temp.path().join("missing").join("file.txt")));
        let current = std::env::current_dir().unwrap();
        if !is_network_path(&current) {
            assert!(!is_network_path(Path::new("relative/file.txt")));
            assert!(!is_network_path(Path::new("file.txt")));
        }
    }

    #[test]
    #[cfg(windows)]
    fn verbatim_local_path_is_not_network() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(temp.path()).unwrap();
        assert!(canonical.to_string_lossy().starts_with(r"\\?\"));
        assert!(!is_network_path(&canonical));
    }

    #[test]
    #[cfg(not(windows))]
    fn non_windows_always_false() {
        let path = Path::new("/home/user/file.txt");
        assert!(!is_network_path(path));
    }
}

#[cfg(test)]
mod additional_path_utility_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn get_unique_path_returns_original_when_no_conflict() {
        let dir = tempdir().unwrap();
        let result = get_unique_path(dir.path(), "newfile.txt", "newfile", "txt");
        assert_eq!(result, dir.path().join("newfile.txt"));
    }

    #[test]
    fn get_unique_path_increments_counter() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "content").unwrap();

        let result = get_unique_path(dir.path(), "file.txt", "file", "txt");
        assert_eq!(result, dir.path().join("file.1.txt"));
    }

    #[test]
    fn get_unique_path_increments_multiple_times() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "content").unwrap();
        std::fs::write(dir.path().join("file.1.txt"), "content").unwrap();
        std::fs::write(dir.path().join("file.2.txt"), "content").unwrap();

        let result = get_unique_path(dir.path(), "file.txt", "file", "txt");
        assert_eq!(result, dir.path().join("file.3.txt"));
    }

    #[test]
    fn get_unique_path_does_not_collide_with_dangling_symlink() {
        let dir = tempdir().unwrap();
        let dangling = dir.path().join("report.pdf");
        if !crate::test_support::create_symlink(&dir.path().join("missing.pdf"), &dangling) {
            return;
        }

        let result = get_unique_path(dir.path(), "report.pdf", "report", "pdf");
        assert_eq!(result, dir.path().join("report.1.pdf"));
    }

    #[cfg(unix)]
    #[test]
    fn insert_suffix_before_extension_preserves_non_utf8_bytes() {
        use std::os::unix::ffi::OsStrExt;

        let path = Path::new(OsStr::from_bytes(b"dir/movie\xff.mp4"));
        let result = insert_suffix_before_extension(path, ".x265");
        assert_eq!(result.as_os_str().as_bytes(), b"dir/movie\xff.x265.mp4");
    }

    #[test]
    fn get_unique_path_handles_no_extension() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("file"), "content").unwrap();

        let result = get_unique_path(dir.path(), "file", "file", "");
        assert_eq!(result, dir.path().join("file.1"));
    }

    #[test]
    fn append_extension_preserves_directory() {
        let path = Path::new("/dir/subdir/file.txt");
        let result = append_extension_to_path(path, "bak");
        assert_eq!(result, PathBuf::from("/dir/subdir/file.txt.bak"));
    }

    #[test]
    fn insert_suffix_preserves_directory() {
        let path = Path::new("/dir/subdir/file.txt");
        let result = insert_suffix_before_extension(path, ".backup");
        assert_eq!(result, PathBuf::from("/dir/subdir/file.backup.txt"));
    }
}

#[cfg(test)]
mod print_function_tests {
    use super::*;

    #[test]
    fn print_error_does_not_panic() {
        print_error("test error message");
    }

    #[test]
    fn print_yellow_does_not_panic() {
        print_yellow("test warning message");
    }

    #[test]
    fn print_green_does_not_panic() {
        print_green("test green message");
    }

    #[test]
    fn print_magenta_does_not_panic() {
        print_magenta("test magenta message");
    }

    #[test]
    fn print_magenta_bold_does_not_panic() {
        print_magenta_bold("test magenta bold message");
    }

    #[test]
    fn print_cyan_does_not_panic() {
        print_cyan("test cyan message");
    }

    #[test]
    fn print_bold_does_not_panic() {
        print_bold("test bold message");
    }

    #[test]
    fn print_dimmed_does_not_panic() {
        print_dimmed("test dimmed message");
    }
}

#[cfg(test)]
mod text_highlight_tests {
    use super::*;

    #[test]
    fn match_range_extracts_selected_substring() {
        let range = MatchRange { start: 6, end: 11 };
        assert_eq!(range.extract_from("hello world"), Some("world"));
    }

    #[test]
    fn format_text_without_range_returns_original_text() {
        assert_eq!(format_text_with_highlight("hello world", None), "hello world");
    }

    #[test]
    fn format_text_with_range_preserves_prefix_match_and_suffix() {
        let formatted = format_text_with_highlight("before MATCH after", Some(MatchRange { start: 7, end: 12 }));

        assert!(formatted.starts_with("before "));
        assert!(formatted.contains("MATCH"));
        assert!(formatted.ends_with(" after"));
    }
}

#[cfg(test)]
mod additional_shared_helper_tests {
    use super::*;

    #[test]
    fn pluralization_handles_zero_one_and_many() {
        assert_eq!(pluralize(0, "file", "files"), "files");
        assert_eq!(pluralize(1, "file", "files"), "file");
        assert_eq!(pluralize(2, "file", "files"), "files");
        assert_eq!(count_label(0, "file", "files"), "0 files");
        assert_eq!(count_label(2, "file", "files"), "2 files");
    }

    #[test]
    fn resolve_output_path_defaults_to_input_file_parent() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let input_file = temp_directory.path().join("input.txt");
        std::fs::write(&input_file, b"input")?;

        let output = resolve_output_path(None, &input_file)?;
        let whitespace_output = resolve_output_path(Some("  \n"), &input_file)?;

        assert_eq!(output, dunce::simplified(temp_directory.path()));
        assert_eq!(whitespace_output, output);
        Ok(())
    }

    #[test]
    fn missing_paths_do_not_refer_to_same_file() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        let missing_left = temp_directory.path().join("missing-left");
        let missing_right = temp_directory.path().join("missing-right");

        assert!(!paths_refer_to_same_file(&missing_left, &missing_right));
    }

    #[test]
    fn formats_valid_and_out_of_range_timestamps() {
        assert_ne!(format_timestamp(0), "unknown");
        assert_eq!(format_timestamp(i64::MAX), "unknown");
    }

    #[cfg(feature = "async")]
    #[test]
    fn io_semaphore_uses_twice_the_physical_cpu_count() {
        let semaphore = create_semaphore_for_io_bound();
        assert_eq!(semaphore.available_permits(), num_cpus::get_physical() * 2);
    }
}

#[cfg(all(test, feature = "async"))]
mod tokio_directory_entry_tests {
    use super::*;

    #[tokio::test]
    async fn identifies_hidden_visible_system_and_normal_entries() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        tokio::fs::write(temp_directory.path().join(".hidden"), b"hidden").await?;
        tokio::fs::write(temp_directory.path().join("visible.txt"), b"visible").await?;
        tokio::fs::create_dir(temp_directory.path().join("lost+found")).await?;
        tokio::fs::create_dir(temp_directory.path().join("ordinary")).await?;
        let mut entries = tokio::fs::read_dir(temp_directory.path()).await?;
        let mut hidden_found = false;
        let mut visible_found = false;
        let mut system_found = false;
        let mut ordinary_found = false;

        while let Some(entry) = entries.next_entry().await? {
            match entry.file_name().to_string_lossy().as_ref() {
                ".hidden" => hidden_found = is_hidden_tokio(&entry),
                "visible.txt" => visible_found = !is_hidden_tokio(&entry),
                "lost+found" => system_found = is_system_directory_tokio(&entry),
                "ordinary" => ordinary_found = !is_system_directory_tokio(&entry),
                _ => {}
            }
        }

        assert!(hidden_found);
        assert!(visible_found);
        assert!(system_found);
        assert!(ordinary_found);
        Ok(())
    }
}

#[cfg(test)]
mod test_read_confirmation {
    use super::*;

    #[test]
    fn an_empty_answer_takes_the_default() {
        assert!(read_confirmation(&b"\n"[..], true).expect("should read"));
        assert!(!read_confirmation(&b"\n"[..], false).expect("should read"));
        assert!(read_confirmation(&b"   \n"[..], true).expect("should read"));
    }

    #[test]
    fn yes_answers_are_accepted_in_any_case() {
        for answer in ["y\n", "Y\n", "yes\n", "YES\n", " Yes \n"] {
            assert!(
                read_confirmation(answer.as_bytes(), false).expect("should read"),
                "{answer:?} should mean yes"
            );
        }
    }

    #[test]
    fn anything_else_means_no_even_when_the_default_is_yes() {
        for answer in ["n\n", "no\n", "nope\n", "maybe\n", "1\n"] {
            assert!(
                !read_confirmation(answer.as_bytes(), true).expect("should read"),
                "{answer:?} should mean no"
            );
        }
    }

    #[test]
    fn a_closed_input_returns_false_regardless_of_default() {
        assert!(!read_confirmation(&b""[..], true).expect("should read"));
        assert!(!read_confirmation(&b""[..], false).expect("should read"));
    }
}

#[cfg(test)]
mod test_core_bugfixes {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    #[test]
    fn format_duration_seconds_rounds_boundary_and_rejects_non_finite() {
        assert_eq!(format_duration_seconds(59.96), "1m 00s");
        assert_eq!(format_duration_seconds(f64::INFINITY), "0.0s");
        assert_eq!(format_duration_seconds(f64::NAN), "0.0s");
    }

    #[test]
    fn available_disk_space_returns_positive_bytes_on_unix_and_windows() {
        let dir = tempdir().unwrap();
        let space = available_disk_space(dir.path());
        assert!(
            space.is_some_and(|bytes| bytes > 0),
            "expected disk space, got {space:?}"
        );

        let nested_missing = dir.path().join("missing_parent").join("file.mp4");
        let nested_space = available_disk_space(&nested_missing);
        assert!(
            nested_space.is_some_and(|bytes| bytes > 0),
            "expected disk space via existing ancestor, got {nested_space:?}"
        );
    }

    #[test]
    fn resolve_input_path_preserves_existing_path_with_surrounding_spaces() {
        let dir = tempdir().unwrap();
        let spaced = dir.path().join(" padded.txt ");
        File::create(&spaced).unwrap();

        let resolved = resolve_input_path(Some(&spaced)).expect("existing spaced file should resolve");
        assert_eq!(resolved, dunce::canonicalize(&spaced).unwrap());
    }

    #[test]
    fn workspace_test_context_excludes_cargo_run_binaries() {
        let workspace_manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let cargo_run_exe = Path::new("/tmp/target/debug/dirmove");
        let unit_test_exe = Path::new("/tmp/target/debug/deps/cli_tools_dirmove-0123456789abcdef");
        let outside_manifest = Path::new("/tmp/external-project");

        assert!(!is_workspace_test_context(
            Some(workspace_manifest),
            None,
            Some(cargo_run_exe),
        ));
        assert!(is_workspace_test_context(
            Some(workspace_manifest),
            None,
            Some(unit_test_exe),
        ));
        assert!(is_workspace_test_context(
            Some(workspace_manifest),
            Some(Path::new("/tmp/target/tmp")),
            Some(cargo_run_exe),
        ));
        assert!(!is_workspace_test_context(
            Some(outside_manifest),
            Some(Path::new("/tmp/target/tmp")),
            Some(unit_test_exe),
        ));
    }
}
