pub mod date;
pub mod dot_rename;

use std::cmp::Ordering;
use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result};
use clap::Command;
use clap_complete::Shell;
use colored::{ColoredString, Colorize};
use difference::{Changeset, Difference};
use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

#[cfg(not(test))]
const PROJECT_NAME: &str = env!("CARGO_PKG_NAME");

/// Path to the user config file: `$HOME/.config/cli-tools.toml`
///
/// Returns `None` if the home directory cannot be determined.
#[cfg(not(test))]
static CONFIG_PATH: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    let home_dir = dirs::home_dir()?;
    Some(home_dir.join(".config").join(format!("{PROJECT_NAME}.toml")))
});

/// Path to the sample config fixture file used during tests.
#[cfg(test)]
static CONFIG_PATH: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    Some(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("sample_config.toml"),
    )
});

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
];

/// Return the path to the user config file.
///
/// During library tests, returns the sample fixture at `tests/fixtures/sample_config.toml`.
/// Otherwise, returns `$HOME/.config/cli-tools.toml`.
///
/// Returns `None` if the home directory cannot be determined.
pub fn config_path() -> Option<&'static Path> {
    CONFIG_PATH.as_deref()
}

/// Append an extension to `PathBuf`, which is missing from the standard lib :(
#[must_use]
pub fn append_extension_to_path(path: &Path, extension: impl AsRef<OsStr>) -> PathBuf {
    let mut os_string: OsString = path.into();
    os_string.push(".");
    os_string.push(extension);
    os_string.into()
}

/// Helper method to assert floating point equality in test cases.
///
/// # Panics
/// Panics if the absolute difference between `a` and `b` exceeds `f64::EPSILON`.
#[inline]
pub fn assert_f64_eq(a: f64, b: f64) {
    let epsilon = f64::EPSILON;
    assert!(
        (a - b).abs() <= epsilon,
        "Values are not equal: {a} and {b} (epsilon = {epsilon})"
    );
}

/// Format bool value as a coloured string.
#[must_use]
pub fn colorize_bool(value: bool) -> ColoredString {
    if value { "true".green() } else { "false".red() }
}

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

/// Format bytes as human-readable size.
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
/// Negative values are treated as zero.
#[must_use]
pub fn format_duration_seconds(seconds: f64) -> String {
    // Ensure non-negative value before converting to avoid sign loss
    let seconds = seconds.max(0.0);
    // Values larger than u64::MAX will saturate, which is acceptable for duration display
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let secs = seconds as u64;
    if secs >= 3600 {
        format!("{}h {:02}m {:02}s", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{seconds:.1}s")
    }
}

/// Determine the appropriate directory for storing shell completions.
///
/// First checks if the user-specific directory exists,
/// then checks for the global directory.
/// If neither exist, creates and uses the user-specific dir.
fn get_shell_completion_dir(shell: Shell, name: &str) -> Result<PathBuf> {
    let home = dirs::home_dir().expect("Failed to get home directory");

    // Special handling for oh-my-zsh.
    // Create custom "plugin", which will then have to be loaded in .zshrc
    if shell == Shell::Zsh {
        let omz_plugins = home.join(".oh-my-zsh/custom/plugins");
        if omz_plugins.exists() {
            let plugin_dir = omz_plugins.join(name);
            std::fs::create_dir_all(&plugin_dir)?;
            return Ok(plugin_dir);
        }
    }

    let user_dir = match shell {
        Shell::PowerShell => {
            if cfg!(windows) {
                home.join(r"Documents\PowerShell\completions")
            } else {
                home.join(".config/powershell/completions")
            }
        }
        Shell::Bash => home.join(".bash_completion.d"),
        Shell::Elvish => home.join(".elvish"),
        Shell::Fish => home.join(".config/fish/completions"),
        Shell::Zsh => home.join(".zsh/completions"),
        _ => anyhow::bail!("Unsupported shell"),
    };

    if user_dir.exists() {
        return Ok(user_dir);
    }

    let global_dir = match shell {
        Shell::PowerShell => {
            if cfg!(windows) {
                home.join(r"Documents\PowerShell\completions")
            } else {
                home.join(".config/powershell/completions")
            }
        }
        Shell::Bash => PathBuf::from("/etc/bash_completion.d"),
        Shell::Fish => PathBuf::from("/usr/share/fish/completions"),
        Shell::Zsh => PathBuf::from("/usr/share/zsh/site-functions"),
        _ => anyhow::bail!("Unsupported shell"),
    };

    if global_dir.exists() {
        return Ok(global_dir);
    }

    std::fs::create_dir_all(&user_dir)?;
    Ok(user_dir)
}

/// Generate a shell completion script for the given shell.
///
/// # Errors
/// Returns an error if:
/// - The shell completion directory cannot be determined or created
/// - The completion file cannot be generated or written
pub fn generate_shell_completion(shell: Shell, mut command: Command, install: bool, command_name: &str) -> Result<()> {
    if install {
        let out_dir = get_shell_completion_dir(shell, command_name)?;
        let path = clap_complete::generate_to(shell, &mut command, command_name, out_dir)?;
        println!("Completion file generated to: {}", path.display());
    } else {
        clap_complete::generate(shell, &mut command, command_name, &mut std::io::stdout());
    }
    Ok(())
}

/// Prompt the user for confirmation with a yes/no question.
///
/// Returns `true` if the user answers yes (y/Y), `false` otherwise.
/// The default value is used when the user presses Enter without input.
///
/// # Errors
/// Returns an error if reading from stdin fails.
pub fn get_user_confirmation(message: &str, default: bool) -> io::Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    print!("{message} {hint} ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    let trimmed = input.trim().to_lowercase();
    if trimmed.is_empty() {
        Ok(default)
    } else {
        Ok(trimmed == "y" || trimmed == "yes")
    }
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
    // Use NFC (Normalization Form Composed) from unicode_normalization crate
    // to retain the correct format and not cause issues later on.
    // https://github.com/unicode-rs/unicode-normalization

    Ok((
        file_stem.nfc().collect::<String>(),
        file_extension.nfc().collect::<String>(),
    ))
}

/// Get the normalized directory name from a Path with special characters retained.
///
/// # Errors
/// Returns an error if the directory name cannot be extracted from the path.
pub fn get_normalized_dir_name(path: &Path) -> Result<String> {
    let dir_name = os_str_to_string(path.file_name().context("Failed to get directory name")?);

    Ok(dir_name.nfc().collect::<String>())
}

/// Check if entry is a hidden file or directory (starts with '.')
#[must_use]
pub fn is_hidden(entry: &walkdir::DirEntry) -> bool {
    let name_bytes = entry.file_name().as_encoded_bytes();
    !name_bytes.is_empty() && name_bytes[0] == b'.'
}

/// Check if entry is a hidden file or directory (starts with '.')
#[must_use]
pub fn is_hidden_tokio(entry: &tokio::fs::DirEntry) -> bool {
    let name = entry.file_name();
    let name_bytes = name.as_encoded_bytes();
    !name_bytes.is_empty() && name_bytes[0] == b'.'
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
#[must_use]
pub fn is_system_directory_tokio(entry: &tokio::fs::DirEntry) -> bool {
    let file_name = entry.file_name();
    let name = file_name.to_string_lossy();
    SYSTEM_DIRECTORIES.iter().any(|dir| name.eq_ignore_ascii_case(dir))
}

/// Check if a path is a system directory that should be skipped.
/// Returns true for OS-specific directories like `$RECYCLE.BIN`, `.Spotlight-V100`, or `lost+found`.
#[must_use]
pub fn is_system_directory_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SYSTEM_DIRECTORIES.iter().any(|dir| name.eq_ignore_ascii_case(dir)))
}

