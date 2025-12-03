use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use clap_complete::Shell;
use serde::Deserialize;

use cli_tools::print_error;

#[derive(Parser)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Convert video files to HEVC (H.265) format using ffmpeg and NVENC")]
struct Args {
    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Convert all known video file types
    #[arg(short, long)]
    all: bool,

    /// Skip files with bitrate lower than NUM kbps (default: 8000)
    #[arg(short, long, default_value = "8000")]
    bitrate: u32,

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
    #[arg(short = 'i', long, num_args = 1, action = clap::ArgAction::Append, name = "INCLUDE_PATTERN")]
    include: Vec<String>,

    /// Exclude files that match the given pattern
    #[arg(short = 'e', long, num_args = 1, action = clap::ArgAction::Append, name = "EXCLUDE_PATTERN")]
    exclude: Vec<String>,

    /// Number of files to convert (default: 1)
    #[arg(short, long, default_value = "1")]
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
    bitrate: Option<u32>,
    #[serde(default)]
    delete: bool,
    #[serde(default)]
    exclude: Vec<String>,
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
struct Config {
    bitrate: u32,
    convert_all: bool,
    convert_other: bool,
    delete: bool,
    dryrun: bool,
    exclude: Vec<String>,
    include: Vec<String>,
    number: usize,
    overwrite: bool,
    path: PathBuf,
    recursive: bool,
    verbose: bool,
}

#[derive(Debug, Default)]
struct VideoConvert {
    config: Config,
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

impl Config {
    /// Create config from given command line args and user config file.
    pub fn try_from_args(args: Args) -> Result<Self> {
        let user_config = VideoConvertConfig::get_user_config();

        let mut include = args.include;
        include.extend(user_config.include);

        let mut exclude = args.exclude;
        exclude.extend(user_config.exclude);

        let path = cli_tools::resolve_input_path(args.path.as_deref())?;

        Ok(Self {
            convert_all: args.all || user_config.convert_all_types,
            bitrate: user_config.bitrate.unwrap_or(args.bitrate),
            delete: args.delete || user_config.delete,
            dryrun: args.print,
            exclude,
            include,
            number: user_config.number.unwrap_or(args.number),
            convert_other: args.other || user_config.convert_other_types,
            overwrite: args.force || user_config.overwrite,
            path,
            recursive: args.recurse || user_config.recursive,
            verbose: args.verbose || user_config.verbose,
        })
    }
}

impl VideoConvert {
    pub fn new(args: Args) -> Result<Self> {
        let config = Config::try_from_args(args)?;
        Ok(Self { config })
    }

    pub fn run(&self) -> Result<()> {
        Ok(())
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    VideoConvert::new(args)?.run()
}
