use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::{fmt, fs};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use colored::Colorize;
use itertools::Itertools;
use regex::Regex;
use serde::Deserialize;
use unicode_segmentation::UnicodeSegmentation;
use walkdir::WalkDir;

static RE_BRACKETS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\[({\]})]+").expect("Failed to create regex pattern for brackets"));

static RE_WHITESPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+").expect("Failed to compile whitespace regex"));

static RE_DOTS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.{2,}").expect("Failed to compile dots regex"));

static RE_EXCLAMATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!+").expect("Failed to compile exclamation regex"));

static RE_DOTCOM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(\.com|\.net)\b").expect("Failed to compile .com regex"));

static RE_IDENTIFIER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9]{8,16}").expect("Failed to compile id regex"));

static REPLACE: [(&str, &str); 26] = [
    (" ", "."),
    (" - ", " "),
    (", ", " "),
    ("_", "."),
    ("-", "."),
    ("–", "."),
    ("*", "."),
    ("~", "."),
    ("¡", "."),
    ("#", "."),
    ("$", "."),
    (";", "."),
    ("@", "."),
    ("=", "."),
    (",.", "."),
    (",", "."),
    ("-=-", "."),
    (".&.", "."),
    (".-.", "."),
    (".rq", ""),
    ("www.", ""),
    ("^", ""),
    ("｜", ""),
    ("`", "'"),
    ("’", "'"),
    ("\"", "'"),
];

#[derive(Debug, Parser)]
#[command(author, version, name = "dots", about = "Rename files to use dots")]
struct Args {
    /// Optional input directory or file
    path: Option<String>,

    /// Convert casing
    #[arg(short, long)]
    case: bool,

    /// Enable debug prints
    #[arg(short, long)]
    debug: bool,

    /// Overwrite existing files
    #[arg(short, long)]
    force: bool,

    /// Only print changes without renaming files
    #[arg(short, long)]
    print: bool,

    /// Recursive directory iteration
    #[arg(short, long)]
    recursive: bool,

    /// Append prefix to the start
    #[arg(long)]
    prefix: Option<String>,

    /// Append suffix to the end
    #[arg(long)]
    suffix: Option<String>,

    /// Substitute pattern with replacement in filenames
    #[arg(short, long, num_args = 2, action = clap::ArgAction::Append)]
    substitute: Vec<String>,

    /// Substitute regex pattern with replacement in filenames
    #[arg(long, num_args = 2, action = clap::ArgAction::Append)]
    regex: Vec<String>,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

/// Config from config file
#[derive(Debug, Default, Deserialize)]
struct DotsConfig {
    #[serde(default)]
    replace: Vec<(String, String)>,
    #[serde(default)]
    regex_replace: Vec<(String, String)>,
    #[serde(default)]
    move_to_start: Vec<String>,
    #[serde(default)]
    move_to_end: Vec<String>,
    #[serde(default)]
    debug: bool,
    #[serde(default)]
    dryrun: bool,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    recursive: bool,
    #[serde(default)]
    verbose: bool,
}

/// Wrapper needed to parse config section.
#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    dots: DotsConfig,
}

/// Final config created from CLI arguments and user config file.
#[derive(Debug, Default)]
struct Config {
    replace: Vec<(String, String)>,
    regex_replace: Vec<(Regex, String)>,
    move_to_start: Vec<String>,
    move_to_end: Vec<String>,
    prefix: Option<String>,
    suffix: Option<String>,
    convert_case: bool,
    debug: bool,
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

fn main() -> Result<()> {
    let args = Args::parse();
    Dots::new(args)?.process_files();
    Ok(())
}

impl Dots {
    /// Init new instance with CLI args.
    pub fn new(args: Args) -> Result<Self> {
        let root = cli_tools::resolve_input_path(args.path.as_deref())?;
        let config = Config::from_args(args)?;
        Ok(Self { root, config })
    }