/// Check if a path is on a network drive.
/// On Windows, detects mapped network drives and UNC paths.
/// On other platforms, always returns false.
#[cfg(windows)]
#[must_use]
pub fn is_network_path(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;

    // Check for UNC paths (\\server\share)
    let path_str = path.to_string_lossy();
    if path_str.starts_with(r"\\") {
        return true;
    }

    // Check drive type for mapped network drives
    if let Some(prefix) = path.components().next() {
        let prefix_str = prefix.as_os_str();
        // Create a root path like "X:\"
        let mut root: Vec<u16> = prefix_str.encode_wide().collect();
        if root.len() >= 2 && root[1] == u16::from(b':') {
            root.push(u16::from(b'\\'));
            root.push(0); // null terminator

            // SAFETY: GetDriveTypeW is a safe Windows API call that only reads
            // the null-terminated string to determine drive type
            #[allow(unsafe_code)]
            let drive_type = unsafe { GetDriveTypeW(root.as_ptr()) };
            return drive_type == DRIVE_REMOTE;
        }
    }

    false
}

/// Check if a path is on a network drive.
/// On Windows, detects mapped network drives and UNC paths.
/// On other platforms, always returns false.
#[cfg(not(windows))]
#[must_use]
pub const fn is_network_path(_path: &Path) -> bool {
    false
}

/// Check if entry should be skipped (hidden or system directory).
/// Combines `is_hidden` and `is_system_directory` checks.
#[must_use]
pub fn should_skip_entry(entry: &walkdir::DirEntry) -> bool {
    is_hidden(entry) || is_system_directory(entry)
}

/// Check if directory is empty (contains no files or subdirectories)
pub fn is_directory_empty(dir: &Path) -> bool {
    for entry in WalkDir::new(dir).into_iter().filter_map(std::result::Result::ok) {
        if entry.path() != dir {
            return false;
        }
    }
    true
}

/// Insert a suffix before the file extension.
///
/// Takes a path and inserts the given suffix string between the file stem and the file extension.
/// If the file has no extension, the suffix is appended to the end.
///
/// ```rust
/// use std::path::Path;
/// use cli_tools::insert_suffix_before_extension;
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
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("");

    let new_name = if extension.is_empty() {
        format!("{stem}{suffix}")
    } else {
        format!("{stem}{suffix}.{extension}")
    };

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
/// use cli_tools::resolve_input_path;
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
    let input_path = path.map(|p| p.to_str().unwrap_or(""));

    resolve_input_path_str(input_path)
}

