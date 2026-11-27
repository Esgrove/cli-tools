//! The `rxrename` binary.
//!
//! Removes a trailing "_1" suffix from file names,
//! moving any conflicting unsuffixed file to the system trash by default
//! (or permanently deleting it with `--delete`).
//! Operates on the given directory only unless `--recurse` is specified.

#![cfg_attr(test, allow(clippy::panic_in_result_fn))]

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use cli_tools_common::{
    CompletionArgs, OverwritePolicy, TargetReservations, count_label, is_network_path, os_str_to_string, print_error,
    print_magenta_bold, print_yellow, resolve_input_path, safe_rename, should_skip_entry,
};
use walkdir::WalkDir;

/// Action to take when a conflicting unsuffixed file already exists.
#[derive(Debug, Clone, Copy)]
enum ConflictAction {
    /// Permanently delete the conflicting file.
    Delete,
    /// Move the conflicting file to the system trash.
    Trash {
        /// Detect paths where system trash is unavailable on Windows.
        is_network: fn(&Path) -> bool,
        /// Send the staged destination to system trash.
        dispose: fn(&Path) -> std::io::Result<()>,
    },
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

/// Subcommands for `rxrename`.
#[derive(Subcommand)]
enum RxRenameCommand {
    /// Generate shell completion script
    #[command(name = "completion")]
    Completion(CompletionArgs),
}

#[derive(Parser)]
#[command(
    author,
    version,
    name = env!("CARGO_BIN_NAME"),
    about = "Remove a trailing '_1' from filenames"
)]
struct Args {
    #[command(subcommand)]
    command: Option<RxRenameCommand>,

    /// Root directory to scan. Defaults to current directory.
    #[arg(value_hint = clap::ValueHint::DirPath)]
    root: Option<PathBuf>,

    /// Permanently delete conflicting files instead of moving them to system trash
    #[arg(short = 'D', long, conflicts_with = "trash")]
    delete: bool,

    /// Only print what would be done without making changes
    #[arg(short, long)]
    dryrun: bool,

    /// Recurse into subdirectories
    #[arg(short = 'r', long)]
    recurse: bool,

    /// Move conflicting files to system trash (default behavior)
    #[arg(short, long)]
    trash: bool,

    /// Print verbose output
    #[arg(short, long, global = true)]
    verbose: bool,
}

/// Counts of the files handled in one run.
#[derive(Debug, Default, PartialEq, Eq)]
struct RenameSummary {
    /// Files renamed, or that would be renamed in a dry run.
    renamed: usize,
    /// Conflicting unsuffixed files removed.
    removed: usize,
    /// Files that could not be processed.
    failed: usize,
}

/// Paths tracked across a batch of renames.
#[derive(Default)]
struct RenameBatch {
    /// Discovered sources, protected while still present at their original paths.
    sources: TargetReservations,
    /// Sources that a dry run would have vacated.
    previewed_sources: HashSet<PathBuf>,
    /// Claimed destinations, which must not be overwritten by a later rename.
    destinations: TargetReservations,
}

impl ConflictAction {
    /// Select system trash using the platform's network path detection.
    const fn trash() -> Self {
        Self::Trash {
            is_network: is_network_path,
            dispose: trash_file,
        }
    }
}

impl RenameBatch {
    /// Track the sources discovered at the start of the run.
    fn new(files: &[PathBuf]) -> Self {
        let mut batch = Self::default();
        for file in files {
            batch.sources.reserve(file);
        }
        batch
    }