    /// Run renaming.
    pub fn process_files(&self) {
        if self.config.debug {
            println!("{self}");
        }

        let files_to_rename = self.gather_files_to_rename();

        if files_to_rename.is_empty() {
            println!("No files to rename");
            return;
        }

        let num_renamed = self.rename_files(files_to_rename);
        let message = format!("{num_renamed} file{}", if num_renamed == 1 { "" } else { "s" });

        if self.config.dryrun {
            println!("Dryrun: would have renamed {message}");
        } else {
            println!("{}", format!("Renamed {message}").green());
        }
    }

    /// Get all files that need to be renamed.
    fn gather_files_to_rename(&self) -> Vec<(PathBuf, PathBuf)> {
        if self.root.is_file() {
            if self.config.verbose {
                println!("{}", format!("Formatting file {}", self.root.display()).bold());
            }
            return self
                .format_filename(&self.root)
                .ok()
                .filter(|new_path| &self.root != new_path)
                .map(|new_path| vec![(self.root.clone(), new_path)])
                .unwrap_or_default();
        }

        if self.config.verbose {
            println!("{}", format!("Formatting files under {}", self.root.display()).bold());
        }

        let max_depth = if self.config.recursive { 100 } else { 1 };

        // Collect and sort all files that need renaming
        WalkDir::new(&self.root)
            .max_depth(max_depth)
            .into_iter()
            // ignore hidden files (name starting with ".")
            .filter_entry(|e| !cli_tools::is_hidden(e))
            .filter_map(std::result::Result::ok)
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

    /// Rename all files or just print changes if dryrun is enabled.
    fn rename_files(&self, files_to_rename: Vec<(PathBuf, PathBuf)>) -> usize {
        let mut num_renamed: usize = 0;
        let max_items = files_to_rename.len();
        let max_chars = files_to_rename.len().to_string().chars().count();
        for (index, (path, new_path)) in files_to_rename.into_iter().enumerate() {
            let old_str = cli_tools::get_relative_path_or_filename(&path, &self.root);
            let new_str = cli_tools::get_relative_path_or_filename(&new_path, &self.root);
            let number = format!("{:>max_chars$} / {max_items}", index + 1);

            if self.config.dryrun {
                println!("{}", format!("Dryrun {number}:").bold().cyan());
                cli_tools::show_diff(&old_str, &new_str);
                num_renamed += 1;
                continue;
            }

            let capitalization_change_only = if new_str.to_lowercase() == old_str.to_lowercase() {
                // File path contains only capitalisation changes:
                // Need to use a temp file to workaround case-insensitive file systems.
                true
            } else {
                false
            };
            if !capitalization_change_only && new_path.exists() && !self.config.overwrite {
                println!(
                    "{}",
                    format!("Skipping rename to already existing file: {new_str}").yellow()
                );
                continue;
            }

            println!("{}", format!("Rename {number}:").bold().magenta());
            cli_tools::show_diff(&old_str, &new_str);

            let rename_result = if capitalization_change_only {
                Self::rename_with_temp_file(&path, &new_path)
            } else {
                fs::rename(&path, &new_path)
            };

            match rename_result {
                Ok(()) => {
                    num_renamed += 1;
                }
                Err(e) => {
                    eprintln!("{}", format!("Error renaming: {old_str}\n{e}").red());
                }
            }
        }
        num_renamed
    }

    /// Get the full path with formatted filename and extension.
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

    /// Format the file name stem without the file extension
    fn format_name(&self, file_name: &str) -> String {
        // Apply static replacements
        let mut new_name = REPLACE
            .iter()
            .fold(file_name.to_string(), |acc, &(pattern, replacement)| {
                acc.replace(pattern, replacement)
            });

        // Apply extra replacements from args and user config
        new_name = self
            .config
            .replace
            .iter()
            .fold(new_name, |acc, (pattern, replacement)| {
                acc.replace(pattern, replacement)
            });

        // Apply regex replacements from args and user config
        if !self.config.regex_replace.is_empty() {
            for (regex, replacement) in &self.config.regex_replace {
                new_name = regex.replace_all(&new_name, replacement).to_string();
            }
        }

        new_name = RE_BRACKETS.replace_all(&new_name, ".").to_string();
        new_name = RE_DOTCOM.replace_all(&new_name, ".").to_string();
        new_name = RE_EXCLAMATION.replace_all(&new_name, ".").to_string();
        new_name = RE_WHITESPACE.replace_all(&new_name, ".").to_string();
        new_name = RE_DOTS.replace_all(&new_name, ".").to_string();

        Self::remove_special_characters(&mut new_name);
        Self::remove_random_identifiers(&mut new_name);

        new_name = new_name.trim_start_matches('.').trim_end_matches('.').to_string();

        if self.config.convert_case {
            new_name = new_name.to_lowercase();
        }

        // Temporarily convert dots back to whitespace so titlecase works
        new_name = new_name.replace('.', " ");
        new_name = titlecase::titlecase(&new_name);
        new_name = new_name.replace(' ', ".");

        // Fix encoding capitalization
        new_name = new_name.replace("X265", "x265").replace("X264", "x264");

        if let Some(ref prefix) = self.config.prefix {
            if new_name.contains(prefix) {
                new_name = new_name.replace(prefix, "");
            }
            let lower_name = new_name.to_lowercase();
            let lower_prefix = prefix.to_lowercase();
            if lower_name.starts_with(&lower_prefix) {
                new_name = format!("{}{}", prefix, &new_name[prefix.len()..]);
            } else {
                new_name = format!("{prefix}.{new_name}");
            }
        }
        if let Some(ref suffix) = self.config.suffix {
            if new_name.contains(suffix) {
                new_name = new_name.replace(suffix, "");
            }
            let lower_name = new_name.to_lowercase();
            let lower_suffix = suffix.to_lowercase();
            if lower_name.ends_with(&lower_suffix) {
                new_name = format!("{}{}", &new_name[..new_name.len() - lower_suffix.len()], suffix);
            } else {
                // If it doesn't end with the suffix, append it
                new_name = format!("{new_name}.{suffix}");
            }
        }

        if !self.config.move_to_start.is_empty() {
            self.move_to_start(&mut new_name);
        }
        if !self.config.move_to_end.is_empty() {
            self.move_to_end(&mut new_name);
        }

        new_name = RE_DOTS.replace_all(&new_name, ".").to_string();
        new_name = new_name.trim_start_matches('.').trim_end_matches('.').to_string();
        new_name
    }

    fn move_to_start(&self, name: &mut String) {
        for sub in &self.config.move_to_start {
            if name.contains(sub) {
                *name = format!("{}.{}", sub, name.replace(sub, ""));
            }
        }
    }

    fn move_to_end(&self, name: &mut String) {
        for sub in &self.config.move_to_end {
            if name.contains(sub) {
                *name = format!("{}.{}", name.replace(sub, ""), sub);
            }
        }
    }

    /// Only retain alphanumeric characters and a few common filename characters
    fn remove_special_characters(name: &mut String) {
        let cleaned: String = name
            // Split the string into graphemes (for handling emojis and complex characters)
            .graphemes(true)
            .filter(|g| {
                g.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '\'' || c == '&')
            })
            .collect();

        *name = cleaned;
    }

