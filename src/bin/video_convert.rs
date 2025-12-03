use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Parser;
use clap_complete::Shell;
use serde::Deserialize;
use walkdir::WalkDir;

use cli_tools::print_error;

/// Default video extensions
const DEFAULT_EXTENSIONS: &[&str] = &["mp4", "mkv"];

/// Other video extensions excluding mp4
const OTHER_EXTENSIONS: &[&str] = &["mkv", "wmv", "flv", "m4v", "ts", "mpg", "avi", "mov", "webm"];

/// All video extensions
const ALL_EXTENSIONS: &[&str] = &["mp4", "mkv", "wmv", "flv", "m4v", "ts", "mpg", "avi", "mov", "webm"];

#[derive(Parser)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Convert video files to HEVC (H.265) format using ffmpeg and NVENC")]
struct Args {
    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Convert all known video file types
    #[arg(short, long)]
    all: bool,

    /// Skip files with bitrate lower than LIMIT kbps
    #[arg(short, long, name = "LIMIT", default_value_t = 8000)]
    bitrate: u64,

    /// Delete input files immediately instead of moving to trash
    #[arg(short, long)]
    delete: bool,

    /// Print commands without running them
    #[arg(short, long)]
    print: bool,

    /// Overwrite existing output files
    #[arg(short, long)]
    force: bool,

    /// Include files that match the given pattern
    #[arg(short = 'i', long, num_args = 1, action = clap::ArgAction::Append, name = "INCLUDE")]
    include: Vec<String>,

    /// Exclude files that match the given pattern
    #[arg(short = 'e', long, num_args = 1, action = clap::ArgAction::Append, name = "EXCLUDE")]
    exclude: Vec<String>,

    /// Override file extensions to convert
    #[arg(short = 't', long, num_args = 1, action = clap::ArgAction::Append, name = "EXTENSION")]
    extension: Vec<String>,

    /// Number of files to convert
    #[arg(short, long, default_value_t = 1)]
    number: usize,

    /// Convert file types other than mp4
    #[arg(short, long)]
    other: bool,

    /// Recurse into subdirectories
    #[arg(short, long)]
    recurse: bool,

    /// Generate shell completion
    #[arg(short = 'l', long, name = "SHELL")]
    completion: Option<Shell>,

    /// Display commands being executed
    #[arg(short, long)]
    verbose: bool,
}

/// Config from a config file
#[derive(Debug, Default, Deserialize)]
struct VideoConvertConfig {
    #[serde(default)]
    convert_all_types: bool,
    #[serde(default)]
    bitrate: Option<u64>,
    #[serde(default)]
    delete: bool,
    #[serde(default)]
    exclude: Vec<String>,
    /// Custom list of file extensions to process (overrides all/other flags)
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    number: Option<usize>,
    #[serde(default)]
    convert_other_types: bool,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    recursive: bool,
    #[serde(default)]
    verbose: bool,
}

/// Wrapper needed for parsing the config file section.
#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    video_convert: VideoConvertConfig,
}

/// Final config created from CLI arguments and user config file.
#[derive(Debug, Default)]
#[allow(unused)]
struct Config {
    bitrate: u64,
    convert_all: bool,
    convert_other: bool,
    delete: bool,
    dryrun: bool,
    exclude: Vec<String>,
    include: Vec<String>,
    /// File extensions to convert (lowercase, without leading dot)
    extensions: Vec<String>,
    number: usize,
    overwrite: bool,
    path: PathBuf,
    recursive: bool,
    verbose: bool,
}

#[derive(Debug)]
struct VideoConvert {
    config: Config,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct VideoFile {
    path: PathBuf,
    name: String,
    extension: String,
}

impl VideoConvertConfig {
    /// Try to read user config from the file if it exists.
    /// Otherwise, fall back to default config.
    fn get_user_config() -> Self {
        cli_tools::config::CONFIG_PATH
            .as_deref()
            .and_then(|path| {
                fs::read_to_string(path)
                    .map_err(|e| {
                        print_error!("Error reading config file {}: {e}", path.display());
                    })
                    .ok()
            })
            .and_then(|config_string| {
                toml::from_str::<UserConfig>(&config_string)
                    .map_err(|e| {
                        print_error!("Error reading config file: {e}");
                    })
                    .ok()
            })
            .map(|config| config.video_convert)
            .unwrap_or_default()
    }
}

impl VideoFile {
    pub fn new(path: &Path) -> Self {
        let path = path.to_owned();
        let name = cli_tools::path_to_filename_string(&path);
        let extension = cli_tools::path_to_file_extension_string(&path);

        Self { path, name, extension }
    }