    /// Test whether a previewed source would have vacated this exact directory entry.
    fn is_vacated_in_preview(&self, target: &Path) -> bool {
        self.previewed_sources.contains(target)
            || self.previewed_sources.iter().any(|source| {
                source.parent() == target.parent() && cli_tools_common::paths_refer_to_same_entry(source, target)
            })
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

    if let Some(RxRenameCommand::Completion(completion)) = &args.command {
        return completion.generate::<Args>(args.verbose, env!("CARGO_BIN_NAME"));
    }

    let root = resolve_input_path(args.root.as_deref())?;
    if !root.is_dir() {
        anyhow::bail!("Not a directory: {}", root.display());
    }

    let action = if args.delete {
        ConflictAction::Delete
    } else {
        ConflictAction::trash()
    };
    let summary = rename_files(&root, args.recurse, args.dryrun, action);
    if summary.failed > 0 {
        anyhow::bail!("Failed to process {}", count_label(summary.failed, "file", "files"));
    }
    Ok(())
}

/// Scan `root` for files with a trailing `_1` suffix and rename them.
///
/// An existing unsuffixed file is moved to the system trash by default, or deleted directly with `--delete`.
/// Errors for individual files are printed and counted without aborting.
/// Prints a summary of renamed and removed file counts when finished.
fn rename_files(root: &Path, recurse: bool, dryrun: bool, action: ConflictAction) -> RenameSummary {
    let files = find_files_with_rx_duplicate_suffix(root, recurse);
    let total = files.len();
    let width = total.to_string().len();

    let mut summary = RenameSummary::default();
    let mut batch = RenameBatch::new(&files);

    for (i, file) in files.iter().enumerate() {
        match process_file_in_batch(root, file, i + 1, total, width, dryrun, action, &mut batch) {
            Ok(Outcome::RenamedAndRemoved) => {
                summary.renamed += 1;
                summary.removed += 1;
            }
            Ok(Outcome::Renamed) => summary.renamed += 1,
            Ok(Outcome::Skipped) => {}
            Err(e) => {
                summary.failed += 1;
                print_error!("{}: {e:#}", file.display());
            }
        }
    }

    let rename_action = if dryrun { "would be renamed" } else { "renamed" };
    let removed_action = match (dryrun, action) {
        (true, ConflictAction::Delete) => "would be deleted",
        (true, ConflictAction::Trash { .. }) => "would be trashed",
        (false, ConflictAction::Delete) => "deleted",
        (false, ConflictAction::Trash { .. }) => "trashed",
    };

    println!("\n{} {rename_action}", count_label(summary.renamed, "file", "files"));
    println!("{} {removed_action}", count_label(summary.removed, "file", "files"));
    summary
}

/// Check whether a raw file stem ends with the literal `_1` suffix without stripping invalid UTF-8 bytes.
fn stem_ends_with_rx_suffix(stem: &std::ffi::OsStr) -> bool {
    // The encoded bytes hold ASCII only for ASCII characters on every platform.
    stem.as_encoded_bytes().ends_with(b"_1")
}

/// Find files under `root` whose stem ends with `_1`, sorted by path.
///
/// Only direct children of `root` are scanned unless `recurse` is `true`.
fn find_files_with_rx_duplicate_suffix(root: &Path, recurse: bool) -> Vec<PathBuf> {
    let walker = if recurse {
        WalkDir::new(root).follow_links(false)
    } else {
        WalkDir::new(root).follow_links(false).max_depth(1)
    };
    let mut files: Vec<PathBuf> = walker
        .into_iter()
        .filter_entry(|e| !should_skip_entry(e))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().file_stem().is_some_and(stem_ends_with_rx_suffix))
        .map(walkdir::DirEntry::into_path)
        .collect();
    files.sort();
    files
}

/// Process a single file ending in `_1` in isolation.
#[cfg(test)]
fn process_file(
    root: &Path,
    file_with_suffix: &Path,
    index: usize,
    total: usize,
    width: usize,
    dryrun: bool,
    action: ConflictAction,
) -> Result<Outcome> {
    process_file_in_batch(
        root,
        file_with_suffix,
        index,
        total,
        width,
        dryrun,
        action,
        &mut RenameBatch::default(),
    )
}

