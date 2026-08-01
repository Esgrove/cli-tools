use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use walkdir::WalkDir;

use cli_tools::{print_error, print_magenta_bold, resolve_input_path, should_skip_entry, trash_or_delete};

/// Action to take when a conflicting unsuffixed file already exists.
#[derive(Debug, Clone, Copy)]
enum ConflictAction {
    /// Permanently delete the conflicting file.
    Delete,
    /// Move the conflicting file to the system trash.
    Trash,
}

/// Result of processing a single file.
#[derive(Debug)]
enum Outcome {
    /// File was renamed (no conflicting unsuffixed file existed).
    Renamed,
    /// Conflicting unsuffixed file was removed and suffixed file was renamed.
    RenamedAndRemoved,
    /// File was skipped (e.g. unsuffixed path exists but is not a regular file).
    Skipped,
}

#[derive(Parser)]
#[command(
    author,
    version,
    name = env!("CARGO_BIN_NAME"),
    about = "Recursively remove a trailing '_1' from filenames"
)]
struct Args {
    #[command(subcommand)]
    command: Option<RxRenameCommand>,

    /// Root directory to scan. Defaults to current directory.
    #[arg(value_hint = clap::ValueHint::DirPath)]
    root: Option<PathBuf>,

    /// Only print what would be done without making changes
    #[arg(short, long)]
    dryrun: bool,

    /// Move conflicting files to system trash instead of permanently deleting them
    #[arg(short, long)]
    trash: bool,

    /// Print verbose output
    #[arg(short, long, global = true)]
    verbose: bool,
}

/// Subcommands for `rx_rename`.
#[derive(Subcommand)]
enum RxRenameCommand {
    /// Generate shell completion script
    #[command(name = "completion")]
    Completion {
        /// Shell to generate completion for
        #[arg(value_enum)]
        shell: Shell,

        /// Install completion script to the shell's completion directory
        #[arg(short = 'I', long)]
        install: bool,
    },
}

fn main() -> Result<()> {
    let args = Args::parse();

    if let Some(RxRenameCommand::Completion { shell, install }) = &args.command {
        return cli_tools::generate_shell_completion(
            *shell,
            Args::command(),
            *install,
            args.verbose,
            env!("CARGO_BIN_NAME"),
        );
    }

    let root = resolve_input_path(args.root.as_deref())?;
    if !root.is_dir() {
        anyhow::bail!("Not a directory: {}", root.display());
    }

    let action = if args.trash {
        ConflictAction::Trash
    } else {
        ConflictAction::Delete
    };
    rename_files(&root, args.dryrun, action);
    Ok(())
}

/// Scan `root` recursively for files with a trailing `_1` suffix and rename them.
///
/// When a matching unsuffixed file already exists, it is deleted (or trashed) before renaming.
/// Errors for individual files are printed and skipped without aborting.
/// Prints a summary of renamed and removed file counts when finished.
fn rename_files(root: &Path, dryrun: bool, action: ConflictAction) {
    let files = find_files_with_rx_duplicate_suffix(root);
    let total = files.len();
    let width = total.to_string().len();

    let mut rename_count: usize = 0;
    let mut removed_count: usize = 0;

    for (i, file) in files.iter().enumerate() {
        match process_file(root, file, i + 1, total, width, dryrun, action) {
            Ok(Outcome::RenamedAndRemoved) => {
                rename_count += 1;
                removed_count += 1;
            }
            Ok(Outcome::Renamed) => rename_count += 1,
            Ok(Outcome::Skipped) => {}
            Err(e) => {
                print_error!("{}: {e}", file.display());
            }
        }
    }

    let rename_action = if dryrun { "would be renamed" } else { "renamed" };
    let removed_action = match (dryrun, action) {
        (true, ConflictAction::Delete) => "would be deleted",
        (true, ConflictAction::Trash) => "would be trashed",
        (false, ConflictAction::Delete) => "deleted",
        (false, ConflictAction::Trash) => "trashed",
    };

    println!("\n{rename_count} files {rename_action}");
    println!("{removed_count} files {removed_action}");
}

/// Find all files under `root` whose stem ends with `_1`, sorted by path.
fn find_files_with_rx_duplicate_suffix(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !should_skip_entry(e))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.ends_with("_1"))
        })
        .map(walkdir::DirEntry::into_path)
        .collect();
    files.sort();
    files
}

/// Process a single file ending in `_1`.
///
/// If a matching unsuffixed file already exists, trash it first, then rename.
fn process_file(
    root: &Path,
    file_with_suffix: &Path,
    index: usize,
    total: usize,
    width: usize,
    dryrun: bool,
    action: ConflictAction,
) -> Result<Outcome> {
    let stem = file_with_suffix
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("Non-UTF-8 filename: {}", file_with_suffix.display()))?;

    let extension = file_with_suffix.extension().and_then(|s| s.to_str());

    // Build the unsuffixed name: strip trailing "_1" and re-add extension
    let new_stem = stem
        .strip_suffix("_1")
        .ok_or_else(|| anyhow::anyhow!("Filename does not end with '_1': {}", file_with_suffix.display()))?;
    let original_name = extension.map_or_else(|| new_stem.to_string(), |ext| format!("{new_stem}.{ext}"));
    let file_without_suffix = file_with_suffix.with_file_name(&original_name);

    let relative = file_with_suffix.strip_prefix(root).unwrap_or(file_with_suffix);

    print_magenta_bold!("[{index:>width$} / {total}]:");
    println!("{}", relative.display());

    let needs_trash = if file_without_suffix.exists() {
        if !file_without_suffix.is_file() {
            return Ok(Outcome::Skipped);
        }
        let rel_without = file_without_suffix.strip_prefix(root).unwrap_or(&file_without_suffix);
        println!("{}", rel_without.display());
        true
    } else {
        false
    };

    if !dryrun {
        if needs_trash {
            match action {
                ConflictAction::Delete => std::fs::remove_file(&file_without_suffix)?,
                ConflictAction::Trash => trash_or_delete(&file_without_suffix)?,
            }
        }
        std::fs::rename(file_with_suffix, &file_without_suffix)?;
    }

    Ok(if needs_trash {
        Outcome::RenamedAndRemoved
    } else {
        Outcome::Renamed
    })
}