    pub fn from_dir_entry(entry: walkdir::DirEntry) -> Self {
        let path = entry.into_path();
        let name = cli_tools::path_to_filename_string(&path);
        let extension = cli_tools::path_to_file_extension_string(&path);

        Self { path, name, extension }
    }
}

impl std::fmt::Display for VideoFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.path.display())
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    pub fn try_from_args(args: Args, user_config: VideoConvertConfig) -> Result<Self> {
        let mut include = args.include;
        include.extend(user_config.include);

        let mut exclude = args.exclude;
        exclude.extend(user_config.exclude);

        let path = cli_tools::resolve_input_path(args.path.as_deref())?;

        let convert_all = args.all || user_config.convert_all_types;
        let convert_other = args.other || user_config.convert_other_types;

        let extensions = if !args.extension.is_empty() {
            args.extension.iter().map(|s| s.to_lowercase()).collect()
        } else if !user_config.extensions.is_empty() {
            user_config.extensions.iter().map(|s| s.to_lowercase()).collect()
        } else if args.all || user_config.convert_all_types {
            ALL_EXTENSIONS.iter().map(|s| (*s).to_string()).collect()
        } else if args.other || user_config.convert_other_types {
            OTHER_EXTENSIONS.iter().map(|s| (*s).to_string()).collect()
        } else {
            DEFAULT_EXTENSIONS.iter().map(|s| (*s).to_string()).collect()
        };

        Ok(Self {
            bitrate: user_config.bitrate.unwrap_or(args.bitrate),
            convert_all,
            convert_other,
            delete: args.delete || user_config.delete,
            dryrun: args.print,
            exclude,
            extensions,
            include,
            number: user_config.number.unwrap_or(args.number),
            overwrite: args.force || user_config.overwrite,
            path,
            recursive: args.recurse || user_config.recursive,
            verbose: args.verbose || user_config.verbose,
        })
    }
}

impl VideoConvert {
    pub fn new(args: Args) -> Result<Self> {
        let user_config = VideoConvertConfig::get_user_config();
        let config = Config::try_from_args(args, user_config)?;

        Ok(Self { config })
    }

    pub fn run(&self) -> Result<()> {
        let files = self.gather_files_to_convert()?;
        if files.is_empty() {
            println!("No files to convert found");
            return Ok(());
        }

        if self.config.verbose {
            println!("Found {} file(s) to process", files.len());
        }

        self.process_files(files)?;

        Ok(())
    }

    /// Gather video files based on the config settings.
    /// Returns a list of files to convert.
    fn gather_files_to_convert(&self) -> Result<Vec<VideoFile>> {
        let path = &self.config.path;

        if path.is_file() {
            let file = VideoFile::new(path);
            return if self.should_include_file(&file) {
                Ok(vec![file])
            } else {
                Ok(vec![])
            };
        }

        // Path must be a directory
        if !path.is_dir() {
            anyhow::bail!("Input path '{}' does not exist or is not accessible", path.display());
        }

        let max_depth = if self.config.recursive { usize::MAX } else { 1 };

        let walker = WalkDir::new(path)
            .max_depth(max_depth)
            .into_iter()
            .filter_map(std::result::Result::ok);

        let mut files: Vec<VideoFile> = walker
            .filter(|entry| entry.file_type().is_file())
            .map(VideoFile::from_dir_entry)
            .filter(|file| self.should_include_file(file))
            .collect();

        files.sort();
        Ok(files)
    }

    fn process_files(&self, files: Vec<VideoFile>) -> Result<()> {
        for file in files {
            println!("  {}", cli_tools::path_to_string_relative(&file.path));
        }
        Ok(())
    }

    /// Check if a file should be converted based on extension and include/exclude patterns.
    fn should_include_file(&self, file: &VideoFile) -> bool {
        // Skip files with "x265" in the name (already converted)
        if file.name.contains(".x265.") {
            return false;
        }

        if !self.config.extensions.iter().any(|ext| ext == &file.extension) {
            return false;
        }

        // Check include patterns (if specified, file must match at least one)
        if !self.config.include.is_empty() {
            let matches_include = self.config.include.iter().any(|pattern| file.name.contains(pattern));
            if !matches_include {
                return false;
            }
        }

        // Check exclude patterns (file must not match any)
        if self.config.exclude.iter().any(|pattern| file.name.contains(pattern)) {
            return false;
        }

        true
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    VideoConvert::new(args)?.run()
}