/// Process a single file ending in `_1` without replacing other sources or earlier destinations in the batch.
///
/// Keep an existing unsuffixed file recoverable until the replacement succeeds.
#[allow(clippy::too_many_arguments)]
fn process_file_in_batch(
    root: &Path,
    file_with_suffix: &Path,
    index: usize,
    total: usize,
    width: usize,
    dryrun: bool,
    action: ConflictAction,
    batch: &mut RenameBatch,
) -> Result<Outcome> {
    let raw_stem = file_with_suffix
        .file_stem()
        .ok_or_else(|| anyhow::anyhow!("Missing file name: {}", file_with_suffix.display()))?;
    if !stem_ends_with_rx_suffix(raw_stem) {
        anyhow::bail!("Filename does not end with '_1': {}", file_with_suffix.display());
    }

    // Parts of the name that are not valid UTF-8 are removed, so the new name is a clean string.
    let stem = os_str_to_string(raw_stem);

    // Build the unsuffixed name: strip trailing "_1" and re-add the extension
    let new_stem = stem
        .strip_suffix("_1")
        .ok_or_else(|| anyhow::anyhow!("Filename does not end with '_1': {}", file_with_suffix.display()))?;
    // A name that is only the suffix has no original name to restore,
    // and dropping the suffix would leave a hidden file such as ".txt".
    if new_stem.is_empty() {
        return Ok(Outcome::Skipped);
    }
    let original_name = match file_with_suffix.extension() {
        Some(raw_extension) => {
            if raw_extension.is_empty() {
                format!("{new_stem}.")
            } else {
                let extension = os_str_to_string(raw_extension);
                // Dropping an extension with no valid characters would leave the bare stem,
                // which could be an unrelated file that the conflict handling then removes.
                if extension.is_empty() {
                    anyhow::bail!(
                        "Extension has no valid UTF-8 characters: {}",
                        file_with_suffix.display()
                    );
                }
                format!("{new_stem}.{extension}")
            }
        }
        None => new_stem.to_string(),
    };
    let file_without_suffix = file_with_suffix.with_file_name(&original_name);

    let relative = file_with_suffix.strip_prefix(root).unwrap_or(file_with_suffix);

    print_magenta_bold!("[{index:>width$} / {total}]:");
    println!("{}", relative.display());

    if !batch.destinations.reserve(&file_without_suffix) {
        print_yellow!(
            "Skipping: {} was already claimed by another rename in this run",
            file_without_suffix.display()
        );
        return Ok(Outcome::Skipped);
    }

    let has_conflict = if let Ok(metadata) = std::fs::symlink_metadata(&file_without_suffix)
        && (!dryrun || !batch.is_vacated_in_preview(&file_without_suffix))
    {
        if metadata.is_dir() || file_without_suffix.is_dir() {
            return Ok(Outcome::Skipped);
        }
        if batch.sources.contains(&file_without_suffix) {
            print_yellow!(
                "Skipping: {} is still a source for another rename in this run",
                file_without_suffix.display()
            );
            return Ok(Outcome::Skipped);
        }
        // A sanitized name is not the original spelling, so the existing entry may be unrelated,
        // or another sanitized file renamed earlier in this run, and must not be removed.
        if file_with_suffix.file_name().and_then(std::ffi::OsStr::to_str).is_none() {
            print_yellow!(
                "Skipping: {} already exists and the name had invalid UTF-8 removed",
                file_without_suffix.display()
            );
            return Ok(Outcome::Skipped);
        }
        let rel_without = file_without_suffix.strip_prefix(root).unwrap_or(&file_without_suffix);
        println!("{}", rel_without.display());
        true
    } else {
        false
    };

    if has_conflict
        && let ConflictAction::Trash { is_network, .. } = action
        && is_network(&file_without_suffix)
    {
        anyhow::bail!(
            "System trash is unavailable for this Windows network path: {}. \
             Pass --delete only if you want to permanently replace the existing file",
            file_without_suffix.display()
        );
    }

    if dryrun {
        batch.previewed_sources.insert(file_with_suffix.to_path_buf());
    } else if has_conflict {
        match action {
            ConflictAction::Delete => {
                safe_rename(file_with_suffix, &file_without_suffix, OverwritePolicy::Overwrite)?;
            }
            ConflictAction::Trash { dispose, .. } => {
                replace_with_trash(file_with_suffix, &file_without_suffix, dispose)?;
            }
        }
    } else {
        std::fs::rename(file_with_suffix, &file_without_suffix)?;
    }

    Ok(if has_conflict {
        Outcome::RenamedAndRemoved
    } else {
        Outcome::Renamed
    })
}

/// Move a file to system trash without a permanent deletion fallback.
fn trash_file(path: &Path) -> std::io::Result<()> {
    cli_tools_common::move_to_trash(path)
}

