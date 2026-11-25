//! Printing what the `dirmove` passes find and do.
//!
//! Formats unpack summaries, directory merge progress, whole directory merge offer titles,
//! and the debug listings of match groups and grouping names.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use colored::Colorize;

use cli_tools_common::{
    count_label, get_relative_path_or_filename, path_to_filename_string, path_to_string_relative, print_magenta,
    print_yellow,
};
use cli_tools_dirmove::{DirectoryInfo, FileInfo, FileMatch, MergeEvent, MoveInfo, Roots, UnpackInfo};

/// Print summary of what will be unpacked.
pub fn print_unpack_summary(target_dir: &Path, info: &UnpackInfo, verbose: bool) {
    let dir_display = path_to_string_relative(target_dir);
    let file_count = info.file_moves.len();
    let dir_count = info.directory_moves.len();

    let mut counts = Vec::new();
    if dir_count > 0 {
        counts.push(format!("{dir_count} d"));
    }
    if file_count > 0 {
        counts.push(format!("{file_count} f"));
    }
    let counts_str = if counts.is_empty() {
        String::new()
    } else {
        format!(" ({})", counts.join(", "))
    };
    let header = format!("Unpacking: {dir_display}{counts_str}");

    print_magenta!("{}", header.bold());

    for MoveInfo { source, target } in &info.directory_moves {
        let src_display = get_relative_path_or_filename(source, target_dir);
        let dst_display = get_relative_path_or_filename(target, target_dir);
        println!("  {src_display} -> {dst_display}");
    }
    if verbose {
        for MoveInfo { source, target } in &info.file_moves {
            let src_display = get_relative_path_or_filename(source, target_dir);
            let dst_display = get_relative_path_or_filename(target, target_dir);
            println!("  {src_display} -> {dst_display}");
        }
    }
}

/// Print one step of a directory merge.
pub fn print_merge_event(event: &MergeEvent<'_>, roots: &Roots, dryrun: bool, verbose: bool) {
    match *event {
        MergeEvent::Started { source, target } => {
            let source_display = roots.merge_display_path(source);
            let target_display = roots.merge_display_path(target);
            print_magenta!("{}", format!("Merging: {source_display} -> {target_display}").bold());
        }
        MergeEvent::Overwriting(name) if verbose => println!("  {} Overwriting: {name}", "→".yellow()),
        MergeEvent::WouldMove(name) if verbose => println!("  {} Would move: {name}", "→".cyan()),
        MergeEvent::Skipped(names) => {
            print_yellow!(
                "Skipped {} that already exist (use -f to overwrite):",
                count_label(names.len(), "file", "files")
            );
            for name in names {
                println!("    {name}");
            }
        }
        MergeEvent::Moved(count) => {
            let action = if dryrun { "Would move" } else { "Moved" };
            println!("  {action} {}", count_label(count, "item", "items"));
        }
        MergeEvent::SourceRemoved(source) if verbose => {
            println!(
                "  {} Removed empty directory: {}",
                "→".green(),
                roots.merge_display_path(source)
            );
        }
        MergeEvent::SourceRemoveFailed(source, error) => {
            print_yellow!(
                "Failed to remove empty directory {}: {error}",
                roots.merge_display_path(source)
            );
        }
        MergeEvent::WouldRemoveSource(source) if verbose => {
            println!(
                "  {} Would remove empty directory: {}",
                "→".yellow(),
                roots.merge_display_path(source)
            );
        }
        _ => {}
    }
}

/// Print match groups with file counts, sorted by count descending.
pub fn print_match_debug(matches: &HashMap<usize, Vec<FileMatch>>, directories: &[DirectoryInfo]) {
    eprintln!("Directory match groups:");
    let mut sorted_matches: Vec<_> = matches.iter().collect();
    sorted_matches.sort_by_key(|entry| std::cmp::Reverse(entry.1.len()));
    for (&directory_index, matched_files) in sorted_matches {
        let Some(directory) = directories.get(directory_index) else {
            continue;
        };
        eprintln!(
            "  {} -> {}",
            directory.name,
            count_label(matched_files.len(), "file", "files")
        );
    }
    if matches.is_empty() {
        eprintln!("  (no matches found)");
    }
    eprintln!();
}

/// Print the unique processed names used for grouping.
pub fn print_grouping_names_debug(files_with_names: &[FileInfo<'_>]) {
    let unique_names: HashSet<_> = files_with_names.iter().map(|f| f.filtered_name.as_ref()).collect();
    let mut sorted_names: Vec<_> = unique_names.into_iter().collect();
    sorted_names.sort_unstable();
    eprintln!("Unique processed names for grouping:");
    for name in sorted_names {
        eprintln!("  {name}");
    }
}

/// Format the title for a whole directory merge offer.
pub fn directory_merge_title(source_file_counts: &[(&Path, usize)]) -> String {
    source_file_counts
        .first()
        .map_or_else(String::new, |(source_directory, _)| {
            path_to_filename_string(source_directory)
        })
}

#[cfg(test)]
mod test_directory_merge_title {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn directory_merge_title_uses_single_source_directory() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root = tmp.path().join("input");
        let output_root = tmp.path().join("output");
        let input_directory = input_root.join("Example");
        std::fs::create_dir_all(&input_directory)?;
        std::fs::create_dir_all(&output_root)?;

        let source_file_counts = [(input_directory.as_path(), 0)];

        assert_eq!(directory_merge_title(&source_file_counts), "Example");
        Ok(())
    }

    #[test]
    fn directory_merge_title_uses_plain_name_for_multiple_sources() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let input_root_a = tmp.path().join("input_a");
        let input_root_b = tmp.path().join("input_b");
        let output_root = tmp.path().join("output");
        let input_directory_a = input_root_a.join("Example");
        let input_directory_b = input_root_b.join("Example");
        std::fs::create_dir_all(&input_directory_a)?;
        std::fs::create_dir_all(&input_directory_b)?;
        std::fs::create_dir_all(&output_root)?;

        let source_file_counts = [(input_directory_a.as_path(), 0), (input_directory_b.as_path(), 0)];

        assert_eq!(directory_merge_title(&source_file_counts), "Example");
        Ok(())
    }
}
