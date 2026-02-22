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
            print_yellow!("No video files found in: {}", self.root.display());
            return Ok(());
        }

        println!(
            "{}",
            format!("Found {} video file(s)", video_files.len()).green().bold()
        );

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
            println!("{}", format!("{error_count} file(s) could not be probed").red());
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
