use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{anyhow, Result};
use clap::Parser;
use colored::Colorize;
use itertools::Itertools;
use regex::Regex;
use serde::Deserialize;
use walkdir::WalkDir;

static RE_BRACKETS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\[({\]})]+").expect("Failed to create regex pattern for brackets"));

static RE_WHITESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

static RE_DOTS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.{2,}").unwrap());

// ["WEBDL", "."],
// [".HEVC", ""],
static REPLACE: [(&str, &str); 10] = [
    (" ", "."),
    (" - ", " "),
    (",.", "."),
    ("-", "."),
    ("-=-", "."),
    (".&.", "."),
    (".-.", "."),
    (".rq", ""),
    ("=", "."),
    ("_", "."),
];

#[derive(Parser, Debug)]
#[command(author, version, name = "dots", about = "Rename files to use dots")]
struct Args {
    /// Optional input directory or file
    path: Option<String>,

    /// Overwrite existing files
    #[arg(short, long)]
    force: bool,

    /// Only print changes without renaming files
    #[arg(short, long)]
    print: bool,

    /// Recursive directory iteration
    #[arg(short, long)]
    recursive: bool,

    /// Substitute patterns with replacements in filenames
    #[arg(short, long, num_args = 2, action = clap::ArgAction::Append)]
    substitute: Vec<String>,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    replace: Vec<(String, String)>,
    #[serde(default)]
    dryrun: bool,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    recursive: bool,
    #[serde(default)]
    verbose: bool,
}

#[derive(Debug, Default)]
struct Config {
    replace: Vec<(String, String)>,
    dryrun: bool,
    overwrite: bool,
    recursive: bool,
    verbose: bool,
}

#[derive(Debug, Default)]
struct Dots {
    root: PathBuf,
    config: Config,
}

impl Args {
    /// Collect substitutes to replace pairs.
    fn parse_substitutes(&self) -> Vec<(String, String)> {
        self.substitute
            .chunks(2)
            .filter_map(|chunk| {
                if chunk.len() == 2 {
                    Some((chunk[0].clone(), chunk[1].clone()))
                } else {
                    None
                }
            })
            .collect()
    }
}

impl Dots {
    pub fn new(args: Args) -> Result<Dots> {
        let root = cli_tools::resolve_input_path(args.path.as_deref())?;
        let config = Config::from_args(args);
        Ok(Dots { root, config })
    }

    pub fn process_files(&self) -> Result<()> {
        let files_to_rename = self.gather_files_to_rename();

        if files_to_rename.is_empty() {
            println!("No files to rename");
            return Ok(());
        }

        let num_renamed = self.rename_files(files_to_rename);
        let message = format!("{num_renamed} file{}", if num_renamed == 1 { "" } else { "s" });

        if self.config.dryrun {
            println!("Dryrun: would have renamed {message}");
        } else {
            println!("{}", format!("Renamed {message}").green());
        }

        Ok(())
    }

    fn gather_files_to_rename(&self) -> Vec<(PathBuf, PathBuf)> {
        if self.root.is_file() {
            if self.config.verbose {
                println!("{}", format!("Formatting file {}", self.root.display()).bold());
            }
            return self
                .format_filename(&self.root)
                .ok()
                .filter(|new_path| &self.root != new_path)
                .map(|new_path| vec![(self.root.to_path_buf(), new_path)])
                .unwrap_or_default();
        }

        let max_depth = if self.config.recursive { 100 } else { 1 };

        if self.config.verbose {
            println!("{}", format!("Formatting files under {}", self.root.display()).bold());
        }

        // Collect and sort all files that need renaming
        WalkDir::new(&self.root)
            .max_depth(max_depth)
            .into_iter()
            .filter_entry(|e| !cli_tools::is_hidden(e))
            .filter_map(|e| e.ok())
            .filter_map(|entry| {
                let path = entry.path();
                self.format_filename(path)
                    .ok()
                    .filter(|new_path| path != new_path)
                    .map(|new_path| (path.to_path_buf(), new_path))
            })
            .sorted_by_key(|(path, _)| path.to_string_lossy().to_lowercase())
            .collect()
    }