/// Stage the old destination on the same volume, then trash it only after the replacement is installed.
fn replace_with_trash(
    source: &Path,
    target: &Path,
    trash_file: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<()> {
    let parent = target
        .parent()
        .context("Replacement destination has no parent directory")?;
    let file_name = target.file_name().context("Replacement destination has no file name")?;
    let backup_directory = tempfile::Builder::new()
        .prefix(".rxrename-backup-")
        .tempdir_in(parent)?
        .keep();
    let backup = backup_directory.join(file_name);
    let result = replace_and_trash_backup(source, target, &backup, trash_file);
    // Never recursively clean up the backup directory, because it may still hold the old destination.
    let _ = std::fs::remove_dir(backup_directory);
    result
}

/// Install the replacement, restoring the backup if the rename fails and keeping it if trash fails.
fn replace_and_trash_backup(
    source: &Path,
    target: &Path,
    backup: &Path,
    trash_file: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<()> {
    std::fs::rename(target, backup).context("Could not stage old destination")?;

    if let Err(error) = std::fs::rename(source, target) {
        if let Err(restore_error) = safe_rename(backup, target, OverwritePolicy::Error) {
            anyhow::bail!(
                "Could not install replacement: {error}. Could not restore old destination: {restore_error}. \
                 Check the backup at {}",
                backup.display()
            );
        }
        return Err(error).context("Could not install replacement");
    }

    trash_file(backup).with_context(|| {
        format!(
            "Installed replacement, but could not trash old destination. Check the backup at {}",
            backup.display()
        )
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
        assert!(!defaults.delete);
        assert!(!defaults.dryrun);
        assert!(!defaults.recurse);
        assert!(!defaults.trash);
        assert!(!defaults.verbose);

        let combined = Args::try_parse_from(["rxrename", "folder", "--delete", "--dryrun", "--recurse", "--verbose"])
            .expect("combined arguments should parse");
        assert_eq!(combined.root, Some(PathBuf::from("folder")));
        assert!(combined.delete);
        assert!(combined.dryrun);
        assert!(combined.recurse);
        assert!(combined.verbose);
    }

    #[test]
    fn parses_completion_and_has_valid_command_definition() {
        use clap::CommandFactory;
        use clap_complete::Shell;

        let args = Args::try_parse_from(["rxrename", "completion", "bash", "--install"])
            .expect("completion command should parse");
        assert!(matches!(
            args.command,
            Some(RxRenameCommand::Completion(CompletionArgs {
                shell: Shell::Bash,
                install: true
            }))
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

        let non_recursive = find_files_with_rx_duplicate_suffix(&scan_root, false);
        assert_eq!(non_recursive, vec![alpha.clone()]);

        let recursive = find_files_with_rx_duplicate_suffix(&scan_root, true);
        assert_eq!(recursive, vec![alpha, beta]);
        Ok(())
    }
}

#[cfg(test)]
mod test_rename_files {
    use super::*;

    #[test]
    fn summary_counts_renamed_and_removed_files() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let root = &temp_directory.path().join("videos");
        std::fs::create_dir(root)?;
        std::fs::write(root.join("alpha_1.mp4"), b"new alpha")?;
        std::fs::write(root.join("beta_1.mp4"), b"new beta")?;
        std::fs::write(root.join("beta.mp4"), b"old beta")?;
        std::fs::write(root.join("gamma_1.mp4"), b"new gamma")?;
        std::fs::create_dir(root.join("gamma.mp4"))?;

        let summary = rename_files(root, true, false, ConflictAction::Delete);

        assert_eq!(
            summary,
            RenameSummary {
                renamed: 2,
                removed: 1,
                failed: 0
            }
        );
        assert_eq!(std::fs::read(root.join("alpha.mp4"))?, b"new alpha");
        assert_eq!(std::fs::read(root.join("beta.mp4"))?, b"new beta");
        assert!(root.join("gamma_1.mp4").exists());
        Ok(())
    }

    #[test]
    fn dryrun_summary_counts_without_changes() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let root = &temp_directory.path().join("videos");
        std::fs::create_dir(root)?;
        std::fs::write(root.join("alpha_1.mp4"), b"new alpha")?;
        std::fs::write(root.join("alpha.mp4"), b"old alpha")?;

        let summary = rename_files(root, true, true, ConflictAction::trash());

        assert_eq!(
            summary,
            RenameSummary {
                renamed: 1,
                removed: 1,
                failed: 0
            }
        );
        assert_eq!(std::fs::read(root.join("alpha.mp4"))?, b"old alpha");
        assert!(root.join("alpha_1.mp4").exists());
        Ok(())
    }

    #[test]
    fn empty_directory_has_empty_summary() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;

        let summary = rename_files(temp_directory.path(), true, false, ConflictAction::Delete);

        assert_eq!(summary, RenameSummary::default());
        Ok(())
    }
}

#[cfg(test)]
mod test_process_file {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_name_that_is_not_utf8_is_sanitized() -> anyhow::Result<()> {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let temp_directory = tempfile::TempDir::new()?;
        let suffixed = temp_directory.path().join(OsStr::from_bytes(b"cl\xfeip_1.m\xffv"));
        let expected = temp_directory.path().join("clip.mv");
        let unrelated = temp_directory.path().join("clip");
        // File systems such as APFS reject names that are not valid UTF-8, so there is nothing to test there.
        if std::fs::write(&suffixed, b"new").is_err() {
            return Ok(());
        }
        std::fs::write(&unrelated, b"unrelated")?;

        let outcome = process_file(temp_directory.path(), &suffixed, 1, 1, 1, false, ConflictAction::Delete)?;

        assert!(matches!(outcome, Outcome::Renamed));
        assert_eq!(std::fs::read(&expected)?, b"new");
        assert_eq!(std::fs::read(&unrelated)?, b"unrelated");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn a_sanitized_name_never_removes_an_existing_file() -> anyhow::Result<()> {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let temp_directory = tempfile::TempDir::new()?;
        let first = temp_directory.path().join(OsStr::from_bytes(b"cl\xfeip_1.mkv"));
        let second = temp_directory.path().join(OsStr::from_bytes(b"cl\xffip_1.mkv"));
        let target = temp_directory.path().join("clip.mkv");
        if std::fs::write(&first, b"first").is_err() {
            return Ok(());
        }
        std::fs::write(&second, b"second")?;

        let first_outcome = process_file(temp_directory.path(), &first, 1, 2, 1, false, ConflictAction::Delete)?;
        let second_outcome = process_file(temp_directory.path(), &second, 2, 2, 1, false, ConflictAction::Delete)?;

        assert!(matches!(first_outcome, Outcome::Renamed));
        assert!(matches!(second_outcome, Outcome::Skipped));
        assert_eq!(std::fs::read(&target)?, b"first");
        assert_eq!(std::fs::read(&second)?, b"second");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn an_extension_with_no_valid_characters_is_an_error() -> anyhow::Result<()> {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let temp_directory = tempfile::TempDir::new()?;
        let suffixed = temp_directory.path().join(OsStr::from_bytes(b"clip_1.\xff"));
        let unrelated = temp_directory.path().join("clip");
        if std::fs::write(&suffixed, b"new").is_err() {
            return Ok(());
        }
        std::fs::write(&unrelated, b"unrelated")?;

        let result = process_file(temp_directory.path(), &suffixed, 1, 1, 1, false, ConflictAction::Delete);

        assert!(result.is_err());
        assert_eq!(std::fs::read(&unrelated)?, b"unrelated");
        assert_eq!(std::fs::read(&suffixed)?, b"new");
        Ok(())
    }

    #[test]
    fn broken_symlink_at_destination_is_counted_as_conflict_in_dryrun() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let suffixed = temp_directory.path().join("clip_1.txt");
        let broken = temp_directory.path().join("clip.txt");
        std::fs::write(&suffixed, b"new")?;
        if !cli_tools_common::test_support::create_symlink(&temp_directory.path().join("missing.txt"), &broken) {
            return Ok(());
        }

        let summary = rename_files(temp_directory.path(), true, true, ConflictAction::trash());
        assert_eq!(summary.renamed, 1);
        assert_eq!(
            summary.removed, 1,
            "broken symlink at destination must be reported as a conflict removal"
        );
        Ok(())
    }

    #[test]
    fn valid_trailing_dot_filename_renames_cleanly() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let suffixed = temp_directory.path().join("clip_1.");
        std::fs::write(&suffixed, b"data")?;

        let summary = rename_files(temp_directory.path(), true, false, ConflictAction::Delete);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.renamed, 1);
        assert_eq!(std::fs::read(temp_directory.path().join("clip."))?, b"data");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn stem_with_non_utf8_bytes_after_suffix_does_not_match() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        assert!(!stem_ends_with_rx_suffix(OsStr::from_bytes(b"clip_1\xff")));
        assert!(stem_ends_with_rx_suffix(OsStr::from_bytes(b"clip_1")));
    }

    #[cfg(windows)]
    #[test]
    fn stem_with_unpaired_surrogate_after_suffix_does_not_match() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        let clip: Vec<u16> = "clip_1".encode_utf16().collect();
        let after = OsString::from_wide(&[clip.as_slice(), &[0xD800]].concat());
        let before = OsString::from_wide(&[&[0xD800], clip.as_slice()].concat());

        assert!(!stem_ends_with_rx_suffix(&after));
        assert!(stem_ends_with_rx_suffix(&before));
    }

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

        let outcome = process_file(temp_directory.path(), &suffixed, 1, 1, 1, true, ConflictAction::trash())?;

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
    fn network_trash_refuses_conflicts_without_modifying_either_file() {
        use cli_tools_common::test_support::TempTree;

        let tree = TempTree::new();
        tree.file_with("clip.txt", "old destination");
        tree.file_with("clip_1.txt", "replacement");
        let before = tree.snapshot();
        let action = ConflictAction::Trash {
            is_network: |_| true,
            dispose: |_| panic!("network conflict must not reach the trash backend"),
        };

        // Check real execution first so a deletion failure is visible in the regression.
        for dryrun in [false, true] {
            let result = process_file(tree.path(), &tree.join("clip_1.txt"), 1, 1, 1, dryrun, action);

            assert_eq!(tree.snapshot(), before);
            let error = result.expect_err("explicit trash must refuse a network conflict");
            assert!(error.to_string().contains("network"), "{error}");
        }
    }

    #[test]
    fn network_trash_allows_a_rename_without_a_conflict() {
        use cli_tools_common::test_support::TempTree;

        let tree = TempTree::new();
        tree.file_with("clip_1.txt", "replacement");
        let action = ConflictAction::Trash {
            is_network: |_| true,
            dispose: |_| panic!("there is no old destination to trash"),
        };

        let summary = rename_files(tree.path(), true, false, action);

        assert_eq!(summary.renamed, 1);
        assert_eq!(summary.removed, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(tree.snapshot(), ["clip.txt: replacement"]);
    }

    #[test]
    fn delete_mode_keeps_destination_when_source_rename_fails() {
        use cli_tools_common::test_support::TempTree;

        let tree = TempTree::new();
        tree.file_with("clip.txt", "old destination");
        let before = tree.snapshot();

        // Simulate a source that vanished after scanning, so the real rename fails on any platform.
        let result = process_file(
            tree.path(),
            &tree.join("clip_1.txt"),
            1,
            1,
            1,
            false,
            ConflictAction::Delete,
        );

        assert!(result.is_err());
        assert_eq!(tree.snapshot(), before);
    }

    #[test]
    fn trash_mode_keeps_destination_when_source_rename_fails() {
        use cli_tools_common::test_support::TempTree;

        let tree = TempTree::new();
        tree.file_with("clip.txt", "old destination");
        let before = tree.snapshot();

        let result = replace_with_trash(&tree.join("clip_1.txt"), &tree.join("clip.txt"), |path| {
            std::fs::remove_file(path)
        });

        assert!(result.is_err());
        assert_eq!(tree.snapshot(), before);
    }

    #[test]
    fn trash_mode_disposes_of_old_destination_only_after_installing_replacement() -> anyhow::Result<()> {
        use cli_tools_common::test_support::TempTree;

        let tree = TempTree::new();
        tree.file_with("clip.txt", "old destination");
        tree.file_with("clip_1.txt", "replacement");
        let target = tree.join("clip.txt");

        replace_with_trash(&tree.join("clip_1.txt"), &target, |backup| {
            assert_eq!(std::fs::read(backup)?, b"old destination");
            assert_eq!(std::fs::read(&target)?, b"replacement");
            std::fs::remove_file(backup)
        })?;

        assert_eq!(tree.snapshot(), ["clip.txt: replacement"]);
        Ok(())
    }

    #[test]
    fn trash_failure_leaves_old_destination_in_recovery_directory() {
        use cli_tools_common::test_support::TempTree;

        let tree = TempTree::new();
        tree.file_with("clip.txt", "old destination");
        tree.file_with("clip_1.txt", "replacement");
        let mut backup_path = None;

        let result = replace_with_trash(&tree.join("clip_1.txt"), &tree.join("clip.txt"), |backup| {
            backup_path = Some(backup.to_path_buf());
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "trash unavailable",
            ))
        });

        let backup_path = backup_path.unwrap();
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains(&*backup_path.to_string_lossy())
        );
        assert_eq!(std::fs::read(backup_path).unwrap(), b"old destination");
        assert_eq!(tree.read("clip.txt"), "replacement");
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

