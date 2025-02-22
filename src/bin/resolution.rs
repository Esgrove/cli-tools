use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock};

use anyhow::{Error, anyhow};
use clap::Parser;
use regex::Regex;
use serde::Deserialize;
use tokio::process::Command;
use tokio::sync::{Semaphore, SemaphorePermit};
use walkdir::WalkDir;

const FILE_EXTENSIONS: [&str; 2] = ["mp4", "mkv"];

static RE_RESOLUTIONS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(480p|540p|544p|720p|1080p|1440p|2160p)")
        .expect("Failed to create regex pattern for valid resolutions")
});

#[derive(Parser, Debug)]
#[command(author, version, name = "resolution", about = "Add video resolution to filenames")]
struct Args {
    /// Optional input directory or file path
    path: Option<String>,

    /// Overwrite existing files
    #[arg(short, long)]
    force: bool,

    /// Only print file names without renaming
    #[arg(short, long)]
    print: bool,

    /// Recursive directory iteration
    #[arg(short, long)]
    recursive: bool,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Debug, Ord, PartialOrd, Eq, PartialEq, Deserialize)]
struct Resolution {
    width: u32,
    height: u32,
}

#[derive(Debug, Ord, PartialOrd, Eq, PartialEq)]
struct FFProbeResult {
    file: PathBuf,
    resolution: Resolution,
}

impl FFProbeResult {
    fn rename(&self, overwrite: bool) -> anyhow::Result<()> {
        if self.resolution.label().is_some() {
            let new_path = self.path_with_label()?;
            if !new_path.exists() || overwrite {
                std::fs::rename(&self.file, new_path)?;
                return Ok(());
            }
            return Err(anyhow!("File already exists: {}", cli_tools::path_to_string(&new_path)));
        }
        Ok(())
    }

    fn path_with_label(&self) -> anyhow::Result<PathBuf> {
        if let Some(label) = self.resolution.label() {
            let (name, extension) = cli_tools::get_normalized_file_name_and_extension(&self.file)?;
            let new_file_name = format!("{name}.{label}.{extension}");
            let new_path = self.file.with_file_name(&new_file_name);
            Ok(new_path)
        } else {
            Ok(self.file.clone())
        }
    }
}

impl Resolution {
    fn label(&self) -> Option<String> {
        match self.height {
            480 | 540 | 544 | 720 | 1080 | 1440 | 2160 => Some(format!("{}p", self.height)),
            1920 if self.width == 1080 => Some("1080p".to_string()),
            _ => None,
        }
    }
}

impl fmt::Display for FFProbeResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.path_with_label().as_ref().map_or(Err(fmt::Error), |path| {
            let (_, new_path) = cli_tools::color_diff(
                &cli_tools::path_to_string(&self.file),
                &cli_tools::path_to_string(path),
                false,
            );
            write!(
                f,
                "{:>4}x{:<4} {:>5} {}",
                self.resolution.width,
                self.resolution.height,
                self.resolution.label().as_ref().map_or("None", |label| label),
                new_path
            )
        })
    }
}

async fn run_ffprobe(file: PathBuf) -> anyhow::Result<FFProbeResult> {
    let path = cli_tools::path_to_string(&file);
    let command = format!(
        "ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of json \"{path}\" | jq .streams[0]"
    );
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdout(Stdio::piped())
        .output()
        .await;

    match output {
        Ok(output) => {
            if output.status.success() {
                let resolution: Resolution = serde_json::from_slice(&output.stdout)?;
                Ok(FFProbeResult { file, resolution })
            } else {
                Err(anyhow!("{path}: {}", std::str::from_utf8(&output.stderr)?))
            }
        }
        _ => Err(anyhow!("Command failed for {path}")),
    }
}

async fn gather_files_without_resolution_label(path: &Path, recursive: bool) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    if recursive {
        for entry in WalkDir::new(path)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();
            if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                if FILE_EXTENSIONS.contains(&ext) {
                    files.push(path.to_path_buf());
                }
            }
        }
    } else {
        let mut dir_entries = tokio::fs::read_dir(&path).await?;
        while let Some(entry) = dir_entries.next_entry().await? {
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                    if FILE_EXTENSIONS.contains(&ext) {
                        files.push(path);
                    }
                }
            }
        }
    }

    // Drop files that already contain a resolution label
    files.retain(|path| {
        path.file_name()
            .and_then(|f| f.to_str())
            .is_some_and(|filename| !RE_RESOLUTIONS.is_match(filename))
    });

    Ok(files)
}

async fn get_resolutions(files: Vec<PathBuf>) -> anyhow::Result<Vec<Result<FFProbeResult, Error>>> {
    let semaphore = create_semaphore_for_num_physical_cpus();
    let tasks: Vec<_> = files
        .into_iter()
        .map(|path| {
            let sem = Arc::clone(&semaphore);
            tokio::spawn(async move {
                let permit: SemaphorePermit = sem.acquire().await.expect("Failed to acquire semaphore");
                let result = run_ffprobe(path).await;
                drop(permit);
                result
            })
        })
        .collect();

    let results = futures::future::join_all(tasks)
        .await
        .into_iter()
        .map(|res| res.expect("Download future failed"))
        .collect();

    Ok(results)
}

#[inline]
/// Create a Semaphore with half the number of logical CPU cores available.
fn create_semaphore_for_num_physical_cpus() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(num_cpus::get_physical()))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let absolute_input_path = cli_tools::resolve_input_path(args.path.as_deref())?;

    let files = gather_files_without_resolution_label(&absolute_input_path, args.recursive).await?;

    if files.is_empty() {
        if args.verbose {
            println!("No video files to process");
        }
        return Ok(());
    }

    if args.verbose {
        println!("Processing {} files...", files.len());
    }

    // Keep successfully processed files, print errors for ffprobe command
    let mut files_to_process: Vec<FFProbeResult> = get_resolutions(files)
        .await?
        .into_iter()
        .filter_map(|res| match res {
            Ok(val) => Some(val),
            Err(err) => {
                eprintln!("Error: {err}");
                None
            }
        })
        .collect();

    files_to_process.sort_unstable_by(|a, b| a.resolution.cmp(&b.resolution).then_with(|| a.file.cmp(&b.file)));

    for result in files_to_process {
        println!("{result}");
        if !args.print {
            if let Err(error) = result.rename(args.force) {
                println!("{error}");
            }
        }
    }

    Ok(())
}
