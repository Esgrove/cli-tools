use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use walkdir::WalkDir;

use cli_tools::{print_error, print_magenta_bold, resolve_input_path, should_skip_entry, trash_or_delete};

/// Result of processing a single file.
enum Outcome {
    /// File was renamed (no conflicting unsuffixed file existed).
    Renamed,
    /// Conflicting unsuffixed file was trashed and suffixed file was renamed.
    RenamedAndTrashed,
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

    rename_files(&root, args.dryrun);
    Ok(())
}

/// Scan `root` recursively for files with a trailing `_1` suffix and rename them.
///
/// When a matching unsuffixed file already exists, it is trashed before renaming.
/// Errors for individual files are printed and skipped without aborting.
/// Prints a summary of renamed and trashed file counts when finished.
fn rename_files(root: &Path, dryrun: bool) {
    let files = find_files_with_rx_duplicate_suffix(root);
    let total = files.len();
    let width = total.to_string().len();

    let mut rename_count: usize = 0;
    let mut trash_count: usize = 0;

    for (i, file) in files.iter().enumerate() {
        match process_file(root, file, i + 1, total, width, dryrun) {
            Ok(Outcome::RenamedAndTrashed) => {
                rename_count += 1;
                trash_count += 1;
            }
            Ok(Outcome::Renamed) => rename_count += 1,
            Ok(Outcome::Skipped) => {}
            Err(e) => {
                print_error!("{}: {e}", file.display());
            }
        }
    }

    let rename_action = if dryrun { "would be renamed" } else { "renamed" };
    let trash_action = if dryrun { "would be trashed" } else { "trashed" };

    println!("\n{rename_count} files {rename_action}");
    println!("{trash_count} files {trash_action}");
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
            trash_or_delete(&file_without_suffix)?;
        }
        std::fs::rename(file_with_suffix, &file_without_suffix)?;
    }

    Ok(if needs_trash {
        Outcome::RenamedAndTrashed
    } else {
        Outcome::Renamed
    })
}