/// Resolves the provided input path to a directory or file to an absolute path.
///
/// If `path` is `None` or an empty string, the current working directory is used.
/// The function verifies that the provided path exists and is accessible,
/// returning an error if it does not.
///
/// ```rust
/// use std::path::PathBuf;
/// use cli_tools::resolve_input_path_str;
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
    let input_path = path.unwrap_or_default().trim();
    let filepath = if input_path.is_empty() {
        env::current_dir().context("Failed to get current working directory")?
    } else {
        PathBuf::from(input_path)
    };
    if !filepath.exists() {
        anyhow::bail!(
            "Input path does not exist or is not accessible: '{}'",
            filepath.display()
        );
    }

    let absolute_input_path = dunce::canonicalize(&filepath)?;

    // Canonicalize fails for network drives on Windows :(
    if path_to_string(&absolute_input_path).starts_with(r"\\?") && !path_to_string(&filepath).starts_with(r"\\?") {
        Ok(filepath)
    } else {
        Ok(absolute_input_path)
    }
}

/// Resolves a required input path to an absolute path.
///
/// Unlike `resolve_input_path`,
/// this function does not fall back to the current working directory when the path is empty.
/// It requires a valid, non-empty path.
/// ```rust
/// use std::path::Path;
/// use cli_tools::resolve_required_input_path;
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
    let input_path = path.to_str().unwrap_or("");

    resolve_required_input_path_str(input_path)
}

/// Resolves a required input path to an absolute path.
///
/// Unlike `resolve_input_path`,
/// this function does not fall back to the current working directory when the path is empty.
/// It requires a valid, non-empty path.
/// ```rust
/// use cli_tools::resolve_required_input_path_str;
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
    let input_path = path.trim();

    if input_path.is_empty() {
        anyhow::bail!("Input path cannot be empty");
    }

    let filepath = PathBuf::from(input_path);
    if !filepath.exists() {
        anyhow::bail!(
            "Input path does not exist or is not accessible: '{}'",
            filepath.display()
        );
    }

    let absolute_input_path = dunce::canonicalize(&filepath)?;

    // Canonicalize fails for network drives on Windows :(
    if path_to_string(&absolute_input_path).starts_with(r"\\?") && !path_to_string(&filepath).starts_with(r"\\?") {
        Ok(filepath)
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
    Ok(output_path)
}

/// Gets the relative path or filename from a full path based on a root directory.
///
/// If the full path is within the root directory, the function returns the relative path.
/// Otherwise, it returns just the filename. If the filename cannot be determined, the
/// full path is returned.
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
/// use cli_tools::get_unique_path;
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
    let mut path = dir.join(filename);

    if !path.exists() {
        return path;
    }

    let mut counter = 1;
    while path.exists() {
        let new_name = if extension.is_empty() {
            format!("{stem}.{counter}")
        } else {
            format!("{stem}.{counter}.{extension}")
        };
        path = dir.join(new_name);
        counter += 1;
    }

    path
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

/// Get relative path and convert to string with invalid unicode handling.
#[must_use]
pub fn path_to_string_relative(path: &Path) -> String {
    path_to_string(&get_relative_path_from_current_working_directory(path))
}

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

/// Print a stacked diff of the changes.
pub fn show_diff(old: &str, new: &str) {
    let (old_diff, new_diff) = color_diff(old, new, true);
    println!("{old_diff}");
    if old_diff != new_diff {
        println!("{new_diff}");
    }
}

/// Delete a file, moving to trash when possible.
///
/// For Windows network paths, uses direct deletion since trash doesn't work there.
/// For local paths, moves the file to the system trash.
///
/// # Errors
/// Returns an error if the file cannot be deleted or trashed.
pub fn trash_or_delete(path: &Path) -> std::io::Result<()> {
    if is_network_path(path) {
        std::fs::remove_file(path)
    } else {
        trash::delete(path).map_err(std::io::Error::other)
    }
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
        assert_eq!(format_duration(Duration::from_secs(60)), "1m 00s");
        assert_eq!(format_duration(Duration::from_secs(90)), "1m 30s");
        assert_eq!(format_duration(Duration::from_secs(3599)), "59m 59s");
    }

    #[test]
    fn hours_minutes_seconds() {
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h 00m 00s");
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
    use std::ffi::OsStr;
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
    fn os_str_to_string_basic() {
        let os_str = OsStr::new("test.txt");
        assert_eq!(os_str_to_string(os_str), "test.txt");
    }

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
}

#[cfg(test)]
mod color_diff_tests {
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
mod show_diff_tests {
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
    #[cfg(not(windows))]
    fn non_windows_always_false() {
        let path = Path::new("/home/user/file.txt");
        assert!(!is_network_path(path));
    }
}

#[cfg(test)]
mod relative_path_tests {
    use super::*;

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
    fn get_unique_path_handles_no_extension() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("file"), "content").unwrap();

        let result = get_unique_path(dir.path(), "file", "file", "");
        assert_eq!(result, dir.path().join("file.1"));
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
}