#[cfg(test)]
mod test_nested_trees {
    use cli_tools_common::test_support::TempTree;

    use super::*;

    /// Tree with suffixed files at every depth, some with an existing unsuffixed duplicate.
    fn nested_tree() -> TempTree {
        let tree = TempTree::new();
        tree.file_with("top_1.mp4", "new top");
        tree.file_with("top.mp4", "old top");
        tree.file_with("a/song_1.mp3", "new song");
        tree.file_with("a/b/clip_1.mkv", "new clip");
        tree.file_with("a/b/clip.mkv", "old clip");
        tree.file_with("a/b/c/photo_1.jpg", "new photo");
        tree.file_with("a/b/c/keep_10.jpg", "keep");
        tree.file_with("a/b/c/d/no_extension_1", "bare");
        tree
    }

    #[test]
    fn renames_suffixed_files_at_every_depth() {
        let tree = nested_tree();

        let summary = rename_files(tree.path(), true, false, ConflictAction::Delete);

        assert_eq!(
            summary,
            RenameSummary {
                renamed: 5,
                removed: 2,
                failed: 0
            }
        );
        assert_eq!(
            tree.snapshot(),
            vec![
                "a/",
                "a/b/",
                "a/b/c/",
                "a/b/c/d/",
                "a/b/c/d/no_extension: bare",
                "a/b/c/keep_10.jpg: keep",
                "a/b/c/photo.jpg: new photo",
                "a/b/clip.mkv: new clip",
                "a/song.mp3: new song",
                "top.mp4: new top",
            ]
        );
    }

