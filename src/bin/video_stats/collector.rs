//! Video file discovery, probing, and aggregate statistics collection.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use cli_tools::video_info::{VideoInfo, VideoStats};
use cli_tools::{create_semaphore_for_io_bound, print_error, print_yellow};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use walkdir::WalkDir;

use crate::VideoStatsArgs;

/// Supported video file extensions.
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "avi", "mov", "wmv", "webm", "m4v"];

/// Progress bar template string.
const PROGRESS_BAR_TEMPLATE: &str = "[{elapsed_precise}] {bar:80.magenta/blue} {pos}/{len} {percent}%";

/// Progress bar fill characters.
const PROGRESS_BAR_CHARS: &str = "=>-";

/// A probed video file with its display name and metadata.
struct ProbedFile {
    /// Display name (relative path or filename).
    name: String,
    /// Video metadata from ffprobe.
    info: VideoInfo,
}

/// Collects and displays statistics for video files.
pub struct StatsCollector {
    root: PathBuf,
    recurse: bool,
    verbose: bool,
}

impl StatsCollector {
    /// Create a new stats collector from command line arguments.
    ///
    /// # Errors
    /// Returns an error if the input path cannot be resolved.
    pub fn new(args: &VideoStatsArgs) -> Result<Self> {
        let input_path = cli_tools::resolve_input_path(args.path.as_deref())?;

        Ok(Self {
            root: input_path,
            recurse: args.recurse,
            verbose: args.verbose,
        })
    }

    /// Run the stats collection process.
    ///
    /// # Errors
    /// Returns an error if video files cannot be gathered or probed.
    pub fn run(&self) -> Result<()> {
        let video_files = self.gather_video_files()?;

        if video_files.is_empty() {
            if self.verbose {
                print_yellow!("No video files found in: {}", self.root.display());
            }
            return Ok(());
        }

        if self.verbose {
            println!(
                "{}",
                format!(
                    "Found {}",
                    cli_tools::count_label(video_files.len(), "video file", "video files")
                )
                .green()
                .bold()
            );
        }

        let runtime = tokio::runtime::Runtime::new()?;
        let (mut probed_files, error_count) = runtime.block_on(probe_files_async(video_files, &self.root));

        let mut stats = VideoStats::new();
        for probed in &probed_files {
            stats.add(&probed.info);
        }

        if self.verbose {
            // Sort by duration descending, then by resolution (pixel count) descending
            probed_files.sort_by(|a, b| {
                let duration_cmp = b
                    .info
                    .duration
                    .unwrap_or(0.0)
                    .total_cmp(&a.info.duration.unwrap_or(0.0));

                duration_cmp.then_with(|| {
                    let pixels_b = b.info.resolution.map_or(0, |r| r.pixel_count());
                    let pixels_a = a.info.resolution.map_or(0, |r| r.pixel_count());
                    pixels_b.cmp(&pixels_a)
                })
            });

            println!();
            for probed in &probed_files {
                Self::print_file_info(&probed.name, &probed.info);
            }
        }

        if error_count > 0 {
            println!(
                "{}",
                format!(
                    "{} could not be probed",
                    cli_tools::count_label(error_count, "file", "files")
                )
                .red()
            );
        }

        stats.print_summary(self.verbose);

        Ok(())
    }

    /// Print detailed info for a single video file.
    fn print_file_info(filename: &str, info: &VideoInfo) {
        let mut info_parts = Vec::new();

        if let Some(duration) = info.duration {
            info_parts.push(cli_tools::format_duration_seconds(duration));
        }
        if let Some(resolution) = info.resolution {
            info_parts.push(resolution.to_string());
        }
        if let Some(ref codec) = info.codec {
            info_parts.push(codec.clone());
        }
        if let Some(bitrate_kbps) = info.bitrate_kbps {
            info_parts.push(format!("{:.2} Mbps", bitrate_kbps as f64 / 1000.0));
        }
        if let Some(size_bytes) = info.size_bytes {
            info_parts.push(cli_tools::format_size(size_bytes));
        }

        if info_parts.is_empty() {
            println!("  {filename}");
        } else {
            println!("  {} | {}", filename.magenta(), info_parts.join(" | "));
        }
    }

    /// Gather all video files from the input path.
    fn gather_video_files(&self) -> Result<Vec<PathBuf>> {
        let mut video_files = Vec::new();

        if self.root.is_file() {
            if Self::is_video_file(&self.root) {
                video_files.push(self.root.clone());
            } else {
                anyhow::bail!("File '{}' is not a supported video file", self.root.display());
            }
        } else if self.root.is_dir() {
            if self.recurse {
                println!(
                    "{}",
                    format!("Searching recursively for video files in: {}", self.root.display()).magenta()
                );
                for entry in WalkDir::new(&self.root)
                    .into_iter()
                    .filter_entry(|e| !cli_tools::should_skip_entry(e))
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_file())
                {
                    let path = entry.path().to_path_buf();
                    if Self::is_video_file(&path) {
                        video_files.push(path);
                    }
                }
            } else {
                println!(
                    "{}",
                    format!("Searching for video files in: {}", self.root.display()).magenta()
                );
                for entry in std::fs::read_dir(&self.root)? {
                    let entry = entry?;
                    let path = entry.path();
                    if path.is_file() && Self::is_video_file(&path) {
                        video_files.push(path);
                    }
                }
            }
        } else {
            anyhow::bail!("Path '{}' does not exist", self.root.display());
        }

        Ok(video_files)
    }

    /// Check if a file is a video file based on its extension.
    fn is_video_file(path: &Path) -> bool {
        path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| {
            VIDEO_EXTENSIONS
                .iter()
                .any(|video_ext| video_ext.eq_ignore_ascii_case(ext))
        })
    }
}