    fn remove_random_identifiers(name: &mut String) {
        let result = RE_IDENTIFIER.replace_all(name, |caps: &regex::Captures| {
            let matched_str = &caps[0];
            if Self::has_at_least_six_digits(matched_str)
                && !matched_str.contains("720")
                && !matched_str.contains("1080")
                && !matched_str.contains("1920")
                && !matched_str.contains("2160")
                && !matched_str.contains("3840")
            {
                String::new()
            } else {
                matched_str.to_string()
            }
        });

        *name = result.trim().to_string();
    }

    fn has_at_least_six_digits(s: &str) -> bool {
        s.chars().filter(char::is_ascii_digit).count() >= 6
    }

    /// Rename a file with an intermediate temp file to work around case-insensitive file systems.
    fn rename_with_temp_file(path: &PathBuf, new_path: &PathBuf) -> std::io::Result<()> {
        let temp_file = cli_tools::append_extension_to_path(new_path.clone(), ".tmp");
        fs::rename(path, &temp_file)?;
        fs::rename(&temp_file, new_path)
    }
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

    /// Collect and compile regex substitutes to replace pairs.
    fn parse_regex_substitutes(&self) -> Result<Vec<(Regex, String)>> {
        self.regex
            .chunks(2)
            .filter_map(|chunk| {
                if chunk.len() == 2 {
                    let regex_result = Regex::new(&chunk[0]).with_context(|| format!("Invalid regex: '{}'", chunk[0]));

                    // If regex compilation fails, return None to propagate the error with `Result`
                    match regex_result {
                        Ok(regex) => Some(Ok((regex, chunk[1].clone()))),
                        Err(e) => Some(Err(e)),
                    }
                } else {
                    None
                }
            })
            .collect()
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    pub fn from_args(args: Args) -> Result<Self> {
        let user_config = DotsConfig::get_user_config();
        let mut replace = args.parse_substitutes();
        replace.extend(user_config.replace);
        let mut regex_replace = args.parse_regex_substitutes()?;
        let config_regex = Self::compile_regex_patterns(&user_config.regex_replace)?;
        regex_replace.extend(config_regex);
        Ok(Self {
            replace,
            regex_replace,
            move_to_start: user_config.move_to_start,
            move_to_end: user_config.move_to_end,
            prefix: args.prefix,
            suffix: args.suffix,
            convert_case: args.case,
            debug: args.debug || user_config.debug,
            dryrun: args.print || user_config.dryrun,
            overwrite: args.force || user_config.overwrite,
            recursive: args.recursive || user_config.recursive,
            verbose: args.verbose || user_config.verbose,
        })
    }

    fn compile_regex_patterns(regex_pairs: &[(String, String)]) -> Result<Vec<(Regex, String)>> {
        let mut compiled_pairs = Vec::new();

        for (pattern, replacement) in regex_pairs {
            let regex = Regex::new(pattern).with_context(|| format!("Invalid regex: '{pattern}'"))?;
            compiled_pairs.push((regex, replacement.clone()));
        }

        Ok(compiled_pairs)
    }
}

impl DotsConfig {
    /// Try to read user config from the file if it exists.
    /// Otherwise, fall back to default config.
    fn get_user_config() -> Self {
        cli_tools::config::CONFIG_PATH
            .as_deref()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|config_string| toml::from_str::<UserConfig>(&config_string).ok())
            .map(|config| config.dots)
            .unwrap_or_default()
    }
}

impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let replace = if self.replace.is_empty() {
            "replace:   []".to_string()
        } else {
            "replace:\n".to_string() + &*self.replace.iter().map(|pair| format!("    {pair:?}")).join("\n")
        };
        let regex_replace = if self.regex_replace.is_empty() {
            "regex_replace: []".to_string()
        } else {
            "regex_replace:\n".to_string() + &*self.regex_replace.iter().map(|pair| format!("    {pair:?}")).join("\n")
        };
        writeln!(f, "Config:")?;
        writeln!(f, "  debug:     {}", cli_tools::colorize_bool(self.debug))?;
        writeln!(f, "  dryrun:    {}", cli_tools::colorize_bool(self.dryrun))?;
        writeln!(f, "  overwrite: {}", cli_tools::colorize_bool(self.overwrite))?;
        writeln!(f, "  recursive: {}", cli_tools::colorize_bool(self.recursive))?;
        writeln!(f, "  verbose:   {}", cli_tools::colorize_bool(self.verbose))?;
        writeln!(f, "  prefix:    \"{}\"", self.prefix.as_ref().unwrap_or(&String::new()))?;
        writeln!(f, "  suffix:    \"{}\"", self.suffix.as_ref().unwrap_or(&String::new()))?;
        writeln!(f, "  {replace}")?;
        writeln!(f, "  {regex_replace}")
    }
}