    #[test]
    fn dryrun_leaves_the_tree_unchanged() {
        let tree = nested_tree();
        let before = tree.snapshot();

        let summary = rename_files(tree.path(), true, true, ConflictAction::Delete);

        assert_eq!(summary.renamed, 5);
        assert_eq!(summary.removed, 2);
        assert_eq!(tree.snapshot(), before);
    }

    #[test]
    fn second_run_changes_nothing() {
        let tree = nested_tree();
        rename_files(tree.path(), true, false, ConflictAction::Delete);
        let after_first_run = tree.snapshot();

        let summary = rename_files(tree.path(), true, false, ConflictAction::Delete);

        assert_eq!(summary, RenameSummary::default());
        assert_eq!(tree.snapshot(), after_first_run);
    }

    #[test]
    fn hidden_files_and_directories_are_skipped() {
        let tree = TempTree::new();
        tree.file(".hidden_1.txt");
        tree.file(".secret/deep/clip_1.mp4");
        tree.file("visible/clip_1.mp4");

        rename_files(tree.path(), true, false, ConflictAction::Delete);

        assert_eq!(
            tree.files(),
            vec![".hidden_1.txt", ".secret/deep/clip_1.mp4", "visible/clip.mp4"]
        );
    }

    #[test]
    fn same_name_in_different_directories_is_renamed_in_each() {
        let tree = TempTree::new();
        tree.file_with("one/x_1.txt", "one");
        tree.file_with("one/two/x_1.txt", "two");
        tree.file_with("one/two/three/x_1.txt", "three");

        rename_files(tree.path(), true, false, ConflictAction::Delete);

        assert_eq!(tree.read("one/x.txt"), "one");
        assert_eq!(tree.read("one/two/x.txt"), "two");
        assert_eq!(tree.read("one/two/three/x.txt"), "three");
    }