/// Probe video files concurrently using semaphore-limited async tasks.
///
/// Each ffprobe call runs in a blocking task with concurrency controlled
/// by a semaphore sized for I/O-bound work (`num_cpus * 2`).
/// Returns the successfully probed files and the number of errors.
async fn probe_files_async(files: Vec<PathBuf>, root: &Path) -> (Vec<ProbedFile>, usize) {
    let semaphore = create_semaphore_for_io_bound();

    let progress_bar = Arc::new(ProgressBar::new(files.len() as u64));
    progress_bar.set_style(
        ProgressStyle::default_bar()
            .template(PROGRESS_BAR_TEMPLATE)
            .expect("Failed to set progress bar template")
            .progress_chars(PROGRESS_BAR_CHARS),
    );

    let tasks: Vec<_> = files
        .into_iter()
        .map(|path| {
            let name = cli_tools::get_relative_path_or_filename(&path, root);
            let semaphore = Arc::clone(&semaphore);
            let progress = Arc::clone(&progress_bar);
            tokio::spawn(async move {
                let permit = semaphore.acquire().await.expect("Failed to acquire semaphore");
                let result = tokio::task::spawn_blocking(move || VideoInfo::from_path(&path))
                    .await
                    .expect("spawn_blocking task failed");
                drop(permit);
                progress.inc(1);
                match result {
                    Ok(info) => Ok(ProbedFile { name, info }),
                    Err(error) => Err((name, error)),
                }
            })
        })
        .collect();

    let results = futures::future::join_all(tasks).await;
    progress_bar.finish_and_clear();

    let mut probed_files = Vec::new();
    let mut error_count: usize = 0;

    for result in results {
        match result.expect("Probe task failed") {
            Ok(probed) => probed_files.push(probed),
            Err((name, error)) => {
                print_error!("Failed to probe {name}: {error}");
                error_count += 1;
            }
        }
    }

    (probed_files, error_count)
}

#[cfg(test)]
mod test_video_file_discovery {
    use super::*;

    fn collector(root: PathBuf, recurse: bool) -> StatsCollector {
        StatsCollector {
            root,
            recurse,
            verbose: false,
        }
    }

    #[test]
    fn recognizes_supported_extensions_case_insensitively() {
        assert!(StatsCollector::is_video_file(Path::new("video.mp4")));
        assert!(StatsCollector::is_video_file(Path::new("video.MKV")));
        assert!(StatsCollector::is_video_file(Path::new("video.webm")));
        assert!(!StatsCollector::is_video_file(Path::new("notes.txt")));
        assert!(!StatsCollector::is_video_file(Path::new("README")));
    }

    #[test]
    fn gathers_supported_single_file_and_rejects_unsupported_file() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let video = temp_directory.path().join("video.mp4");
        let text = temp_directory.path().join("notes.txt");
        std::fs::write(&video, b"video")?;
        std::fs::write(&text, b"notes")?;

        assert_eq!(collector(video.clone(), false).gather_video_files()?, vec![video]);
        let error = collector(text, false)
            .gather_video_files()
            .expect_err("unsupported single file should fail");
        assert!(error.to_string().contains("not a supported video file"));
        Ok(())
    }

    #[test]
    fn non_recursive_scan_excludes_nested_videos_and_non_video_files() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let nested = temp_directory.path().join("nested");
        std::fs::create_dir(&nested)?;
        let top_level = temp_directory.path().join("top.mp4");
        std::fs::write(&top_level, b"top")?;
        std::fs::write(temp_directory.path().join("notes.txt"), b"notes")?;
        std::fs::write(nested.join("nested.mkv"), b"nested")?;

        let files = collector(temp_directory.path().to_path_buf(), false).gather_video_files()?;

        assert_eq!(files, vec![top_level]);
        Ok(())
    }

    #[test]
    fn recursive_scan_includes_nested_videos_and_skips_hidden_directories() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let scan_root = temp_directory.path().join("videos");
        let nested = scan_root.join("nested");
        let hidden = scan_root.join(".hidden");
        std::fs::create_dir_all(&nested)?;
        std::fs::create_dir(&hidden)?;
        let top_level = scan_root.join("top.mp4");
        let nested_video = nested.join("nested.mkv");
        std::fs::write(&top_level, b"top")?;
        std::fs::write(&nested_video, b"nested")?;
        std::fs::write(hidden.join("ignored.mp4"), b"hidden")?;

        let files = collector(scan_root, true).gather_video_files()?;

        assert_eq!(files.len(), 2);
        assert!(files.contains(&top_level));
        assert!(files.contains(&nested_video));
        Ok(())
    }

    #[test]
    fn missing_path_returns_contextual_error() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        let missing = temp_directory.path().join("missing");

        let error = collector(missing, false)
            .gather_video_files()
            .expect_err("missing path should fail");

        assert!(error.to_string().contains("does not exist"));
    }

    #[test]
    fn empty_directory_run_completes_without_probing() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        collector(temp_directory.path().to_path_buf(), false).run()
    }
}

#[cfg(test)]
mod test_stats_collector_new {
    use clap::Parser;

    use super::*;

    #[test]
    fn resolves_path_and_copies_flags() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let path = temp_directory
            .path()
            .to_str()
            .expect("temporary path should be UTF-8")
            .to_string();
        let args = VideoStatsArgs::try_parse_from(["vstats", &path, "--recurse", "--verbose"])
            .expect("arguments should parse");

        let collector = StatsCollector::new(&args)?;

        assert_eq!(collector.root, dunce::canonicalize(temp_directory.path())?);
        assert!(collector.recurse);
        assert!(collector.verbose);
        Ok(())
    }
}