    fn rename_files(&self, files_to_rename: Vec<(PathBuf, PathBuf)>) -> usize {
        let mut num_renamed: usize = 0;
        for (path, new_path) in files_to_rename {
            let old_str = cli_tools::get_relative_path_or_filename(&path, &self.root);
            let new_str = cli_tools::get_relative_path_or_filename(&new_path, &self.root);
            if self.config.dryrun {
                println!("{}", "Dryrun:".bold());
                cli_tools::show_diff(&old_str, &new_str);
                num_renamed += 1;
            } else if new_path.exists() && !self.config.overwrite {
                println!(
                    "{}",
                    format!("Skipping rename to already existing file: {new_str}").yellow()
                )
            } else {
                match fs::rename(&path, &new_path) {
                    Ok(_) => {
                        println!("{}", "Rename:".bold().magenta());
                        cli_tools::show_diff(&old_str, &new_str);
                        num_renamed += 1;
                    }
                    Err(e) => {
                        eprintln!("{}", format!("Error renaming: {old_str}\n{e}").red());
                    }
                }
            }
        }
        num_renamed
    }

    fn format_filename(&self, path: &Path) -> Result<PathBuf> {
        if !path.is_file() {
            anyhow::bail!("Path is not a file")
        }

        if let Ok((file_name, file_extension)) = cli_tools::get_normalized_file_name_and_extension(path) {
            let new_file = format!("{}.{}", self.format_name(&file_name), file_extension.to_lowercase());
            let new_path = path.with_file_name(new_file);
            Ok(new_path)
        } else {
            Err(anyhow!("Failed to get filename"))
        }
    }

    fn format_name(&self, file_name: &str) -> String {
        let mut new_name = file_name.to_string();

        for (pattern, replacement) in REPLACE {
            new_name = new_name.replace(pattern, replacement);
        }

        for (pattern, replacement) in self.config.replace.iter() {
            new_name = new_name.replace(pattern, replacement);
        }

        new_name = RE_WHITESPACE.replace_all(&new_name, "").to_string();
        new_name = RE_BRACKETS.replace_all(&new_name, "").to_string();
        new_name = RE_DOTS.replace_all(&new_name, ".").to_string();

        // Temporarily convert dots back to whitespace so titlecase works
        new_name = new_name.replace(".", " ");
        new_name = titlecase::titlecase(new_name.trim());
        new_name.replace(" ", ".")
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    pub fn from_args(args: Args) -> Self {
        let user_config = UserConfig::get_user_config();
        let mut replace = args.parse_substitutes();
        replace.extend(user_config.replace);
        Config {
            replace,
            recursive: args.recursive || user_config.recursive,
            overwrite: args.force || user_config.overwrite,
            dryrun: args.print || user_config.dryrun,
            verbose: args.verbose || user_config.verbose,
        }
    }
}

impl UserConfig {
    /// Try to read user config from file if it exists.
    /// Otherwise, fall back to default config.
    fn get_user_config() -> UserConfig {
        cli_tools::config::CONFIG_PATH
            .as_deref()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|config_string| toml::from_str(&config_string).ok())
            .unwrap_or_default()
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    Dots::new(args)?.process_files()
}

#[cfg(test)]
mod dots_tests {
    use super::*;

    static DOTS: LazyLock<Dots> = LazyLock::new(Dots::default);

    #[test]
    fn test_format_basic() {
        assert_eq!(DOTS.format_name("Some file"), "Some.File");
        assert_eq!(DOTS.format_name("some file"), "Some.File");
        assert_eq!(DOTS.format_name("word"), "Word");
    }

    #[test]
    fn test_format_name_no_brackets() {
        assert_eq!(DOTS.format_name("John Doe - Document"), "John.Doe.Document");
    }

    #[test]
    fn test_format_name_with_brackets() {
        assert_eq!(
            DOTS.format_name("Project Report - [Final Version]"),
            "Project.Report.Final.Version"
        );
    }

    #[test]
    fn test_format_name_with_parentheses() {
        assert_eq!(
            DOTS.format_name("Meeting Notes (2023) - Draft"),
            "Meeting.Notes.2023.Draft"
        );
    }

    #[test]
    fn test_format_name_with_newlines() {
        assert_eq!(
            DOTS.format_name("Meeting \tNotes \n(2023) - Draft\r\n"),
            "Meeting.Notes.2023.Draft"
        );
    }

    #[test]
    fn test_format_name_empty_string() {
        assert_eq!(DOTS.format_name(""), "");
    }

    #[test]
    fn test_format_name_no_changes() {
        assert_eq!(DOTS.format_name("SingleWord"), "SingleWord");
    }
}