impl fmt::Display for Dots {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Root: {}", self.root.display())?;
        write!(f, "{}", self.config)
    }
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
        assert_eq!(DOTS.format_name("__word__"), "Word");
    }

    #[test]
    fn test_format_name_with_newlines() {
        assert_eq!(
            DOTS.format_name("Meeting \tNotes \n(2023) - Draft\r\n"),
            "Meeting.Notes.2023.Draft"
        );
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
        assert_eq!(DOTS.format_name("Code {Snippet} (example)"), "Code.Snippet.Example");
    }

    #[test]
    fn test_format_name_with_parentheses() {
        assert_eq!(
            DOTS.format_name("Meeting Notes (2023) - Draft"),
            "Meeting.Notes.2023.Draft"
        );
    }

    #[test]
    fn test_format_name_with_extra_dots() {
        assert_eq!(DOTS.format_name("file..with...dots"), "File.With.Dots");
        assert_eq!(
            DOTS.format_name("...leading.and.trailing.dots..."),
            "Leading.and.Trailing.Dots"
        );
    }

    #[test]
    fn test_format_name_with_exclamations() {
        assert_eq!(DOTS.format_name("Exciting!Document!!"), "Exciting.Document");
        assert_eq!(DOTS.format_name("Hello!!!World!!"), "Hello.World");
    }

    #[test]
    fn test_format_name_with_dotcom() {
        assert_eq!(
            DOTS.format_name("visit.website.com.for.details"),
            "Visit.Website.for.Details"
        );
        assert_eq!(
            DOTS.format_name("Contact us at email@domain.net"),
            "Contact.Us.at.Email.Domain"
        );
        assert_eq!(DOTS.format_name("Contact.company.test"), "Contact.Company.Test");
    }

    #[test]
    fn test_format_name_with_combined_cases() {
        assert_eq!(
            DOTS.format_name("Amazing [Stuff]!! Visit my.site.com..now"),
            "Amazing.Stuff.Visit.My.Site.Now"
        );
    }

    #[test]
    fn test_format_name_with_weird_characters() {
        assert_eq!(
            DOTS.format_name("Weird-Text-~File-Name-@Example#"),
            "Weird.Text.File.Name.Example"
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

    #[test]
    fn test_move_to_start() {
        let mut dots = Dots::default();
        dots.config.move_to_start = vec!["Test".to_string()];
        assert_eq!(dots.format_name("This is a test string test"), "Test.This.Is.a.String");
        assert_eq!(
            dots.format_name("Test.This.Is.a.test.string.test"),
            "Test.This.Is.a.String"
        );
        assert_eq!(dots.format_name("test"), "Test");
        assert_eq!(dots.format_name("Test"), "Test");
    }

    #[test]
    fn test_move_to_end() {
        let mut dots = Dots::default();
        dots.config.move_to_end = vec!["Test".to_string()];
        assert_eq!(dots.format_name("This is a test string test"), "This.Is.a.String.Test");
        assert_eq!(
            dots.format_name("Test.This.Is.a.test.string.test"),
            "This.Is.a.String.Test"
        );
        assert_eq!(dots.format_name("test"), "Test");
        assert_eq!(dots.format_name("Test"), "Test");
    }

    #[test]
    fn test_remove_identifier() {
        let dots = Dots::default();
        assert_eq!(
            dots.format_name("This is a string test ^[640e54a564228]"),
            "This.Is.a.String.Test"
        );
        assert_eq!(
            dots.format_name("This.Is.a.test.string.65f09e4248e03..."),
            "This.Is.a.Test.String"
        );
        assert_eq!(dots.format_name("test Ph5d9473a841fe9"), "Test");
        assert_eq!(dots.format_name("Test-355989849"), "Test");
    }
}
