pub mod config;
pub mod date;

use std::cmp::Ordering;
use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Command;
use clap_complete::Shell;
use colored::{ColoredString, Colorize};
use difference::{Changeset, Difference};
use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

/// Append an extension to `PathBuf`, which is missing from the standard lib :(
pub fn append_extension_to_path(path: PathBuf, extension: impl AsRef<OsStr>) -> PathBuf {
    let mut os_string: OsString = path.into();
    os_string.push(".");
    os_string.push(extension);
    os_string.into()
}

/// Format bool value as a coloured string.
#[must_use]
pub fn colorize_bool(value: bool) -> ColoredString {
    if value { "true".green() } else { "false".red() }
}

/// Get filename from Path with special characters retained instead of decomposed.
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

/// Check if directory is empty (contains no files or subdirectories)
pub fn is_directory_empty(dir: &Path) -> bool {
    for entry in WalkDir::new(dir).into_iter().filter_map(std::result::Result::ok) {
        if entry.path() != dir {
            return false;
        }
    }
    true
}

/// Resolves the provided input path to a directory or file to an absolute path.
///
/// If `path` is `None`, the current working directory is used.
/// The function verifies that the provided path exists and is accessible,
/// returning an error if it does not.
/// ```rust
/// use std::path::{Path, PathBuf};
/// use cli_tools::resolve_input_path;
///
/// let path = Path::new("src");
/// let absolute_path = resolve_input_path(Some(path)).unwrap();
/// ```
#[inline]
pub fn resolve_input_path(path: Option<&Path>) -> Result<PathBuf> {
    let input_path = path
        .map(|p| p.to_str().unwrap_or(""))
        .unwrap_or_default()
        .trim()
        .to_string();

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
#[inline]
pub fn resolve_input_path_str(path: Option<&str>) -> Result<PathBuf> {
    let input_path = path.unwrap_or_default().trim().to_string();
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

/// Resolves the provided output path relative to an absolute input path.
///
/// If `path` is provided, it is used directly.
/// If `path` is `None` or an empty string, and the absolute input path is a file,
/// the parent directory of the input path is used.
/// Otherwise, the input directory is used as the output path.
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
        return full_path.file_name().unwrap_or_default().to_string_lossy().to_string();
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
pub fn print_warning(message: &str) {
    eprintln!("{}", message.yellow());
}

#[macro_export]
macro_rules! print_warning {
    ($($arg:tt)*) => {
        $crate::print_warning(&format!($($arg)*))
    };
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

/// Print a stacked diff of the changes.
pub fn show_diff(old: &str, new: &str) {
    let (old_diff, new_diff) = color_diff(old, new, true);
    println!("{old_diff}");
    if old_diff != new_diff {
        println!("{new_diff}");
    }
}

/// Generate a shell completion script for the given shell.
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

/// Helper method to assert floating point equality in test cases.
#[inline]
pub fn assert_f64_eq(a: f64, b: f64) {
    let epsilon = f64::EPSILON;
    assert!(
        (a - b).abs() <= epsilon,
        "Values are not equal: {a} and {b} (epsilon = {epsilon})"
    );
}

#[cfg(test)]
mod lib_tests {
    use super::*;

    use std::fs::File;

    use tempfile::tempdir;
    use walkdir::WalkDir;

    #[test]
    fn test_is_hidden_file() {
        let dir = tempdir().unwrap();
        let hidden_file_path = dir.path().join(".hidden");
        File::create(hidden_file_path).unwrap();

        let entry = WalkDir::new(dir.path())
            .into_iter()
            .filter_map(Result::ok)
            .find(|e| e.file_name().to_string_lossy().eq(".hidden"))
            .unwrap();

        assert!(is_hidden(&entry));

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
    fn test_resolve_input_path_valid() {
        let dir = tempdir().unwrap();
        let path = dir.path();
        let resolved = resolve_input_path(Some(path));
        assert!(resolved.is_ok());
    }

    #[test]
    fn test_resolve_input_path_nonexistent() {
        let path = Path::new("nonexistent");
        let resolved = resolve_input_path(Some(path));
        assert!(resolved.is_err());
    }

    #[test]
    fn test_resolve_input_path_empty() {
        let path = Path::new("  \n");
        let resolved = resolve_input_path(Some(path));
        assert!(resolved.is_ok());
        assert_eq!(resolved.unwrap(), env::current_dir().unwrap());
    }

    #[test]
    fn test_resolve_input_path_default() {
        let resolved = resolve_input_path(None);
        assert!(resolved.is_ok());
        assert_eq!(resolved.unwrap(), env::current_dir().unwrap());
    }

    #[test]
    fn test_resolve_output_path_with_file() {
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
    fn test_resolve_output_path_default() {
        let dir = tempdir().unwrap();
        let output_path = resolve_output_path(None, dir.path());
        assert!(output_path.is_ok());
        assert_eq!(output_path.unwrap(), dunce::simplified(dir.path()));
    }
}
