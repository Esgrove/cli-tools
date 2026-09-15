//! Core duplicate finder orchestration, grouping, and file operations.
//!
//! This module defines `DupeFind`, coordinates directory scanning and duplicate detection,
//! and handles duplicate filtering, display, and moves.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use colored::Colorize;
#[cfg(not(test))]
use indicatif::ProgressBar;
#[cfg(not(test))]
use indicatif::ProgressStyle;
use itertools::Itertools;
use rayon::iter::IntoParallelRefIterator;
use rayon::iter::ParallelIterator;
use walkdir::WalkDir;

use cli_tools::dupe_find::{
    DupeFileInfo, DuplicateGroup, DuplicateMatchOptions, filter_ignored_groups, find_duplicates,
    format_filename_with_highlight, group_matches_ignore,
};
use cli_tools::{print_error, print_yellow};

use crate::config::{Config, DupeConfig};
#[cfg(not(test))]
use crate::helpers::SPINNER_TEMPLATE;
use crate::{Args, helpers};

/// Duplicate file finder that scans directories for duplicate video files.
pub struct DupeFind {
    config: Config,
    roots: Vec<PathBuf>,
}

impl DupeFind {
    pub fn new(args: Args) -> anyhow::Result<Self> {
        let user_config = DupeConfig::get_user_config()?;

        // Resolve all input paths:
        // - If default flag is set, use default_paths from config
        // - CLI args take priority, then config file, then current directory
        let roots = if args.default && !user_config.default_paths.is_empty() {
            user_config
                .default_paths
                .iter()
                .map(|p| cli_tools::resolve_required_input_path(p))
                .collect::<anyhow::Result<Vec<_>>>()?
        } else if !args.paths.is_empty() {
            args.paths
                .iter()
                .map(|p| cli_tools::resolve_required_input_path(p))
                .collect::<anyhow::Result<Vec<_>>>()?
        } else if !user_config.paths.is_empty() {
            user_config
                .paths
                .iter()
                .map(|p| cli_tools::resolve_required_input_path(p))
                .collect::<anyhow::Result<Vec<_>>>()?
        } else {
            vec![cli_tools::resolve_input_path(None)?]
        };

        let config = Config::from_args(args)?;

        Ok(Self { config, roots })
    }

    pub fn run(&self) -> anyhow::Result<()> {
        if self.config.verbose {
            let paths_display = self
                .roots
                .iter()
                .map(|path| cli_tools::path_to_string(path))
                .collect::<Vec<_>>()
                .join(", ");
            println!("Scanning paths: {}", paths_display.magenta());
        }

        if self.config.debug {
            println!("Extensions: {:?}", self.config.extensions);
            if !self.config.patterns.is_empty() {
                println!("Patterns:");
                for pattern in &self.config.patterns {
                    println!("  {}", pattern.as_str());
                }
            }
            if !self.config.ignore_matches.is_empty() {
                println!("Ignore matches: {:?}", self.config.ignore_matches);
            }
            if !self.config.prefix_ignores.is_empty() {
                println!("Prefix ignores: {:?}", self.config.prefix_ignores);
            }
            println!("Hash comparison: {}", self.config.hash_compare);
        }

        let files = self.gather_files();
        if self.config.verbose {
            println!("Checking {} files for duplicates...", files.len());
        }
        let hash_matches = helpers::find_hash_matches(&files, self.config.hash_compare, self.config.verbose);
        let duplicates = find_duplicates(
            &files,
            &DuplicateMatchOptions {
                patterns: &self.config.patterns,
                prefix_ignores: &self.config.prefix_ignores,
                additional_match_groups: &hash_matches,
            },
        );
        if self.config.verbose {
            for group in duplicates
                .iter()
                .filter(|group| group_matches_ignore(group, &self.config.ignore_matches))
            {
                println!("Ignoring group: {}", group.display_name());
            }
        }
        let duplicates = filter_ignored_groups(duplicates, &self.config.ignore_matches);

        if duplicates.is_empty() {
            println!("{}", "No duplicates found".green());
            return Ok(());
        }

        // Interactive mode when not in print/dryrun mode
        if !self.config.dryrun {
            let metadata = helpers::collect_metadata_for_groups(&duplicates);
            return crate::tui::run_interactive(&duplicates, &metadata);
        }

        // Print-only mode
        println!(
            "{}",
            format!("Found {} duplicate groups:", duplicates.len()).yellow().bold()
        );

        for group in &duplicates {
            println!("\n{}:", group.key.cyan());
            for file in group.files.iter().sorted_by_key(|f| &f.path) {
                let display_name = format_filename_with_highlight(&file.filename, file.pattern_match);
                println!("  {display_name}");
            }
        }

        if self.config.move_files {
            self.move_duplicates(&duplicates)?;
        }

        Ok(())
    }