#[cfg(test)]
mod test_args {
    use super::*;

    #[test]
    fn parses_defaults_and_combined_flags() {
        let defaults = Args::try_parse_from(["rxrename"]).expect("default arguments should parse");
        assert!(defaults.command.is_none());
        assert!(defaults.root.is_none());
        assert!(!defaults.dryrun);
        assert!(!defaults.trash);
        assert!(!defaults.verbose);

        let combined = Args::try_parse_from(["rxrename", "folder", "--dryrun", "--trash", "--verbose"])
            .expect("combined arguments should parse");
        assert_eq!(combined.root, Some(PathBuf::from("folder")));
        assert!(combined.dryrun);
        assert!(combined.trash);
        assert!(combined.verbose);
    }

    #[test]
    fn parses_completion_and_has_valid_command_definition() {
        let args = Args::try_parse_from(["rxrename", "completion", "bash", "--install"])
            .expect("completion command should parse");
        assert!(matches!(
            args.command,
            Some(RxRenameCommand::Completion {
                shell: Shell::Bash,
                install: true
            })
        ));
        Args::command().debug_assert();
    }
}

#[cfg(test)]
mod test_find_files_with_rx_duplicate_suffix {
    use super::*;

    #[test]
    fn finds_only_exact_suffix_files_recursively_and_sorts_them() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let scan_root = temp_directory.path().join("videos");
        let nested = scan_root.join("nested");
        let hidden = scan_root.join(".hidden");
        std::fs::create_dir_all(&nested)?;
        std::fs::create_dir(&hidden)?;
        let alpha = scan_root.join("alpha_1.mp4");
        let beta = nested.join("beta_1");
        std::fs::write(&alpha, b"alpha")?;
        std::fs::write(&beta, b"beta")?;
        std::fs::write(scan_root.join("plain.mp4"), b"plain")?;
        std::fs::write(scan_root.join("gamma_10.mkv"), b"gamma")?;
        std::fs::write(hidden.join("ignored_1.mp4"), b"hidden")?;
        std::fs::create_dir(scan_root.join("directory_1"))?;

        let files = find_files_with_rx_duplicate_suffix(&scan_root);

        assert_eq!(files, vec![alpha, beta]);
        Ok(())
    }
}

#[cfg(test)]
mod test_process_file {
    use super::*;

    #[test]
    fn dryrun_without_conflict_reports_rename_without_changes() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let suffixed = temp_directory.path().join("clip_1.mp4");
        let unsuffixed = temp_directory.path().join("clip.mp4");
        std::fs::write(&suffixed, b"new")?;

        let outcome = process_file(temp_directory.path(), &suffixed, 1, 1, 1, true, ConflictAction::Delete)?;

        assert!(matches!(outcome, Outcome::Renamed));
        assert!(suffixed.exists());
        assert!(!unsuffixed.exists());
        Ok(())
    }

    #[test]
    fn dryrun_with_conflict_reports_removal_without_changes() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let suffixed = temp_directory.path().join("clip_1.mp4");
        let unsuffixed = temp_directory.path().join("clip.mp4");
        std::fs::write(&suffixed, b"new")?;
        std::fs::write(&unsuffixed, b"old")?;

        let outcome = process_file(temp_directory.path(), &suffixed, 1, 1, 1, true, ConflictAction::Trash)?;

        assert!(matches!(outcome, Outcome::RenamedAndRemoved));
        assert_eq!(std::fs::read(&suffixed)?, b"new");
        assert_eq!(std::fs::read(&unsuffixed)?, b"old");
        Ok(())
    }

    #[test]
    fn existing_directory_target_is_skipped() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let suffixed = temp_directory.path().join("clip_1.mp4");
        std::fs::write(&suffixed, b"new")?;
        std::fs::create_dir(temp_directory.path().join("clip.mp4"))?;

        let outcome = process_file(temp_directory.path(), &suffixed, 1, 1, 1, false, ConflictAction::Delete)?;

        assert!(matches!(outcome, Outcome::Skipped));
        assert!(suffixed.exists());
        Ok(())
    }

    #[test]
    fn rejects_filename_without_duplicate_suffix() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let path = temp_directory.path().join("clip_2.mp4");
        std::fs::write(&path, b"contents")?;

        let error = process_file(temp_directory.path(), &path, 1, 1, 1, true, ConflictAction::Delete)
            .expect_err("nonmatching suffix should fail");

        assert!(error.to_string().contains("does not end with '_1'"));
        Ok(())
    }

    #[test]
    fn delete_mode_replaces_conflicting_file() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let suffixed = temp_directory.path().join("clip_1.mp4");
        let unsuffixed = temp_directory.path().join("clip.mp4");
        std::fs::write(&suffixed, b"new")?;
        std::fs::write(&unsuffixed, b"old")?;

        let outcome = process_file(temp_directory.path(), &suffixed, 1, 1, 1, false, ConflictAction::Delete)?;

        assert!(matches!(outcome, Outcome::RenamedAndRemoved));
        assert!(!suffixed.exists());
        assert_eq!(std::fs::read(&unsuffixed)?, b"new");
        Ok(())
    }
}