    #[test]
    fn chained_suffixes_are_removed_one_level_per_file() {
        let tree = TempTree::new();
        tree.file_with("x_1_1.txt", "newest");
        tree.file_with("x_1.txt", "newer");

        let preview = rename_files(tree.path(), true, true, ConflictAction::Delete);
        let summary = rename_files(tree.path(), true, false, ConflictAction::Delete);

        assert_eq!(preview, summary);
        assert_eq!(summary.renamed, 2);
        assert_eq!(summary.removed, 0);
        assert_eq!(tree.snapshot(), vec!["x.txt: newer", "x_1.txt: newest"]);
    }

    #[test]
    fn a_name_that_is_only_the_suffix_is_left_alone() {
        let tree = TempTree::new();
        tree.file_with("folder/_1.txt", "only suffix");
        tree.file_with("folder/_1", "bare suffix");

        rename_files(tree.path(), true, false, ConflictAction::Delete);

        assert_eq!(
            tree.snapshot(),
            vec!["folder/", "folder/_1.txt: only suffix", "folder/_1: bare suffix"]
        );
    }

    #[test]
    fn empty_directory_is_a_no_op() {
        let tree = TempTree::with_entries(&["empty/nested/"]);

        let summary = rename_files(tree.path(), true, false, ConflictAction::Delete);

        assert_eq!(summary, RenameSummary::default());
        assert_eq!(tree.snapshot(), vec!["empty/", "empty/nested/"]);
    }