    /// Collect all video files from all root directories in parallel.
    /// Shows a spinner progress bar while scanning.
    fn gather_files(&self) -> Vec<DupeFileInfo> {
        #[cfg(not(test))]
        let progress_bar = {
            let pb = ProgressBar::new_spinner();
            pb.set_style(
                ProgressStyle::default_spinner()
                    .template(SPINNER_TEMPLATE)
                    .expect("Failed to set spinner template"),
            );
            pb.set_message("Scanning directories");
            pb
        };

        let files: Mutex<Vec<DupeFileInfo>> = Mutex::new(Vec::new());

        // Process each root directory in parallel
        self.roots.par_iter().for_each(|root| {
            let collected_files = self.collect_video_files_from_root(
                root,
                #[cfg(not(test))]
                &progress_bar,
            );
            if let Ok(mut all_files) = files.lock() {
                all_files.extend(collected_files);
            }
        });

        #[cfg(not(test))]
        progress_bar.finish_and_clear();

        files.into_inner().unwrap_or_default()
    }

    /// Collect video files from a single root directory.
    fn collect_video_files_from_root(
        &self,
        root: &Path,
        #[cfg(not(test))] progress_bar: &ProgressBar,
    ) -> Vec<DupeFileInfo> {
        let walker = if self.config.recurse {
            WalkDir::new(root)
        } else {
            WalkDir::new(root).max_depth(1)
        };

        walker
            .into_iter()
            .filter_entry(|e| !cli_tools::should_skip_entry(e))
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .filter_map(|entry| {
                let path = entry.path();
                let extension = cli_tools::path_to_file_extension_string(path);
                if self.config.extensions.contains(&extension) {
                    #[cfg(not(test))]
                    progress_bar.inc(1);
                    Some(DupeFileInfo::new(path.to_path_buf(), extension))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Move duplicate files to a Duplicates directory
    fn move_duplicates(&self, duplicates: &[DuplicateGroup]) -> anyhow::Result<()> {
        // Use the first root as the duplicates directory location
        let duplicates_dir = self.roots.first().map_or_else(
            || {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join("Duplicates")
            },
            |r| r.join("Duplicates"),
        );

        println!(
            "{}",
            format!("\nMoving duplicates to {}", duplicates_dir.display())
                .magenta()
                .bold()
        );

        for group in duplicates {
            let group_name = group.display_name();

            for file in &group.files {
                let target_dir = duplicates_dir.join(&group_name);
                let target_path = cli_tools::get_unique_path(&target_dir, &file.filename, &file.stem, &file.extension);

                println!(
                    "{}: {}",
                    if self.config.dryrun {
                        "[DRYRUN] Move".magenta()
                    } else {
                        "Move".magenta()
                    },
                    cli_tools::path_to_string_relative(&target_path)
                );

                if self.config.dryrun {
                    continue;
                }

                if cli_tools::get_user_confirmation("Move file?", false)? {
                    // Create directory if needed
                    if let Err(e) = std::fs::create_dir_all(&target_dir) {
                        print_yellow!("Failed to create directory {}: {e}", target_dir.display());
                        continue;
                    }

                    match std::fs::rename(&file.path, &target_path) {
                        Ok(()) => println!("{}", "Moved".green()),
                        Err(e) => print_error!("Failed to move file: {e}"),
                    }
                } else {
                    println!("Skipped");
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod test_gather_files {
    use std::fs;

    use super::*;

    fn make_finder(root: PathBuf, recurse: bool) -> DupeFind {
        DupeFind {
            roots: vec![root],
            config: Config {
                debug: false,
                dryrun: true,
                extensions: vec!["mp4".to_string()],
                hash_compare: false,
                ignore_matches: vec![],
                move_files: false,
                patterns: vec![],
                prefix_ignores: vec![],
                recurse,
                verbose: false,
            },
        }
    }

    #[test]
    fn includes_configured_extensions_and_skips_other_files() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        let scan_root = temp_directory.path().join("videos");
        fs::create_dir(&scan_root).expect("should create scan root");
        fs::write(scan_root.join("video.mp4"), b"video").expect("should write video file");
        fs::write(scan_root.join("notes.txt"), b"notes").expect("should write text file");
        let finder = make_finder(scan_root, false);

        let files = finder.gather_files();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "video.mp4");
    }

    #[test]
    fn respects_recursive_scanning_option() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        let scan_root = temp_directory.path().join("videos");
        let nested_directory = scan_root.join("nested");
        fs::create_dir_all(&nested_directory).expect("should create nested directory");
        fs::write(nested_directory.join("video.mp4"), b"video").expect("should write nested video file");

        let non_recursive = make_finder(scan_root.clone(), false).gather_files();
        let recursive = make_finder(scan_root, true).gather_files();

        assert!(non_recursive.is_empty());
        assert_eq!(recursive.len(), 1);
        assert_eq!(recursive[0].filename, "video.mp4");
    }
}

#[cfg(test)]
mod test_move_duplicates {
    use super::*;
    use cli_tools::dupe_find::DupeFileInfo;

    /// Finder rooted at the directory, always in dryrun mode so no test moves a file.
    fn dryrun_finder(root: PathBuf) -> DupeFind {
        DupeFind {
            roots: vec![root],
            config: Config {
                debug: false,
                dryrun: true,
                extensions: vec!["mp4".to_string()],
                hash_compare: false,
                ignore_matches: vec![],
                move_files: true,
                patterns: vec![],
                prefix_ignores: vec![],
                recurse: false,
                verbose: false,
            },
        }
    }

    /// Duplicate group of files created inside the directory.
    fn group_in(directory: &Path, names: &[&str]) -> DuplicateGroup {
        let files: Vec<DupeFileInfo> = names
            .iter()
            .map(|name| {
                let path = directory.join(name);
                std::fs::write(&path, b"video").expect("file should be written");
                DupeFileInfo::new(path, "mp4".to_string())
            })
            .collect();
        DuplicateGroup::new("movie".to_string(), files)
    }

    #[test]
    fn a_dryrun_moves_nothing_and_creates_no_directory() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let group = group_in(directory.path(), &["movie.1080p.mp4", "movie.720p.mp4"]);

        dryrun_finder(directory.path().to_path_buf())
            .move_duplicates(&[group])
            .expect("a dryrun should succeed");

        assert!(directory.path().join("movie.1080p.mp4").is_file());
        assert!(directory.path().join("movie.720p.mp4").is_file());
        assert!(
            !directory.path().join("Duplicates").exists(),
            "a dryrun should not create the duplicates directory"
        );
    }

    #[test]
    fn no_duplicate_groups_is_a_no_op() {
        let directory = tempfile::TempDir::new().expect("temporary directory");

        dryrun_finder(directory.path().to_path_buf())
            .move_duplicates(&[])
            .expect("an empty list should succeed");

        assert!(!directory.path().join("Duplicates").exists());
    }

    #[test]
    fn several_groups_are_all_reported() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let first = group_in(directory.path(), &["one.1080p.mp4", "one.720p.mp4"]);
        let second = group_in(directory.path(), &["two.1080p.mp4", "two.720p.mp4"]);

        dryrun_finder(directory.path().to_path_buf())
            .move_duplicates(&[first, second])
            .expect("a dryrun should succeed");

        assert_eq!(
            std::fs::read_dir(directory.path())
                .expect("directory should be readable")
                .count(),
            4,
            "every file should still be in place"
        );
    }

    #[test]
    fn without_a_root_the_working_directory_is_used() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let group = group_in(directory.path(), &["movie.1080p.mp4"]);
        let finder = DupeFind {
            roots: Vec::new(),
            ..dryrun_finder(directory.path().to_path_buf())
        };

        finder.move_duplicates(&[group]).expect("a dryrun should succeed");

        assert!(directory.path().join("movie.1080p.mp4").is_file());
    }
}

#[cfg(test)]
mod test_new {
    use super::*;
    use clap::Parser;

    #[test]
    fn a_given_path_becomes_the_only_root() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let path = directory.path().to_string_lossy().into_owned();
        let args = Args::try_parse_from(["dupefind", &path]).expect("arguments should parse");

        let finder = DupeFind::new(args).expect("an existing path should be accepted");

        assert_eq!(finder.roots.len(), 1);
        assert!(finder.roots[0].ends_with(directory.path().file_name().expect("the directory should have a name")));
    }

    #[test]
    fn several_given_paths_all_become_roots() {
        let first = tempfile::TempDir::new().expect("temporary directory");
        let second = tempfile::TempDir::new().expect("temporary directory");
        let args = Args::try_parse_from([
            "dupefind",
            &first.path().to_string_lossy(),
            &second.path().to_string_lossy(),
        ])
        .expect("arguments should parse");

        let finder = DupeFind::new(args).expect("existing paths should be accepted");

        assert_eq!(finder.roots.len(), 2);
    }

    #[test]
    fn a_missing_path_is_rejected() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let missing = directory.path().join("missing");
        let args = Args::try_parse_from(["dupefind", &missing.to_string_lossy()]).expect("arguments should parse");

        assert!(DupeFind::new(args).is_err());
    }

    #[test]
    fn the_configured_paths_are_used_when_none_are_given() {
        let args = Args::try_parse_from(["dupefind"]).expect("arguments should parse");

        // The test config names paths that do not exist, so resolving them fails,
        // which is what proves the configured paths were the ones tried.
        let Err(error) = DupeFind::new(args) else {
            panic!("the configured paths do not exist, so this should fail");
        };

        assert!(
            error.to_string().contains("videos") || error.to_string().contains("movies"),
            "{error}"
        );
    }

    #[test]
    fn the_default_flag_uses_the_configured_default_paths() {
        let args = Args::try_parse_from(["dupefind", "--default"]).expect("arguments should parse");

        let Err(error) = DupeFind::new(args) else {
            panic!("the configured default path does not exist, so this should fail");
        };

        assert!(error.to_string().contains("default"), "{error}");
    }
}