    #[test]
    fn suffixed_symlinks_are_not_renamed_and_their_targets_are_untouched() {
        let tree = TempTree::new();
        tree.file_with("outside/real_1.txt", "real");
        tree.file_with("outside/folder/inner_1.txt", "inner");
        if tree.try_symlink("outside/real_1.txt", "walked/link_1.txt").is_none()
            || tree.try_symlink("outside/folder", "walked/folder_1").is_none()
        {
            return;
        }

        rename_files(&tree.join("walked"), true, false, ConflictAction::Delete);

        assert_eq!(tree.read("outside/real_1.txt"), "real");
        assert_eq!(tree.read("outside/folder/inner_1.txt"), "inner");
        assert!(tree.exists("walked/link_1.txt"));
        assert!(tree.exists("walked/folder_1"));
    }

    #[test]
    fn a_conflicting_symlink_is_removed_without_touching_its_target() {
        let tree = TempTree::new();
        tree.file_with("outside/original.txt", "original");
        tree.file_with("walked/clip_1.txt", "new");
        if tree.try_symlink("outside/original.txt", "walked/clip.txt").is_none() {
            return;
        }

        rename_files(&tree.join("walked"), true, false, ConflictAction::Delete);

        assert_eq!(tree.read("outside/original.txt"), "original");
        assert_eq!(tree.read("walked/clip.txt"), "new");
        assert!(!tree.join("walked/clip.txt").is_symlink());
    }

    #[test]
    fn a_symlink_to_a_directory_blocks_the_rename() {
        let tree = TempTree::new();
        tree.file_with("outside/folder/inner.txt", "inner");
        tree.file_with("walked/clip_1.txt", "new");
        if tree.try_symlink("outside/folder", "walked/clip.txt").is_none() {
            return;
        }

        rename_files(&tree.join("walked"), true, false, ConflictAction::Delete);

        assert_eq!(tree.read("walked/clip_1.txt"), "new");
        assert_eq!(tree.read("outside/folder/inner.txt"), "inner");
    }

    #[test]
    fn a_target_differing_only_by_case_is_treated_consistently() {
        let tree = TempTree::new();
        tree.file_with("Clip_1.txt", "new");
        tree.file_with("clip.txt", "old");
        let case_sensitive =
            cli_tools_common::directory_is_case_sensitive(tree.path()).expect("case probe should succeed");

        let summary = rename_files(tree.path(), true, false, ConflictAction::Delete);

        assert_eq!(summary.failed, 0);
        if case_sensitive {
            assert_eq!(tree.snapshot(), vec!["Clip.txt: new", "clip.txt: old"]);
        } else {
            assert_eq!(tree.files().len(), 1, "{:?}", tree.files());
            assert_eq!(tree.read("Clip.txt"), "new");
        }
    }
}
