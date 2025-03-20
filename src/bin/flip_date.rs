use std::fs;
use std::path::PathBuf;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use chrono::Datelike;
use clap::Parser;
use colored::Colorize;
use regex::{Captures, Regex};
use walkdir::WalkDir;

static FILE_EXTENSIONS: [&str; 7] = ["m4a", "mp3", "txt", "rtf", "csv", "mp4", "mkv"];

// Static variables that are initialised at runtime the first time they are accessed.
static RE_DD_MM_YYYY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?P<day>[12]\d|3[01]|0?[1-9])\.(?P<month>1[0-2]|0?[1-9])\.(?P<year>\d{4})")
        .expect("Failed to create regex pattern for dd.mm.yyyy")
});

static RE_YYYY_MM_DD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?P<year>[12]\d{3})\.(?P<month>1[0-2]|0?[1-9])\.(?P<day>[12]\d|3[01]|0?[1-9])")
        .expect("Failed to create regex pattern for yyyy.mm.dd")
});

static RE_CORRECT_DATE_FORMAT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([12]\d{3})\.([12]\d|3[01]|0?[1-9])\.([12]\d|3[01]|0?[1-9])")
        .expect("Failed to create regex pattern for correct date")
});

static RE_FULL_DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:\b|\D)(?P<date>(?:[12]\d|3[01]|0?[1-9])\.(?:0?[1-9]|1[0-2])\.\d{4})(?:\b|\D)")
        .expect("Failed to create regex pattern for full date")
});

static RE_SHORT_DATE_DAY_FIRST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:\b|\D)(?P<date>(?:0?[1-9]|[12]\d|3[01])\.(?:0?[1-9]|1[0-2])\.\d{2})(?:\b|\D)")
        .expect("Failed to create regex pattern for short date")
});

static RE_SHORT_DATE_YEAR_FIRST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:\b|\D)(?P<date>(\d{2})\.(?:0?[1-9]|[12]\d|3[01])\.(?:0?[1-9]|1[0-2]))(?:\b|\D)")
        .expect("Failed to create regex pattern for short date")
});

static CURRENT_YEAR: LazyLock<i32> = LazyLock::new(|| chrono::Utc::now().year());

#[derive(Parser)]
#[command(
    author,
    version,
    name = "flip-date",
    about = "Flip dates in file and directory names to start with year"
)]
struct Args {
    /// Optional input directory or file
    path: Option<String>,

    /// Use directory rename mode
    #[arg(short, long)]
    dir: bool,

    /// Overwrite existing
    #[arg(short, long)]
    force: bool,

    /// Specify file extension(s)
    #[arg(short, long, num_args = 1, action = clap::ArgAction::Append, value_name = "EXTENSION", conflicts_with = "dir")]
    extensions: Option<Vec<String>>,

    /// Assume year is first
    #[arg(short, long)]
    year: bool,

    /// Only print changes without renaming
    #[arg(short, long)]
    print: bool,

    /// Use recursive path handling
    #[arg(short, long)]
    recursive: bool,

    /// Print verbose output
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Debug)]
struct RenameItem {
    path: PathBuf,
    filename: String,
    new_name: String,
}

struct Date {
    year: u32,
    month: u32,
    day: u32,
}

impl Date {
    /// Check that date is valid
    pub fn try_from(year: u32, month: u32, day: u32) -> Option<Self> {
        if year <= *CURRENT_YEAR as u32 && year >= 2000 && month > 0 && month <= 12 && day > 0 && day <= 31 {
            Some(Self { year, month, day })
        } else {
            None
        }
    }

    pub fn parse_from_short(year: &str, month: &str, day: &str) -> Option<Self> {
        if year.chars().count() != 2 {
            return None;
        }
        let year = format!("20{year}");
        let year = year.parse::<u32>().ok()?;
        let month = month.parse::<u32>().ok()?;
        let day = day.parse::<u32>().ok()?;
        Self::try_from(year, month, day)
    }

    pub fn dash_format(&self) -> String {
        format!("{}-{:02}-{:02}", self.year, self.month, self.day)
    }

    #[allow(unused)]
    pub fn dot_format(&self) -> String {
        self.to_string()
    }
}

impl std::fmt::Display for Date {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}.{:02}.{:02}", self.year, self.month, self.day)
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let path = cli_tools::resolve_input_path(args.path.as_deref())?;
    if args.dir {
        date_flip_directories(path, args.recursive, args.print)
    } else {
        let extensions = args.extensions.unwrap_or_default();
        let extensions_owned;
        let file_extensions: &[&str] = if extensions.is_empty() {
            &FILE_EXTENSIONS
        } else {
            extensions_owned = extensions.iter().map(String::as_str).collect::<Vec<_>>();
            &extensions_owned
        };

        date_flip_files(
            &path,
            file_extensions,
            args.recursive,
            args.print,
            args.year,
            args.force,
            args.verbose,
        )
    }
}

/// Flip date to start with year for all matching files from the given path.
fn date_flip_files(
    path: &PathBuf,
    file_extensions: &[&str],
    recursive: bool,
    dryrun: bool,
    starts_with_year: bool,
    overwrite_existing: bool,
    verbose: bool,
) -> Result<()> {
    let (files, root) = files_to_rename(path, file_extensions, recursive)?;
    if files.is_empty() {
        anyhow::bail!("No files to process");
    }

    let mut files_to_rename: Vec<RenameItem> = Vec::new();
    for file in files {
        let filename = file
            .file_name()
            .context("Failed to get filename")?
            .to_string_lossy()
            .into_owned();

        if let Some(new_name) = reorder_filename_date(&filename, starts_with_year, verbose) {
            files_to_rename.push(RenameItem {
                path: file,
                filename,
                new_name,
            });
        }
    }

    // Case-insensitive sort by filename
    files_to_rename.sort_by(|a, b| a.filename.to_lowercase().cmp(&b.filename.to_lowercase()));

    let heading = if dryrun {
        "Dryrun:".cyan().bold()
    } else {
        "Rename:".magenta().bold()
    };

    for item in files_to_rename {
        println!("{heading}");
        cli_tools::show_diff(&item.filename, &item.new_name);
        if !dryrun {
            let new_path = root.join(item.new_name);
            if new_path.exists() && !overwrite_existing {
                eprintln!("{}", "File already exists".yellow());
            } else {
                fs::rename(item.path, new_path).context("Failed to rename file")?;
            }
        }
    }

    Ok(())
}

/// Flip date to start with year for all matching directories from given path.
fn date_flip_directories(path: PathBuf, recursive: bool, dryrun: bool) -> Result<()> {
    let directories = directories_to_rename(path, recursive)?;
    if directories.is_empty() {
        anyhow::bail!("No directories to rename")
    }

    let max_chars: usize = directories
        .iter()
        .map(|r| r.filename.chars().count())
        .max()
        .context("Failed to get max path length")?;

    for directory in directories {
        let new_path = directory.path.with_file_name(directory.new_name.clone());
        println!(
            "{:<width$}  ==>  {}",
            directory.filename,
            directory.new_name,
            width = max_chars
        );
        if !dryrun {
            fs::rename(&directory.path, &new_path).with_context(|| {
                format!(
                    "Failed to rename {} to {}",
                    directory.path.display(),
                    new_path.display()
                )
            })?;
        }
    }

    Ok(())
}

/// Get list of files to process
fn files_to_rename(path: &PathBuf, file_extensions: &[&str], recursive: bool) -> Result<(Vec<PathBuf>, PathBuf)> {
    let (mut files, root) = if path.is_file() {
        (
            vec![path.clone()],
            path.parent().context("Failed to get file parent")?.to_path_buf(),
        )
    } else {
        let list: Vec<PathBuf> = WalkDir::new(path)
            .min_depth(1)
            .max_depth(if recursive { usize::MAX } else { 1 })
            .into_iter()
            .filter_map(std::result::Result::ok)
            .map(walkdir::DirEntry::into_path)
            .filter(|path| {
                path.is_file()
                    && path.extension().is_some_and(|ext| {
                        file_extensions.contains(
                            &ext.to_str()
                                .unwrap_or_else(|| panic!("Invalid file extension: {ext:#?}")),
                        )
                    })
            })
            .collect();
        (list, path.clone())
    };

    files.sort();
    Ok((files, root))
}

/// Get list of directories to process
fn directories_to_rename(path: PathBuf, recursive: bool) -> Result<Vec<RenameItem>> {
    let mut directories_to_rename = Vec::new();

    let walker = WalkDir::new(path)
        .min_depth(1)
        .max_depth(if recursive { 100 } else { 1 });

    for entry in walker {
        let entry = entry.context("Failed to read directory entry")?;
        if entry.path().is_dir() {
            let filename = entry.file_name().to_string_lossy().into_owned();
            if let Some(new_name) = reorder_directory_date(&filename) {
                directories_to_rename.push(RenameItem {
                    path: entry.path().to_path_buf(),
                    filename,
                    new_name,
                });
            }
        }
    }

    // Case-insensitive sort by filename
    directories_to_rename.sort_by(|a, b| a.filename.to_lowercase().cmp(&b.filename.to_lowercase()));

    Ok(directories_to_rename)
}

/// Check if filename contains a matching date and reorder it.
fn reorder_filename_date(filename: &str, year_first: bool, verbose: bool) -> Option<String> {
    if RE_CORRECT_DATE_FORMAT.is_match(filename) {
        if verbose {
            println!("Skipping: {}", filename.yellow());
        }
        return None;
    }

    println!("filename: {filename}");

    // Check for full dates
    let mut best_match = None;
    for caps in RE_FULL_DATE.captures_iter(filename) {
        println!("{caps:?}");
        if let Some(date_match) = caps.name("date") {
            let date_str = date_match.as_str();
            println!("date_str: {date_str}");

            let numbers: Vec<&str> = date_str.split('.').map(str::trim).filter(|s| !s.is_empty()).collect();
            if numbers.len() != 3 {
                continue;
            }

            if let Some(date) = parse_date_from_dd_mm_yyyy(numbers[0], numbers[1], numbers[2]) {
                best_match = Some((date_str.to_string(), date.to_string()));
            }
        }
    }

    if let Some((original_date, flip_date)) = best_match {
        let new_name = filename.replacen(&original_date, &flip_date, 1);
        return Some(new_name);
    }

    // Check for short dates
    let mut best_match = None;
    for caps in RE_SHORT_DATE_DAY_FIRST.captures_iter(filename) {
        println!("{caps:?}");
        if let Some(date_match) = caps.name("date") {
            let date_str = date_match.as_str();
            println!("date_str: {date_str}");

            let numbers: Vec<&str> = date_str.split('.').map(str::trim).filter(|s| !s.is_empty()).collect();
            if numbers.len() != 3 {
                continue;
            }

            if let Some(date) = (year_first && numbers[0].len() == 2)
                .then(|| Date::parse_from_short(numbers[0], numbers[1], numbers[2]))
                .flatten()
                .or_else(|| Date::parse_from_short(numbers[2], numbers[1], numbers[0]))
            {
                best_match = Some((date_str.to_string(), date.to_string()));
            }
        }
    }

    if let Some((original_date, flip_date)) = best_match {
        let new_name = filename.replace(&original_date, &flip_date);
        return Some(new_name);
    }

    // Check for short dates
    let mut best_match = None;
    for caps in RE_SHORT_DATE_YEAR_FIRST.captures_iter(filename) {
        println!("{caps:?}");
        if let Some(date_match) = caps.name("date") {
            let date_str = date_match.as_str();
            println!("date_str: {date_str}");

            let numbers: Vec<&str> = date_str.split('.').map(str::trim).filter(|s| !s.is_empty()).collect();
            if numbers.len() != 3 {
                continue;
            }

            if let Some(date) = Date::parse_from_short(numbers[0], numbers[1], numbers[2]) {
                best_match = Some((date_str.to_string(), date.to_string()));
            }
        }
    }

    if let Some((original_date, flip_date)) = best_match {
        let new_name = filename.replace(&original_date, &flip_date);
        return Some(new_name);
    }

    None
}

/// Check if directory name contains a matching date and reorder it.
fn reorder_directory_date(name: &str) -> Option<String> {
    // Handle dd.mm.yyyy format
    println!("dir name: {name}");
    if let Some(caps) = RE_DD_MM_YYYY.captures(name) {
        if let Some(date) = parse_date_from_match(name, &caps) {
            println!("RE_DD_MM_YYYY: {date}");
            let name_part = RE_DD_MM_YYYY.replace(name, "").to_string();
            println!("name_part: {name_part}");
            let name = get_directory_separator(&name_part);
            println!("name: {name}");
            return Some(format!("{}{name}", date.dash_format()));
        }
    }
    // Handle yyyy.mm.dd format
    if let Some(caps) = RE_YYYY_MM_DD.captures(name) {
        println!("caps: {caps:#?}");
        if let Some(date) = parse_date_from_match(name, &caps) {
            println!("RE_YYYY_MM_DD: {date}");
            let name_part = RE_YYYY_MM_DD.replace(name, "").to_string();
            println!("name_part: {name_part}");
            let name = get_directory_separator(&name_part);
            println!("name: {name}");
            return Some(format!("{}{name}", date.dash_format()));
        }
    }
    None
}

fn parse_date_from_match(name: &str, caps: &Captures) -> Option<Date> {
    let year_str = if let Some(y) = caps.name("year") {
        y.as_str().to_string()
    } else {
        eprintln!("{}", format!("Failed to extract 'year' from '{name}'").red());
        return None;
    };
    let month_str = if let Some(m) = caps.name("month") {
        m.as_str()
    } else {
        eprintln!("{}", format!("Failed to extract 'month' from '{name}'").red());
        return None;
    };
    let day_str = if let Some(d) = caps.name("day") {
        d.as_str()
    } else {
        eprintln!("{}", format!("Failed to extract 'day' from '{name}'").red());
        return None;
    };
    let Ok(year) = year_str.parse::<u32>() else {
        eprintln!("{}", format!("Failed to parse 'year' in '{name}'").red());
        return None;
    };
    let Ok(month) = month_str.parse::<u32>() else {
        eprintln!("{}", format!("Failed to parse 'month' in '{name}'").red());
        return None;
    };
    let Ok(day) = day_str.parse::<u32>() else {
        eprintln!("{}", format!("Failed to parse 'day' in '{name}'").red());
        return None;
    };
    Date::try_from(year, month, day)
}

fn parse_date_from_dd_mm_yyyy(day: &str, month: &str, year: &str) -> Option<Date> {
    let Ok(year) = year.parse::<u32>() else {
        eprintln!("{}", format!("Failed to parse year from '{year}'").red());
        return None;
    };
    let Ok(month) = month.parse::<u32>() else {
        eprintln!("{}", format!("Failed to parse month from '{month}'").red());
        return None;
    };
    let Ok(day) = day.parse::<u32>() else {
        eprintln!("{}", format!("Failed to parse day from '{day}'").red());
        return None;
    };
    Date::try_from(year, month, day)
}

fn get_directory_separator(input: &str) -> String {
    let separators = "_-.";
    if input.starts_with(|c: char| separators.contains(c)) {
        input.trim().to_string()
    } else if input.ends_with(|c: char| separators.contains(c)) {
        let separator = input.chars().last().expect("Failed to get last element");
        let rest = &input[..input.len() - 1];
        format!("{separator}{rest}")
    } else {
        format!(" {}", input.trim())
    }
}

#[cfg(test)]
mod regex_tests {
    use super::*;

    #[test]
    fn correct_date_format() {
        assert!(RE_CORRECT_DATE_FORMAT.is_match("2023.12.30"));
        assert!(RE_CORRECT_DATE_FORMAT.is_match("2024.11.30.txt"));
        assert!(RE_CORRECT_DATE_FORMAT.is_match("2020.01.01"));
        assert!(!RE_CORRECT_DATE_FORMAT.is_match("23.12.30"));
        assert!(!RE_CORRECT_DATE_FORMAT.is_match("0123.12.30"));
        assert!(!RE_CORRECT_DATE_FORMAT.is_match("0000.00.30"));
        assert!(!RE_CORRECT_DATE_FORMAT.is_match("0100.1.2"));
        assert!(!RE_CORRECT_DATE_FORMAT.is_match("2000.1.0"));
    }

    #[test]
    fn full_date_valid_dates() {
        let valid_cases = [
            "01.02.2024",
            "10.12.2024",
            "1.12.2024",
            "1.2.2024",
            "12.1.2024",
            "_1.2.2024_",
            "11.1.2024.txt",
            "test-11.01.2024.txt",
            "file.11.01.2024.10.txt",
            "test 1.2.2024",
            "some01.02.2024test",
        ];

        for &case in &valid_cases {
            assert!(RE_FULL_DATE.is_match(case), "Expected to match: {case}");
        }
    }

    #[test]
    fn full_date_invalid_dates() {
        let invalid_cases = [
            "01.02.20000",
            "00.02.2024",
            "001.02.2024",
            "01.002.2024",
            "01.2.200",
            "2024.01.001",
            "123.12.2024",
            "01.12.999",
        ];

        for &case in &invalid_cases {
            assert!(!RE_FULL_DATE.is_match(case), "Expected NOT to match: {case}");
        }
    }
}

#[cfg(test)]
mod filename_tests {
    use super::*;

    #[test]
    fn normal_date() {
        let filename = "20.12.23.txt";
        let correct = "2023.12.20.txt";
        assert_eq!(reorder_filename_date(filename, false, false), Some(correct.to_string()));

        let filename = "30.12.23.txt";
        let correct = "2023.12.30.txt";
        assert_eq!(reorder_filename_date(filename, false, false), Some(correct.to_string()));
        assert_eq!(reorder_filename_date(filename, true, false), Some(correct.to_string()));
    }

    #[test]
    fn full_date() {
        let filename = "report_20.12.2023.txt";
        let correct = "report_2023.12.20.txt";
        assert_eq!(reorder_filename_date(filename, false, false), Some(correct.to_string()));
    }

    #[test]
    fn short_date() {
        let filename = "report_20.12.23.txt";
        let correct = "report_2023.12.20.txt";
        assert_eq!(reorder_filename_date(filename, false, false), Some(correct.to_string()));
    }

    #[test]
    fn single_digit_date() {
        let filename = "report_1.2.23.txt";
        let correct = "report_2023.02.01.txt";
        assert_eq!(reorder_filename_date(filename, false, false), Some(correct.to_string()));
    }

    #[test]
    fn single_digit_date_with_full_year() {
        let filename = "report_8.7.2023.txt";
        let correct = "report_2023.07.08.txt";
        assert_eq!(reorder_filename_date(filename, false, false), Some(correct.to_string()));
    }

    #[test]
    fn no_date() {
        let filename = "report.txt";
        assert_eq!(reorder_filename_date(filename, false, false), None);
        assert_eq!(reorder_filename_date(filename, true, false), None);
        let filename = "123.txt";
        assert_eq!(reorder_filename_date(filename, false, false), None);
        assert_eq!(reorder_filename_date(filename, true, false), None);
        let filename = "00.11.22.txt";
        assert_eq!(reorder_filename_date(filename, false, false), None);
        assert_eq!(reorder_filename_date(filename, true, false), None);
        let filename = "name1000.5.22.txt";
        assert_eq!(reorder_filename_date(filename, false, false), None);
        assert_eq!(reorder_filename_date(filename, true, false), None);
    }

    #[test]
    fn correct_date_format() {
        let filename = "report_2023.12.20.txt";
        assert_eq!(reorder_filename_date(filename, false, false), None);
    }

    #[test]
    fn correct_date_format_year_first() {
        let filename = "report_2023.12.20.txt";
        assert_eq!(reorder_filename_date(filename, true, false), None);
    }

    #[test]
    fn full_date_year_first() {
        let filename = "report_23.12.20.txt";
        let correct = "report_2023.12.20.txt";
        assert_eq!(reorder_filename_date(filename, true, false), Some(correct.to_string()));
    }

    #[test]
    fn not_a_valid_date() {
        let filename = "test-EKS510.13.720p.mp4";
        assert_eq!(reorder_filename_date(filename, false, false), None);
        assert_eq!(reorder_filename_date(filename, true, false), None);

        let filename = "testing08.12.1080p.mp4";
        assert_eq!(reorder_filename_date(filename, false, false), None);
        assert_eq!(reorder_filename_date(filename, true, false), None);
    }

    #[test]
    fn extra_numbers() {
        let name = "meeting.500.2023.02.03";
        assert_eq!(reorder_filename_date(name, true, false), None);
        let name = "something.500.24.07.12";
        let correct = "something.500.2012.07.24";
        assert_eq!(reorder_filename_date(name, false, false), Some(correct.to_string()));
        let name = "something.500.24.07.12";
        let correct = "something.500.2024.07.12";
        assert_eq!(reorder_filename_date(name, true, false), Some(correct.to_string()));
        let name = "meeting 0000.2019-11-17";
        assert_eq!(reorder_filename_date(name, true, false), None);
        let name = "meeting 0000.11.22.pdf";
        assert_eq!(reorder_filename_date(name, true, false), None);
        let name = "meeting 00.11.2022.pdf";
        assert_eq!(reorder_filename_date(name, false, false), None);
        let name = "2000.11.2022.pdf";
        assert_eq!(reorder_filename_date(name, false, false), None);
        let name = "2000.11.200.pdf";
        assert_eq!(reorder_filename_date(name, false, false), None);
        let name = "1080.11.200.pdf";
        assert_eq!(reorder_filename_date(name, false, false), None);
        let name = "600.00.11.2222.pdf";
        assert_eq!(reorder_filename_date(name, false, false), None);
        let name = "99 meeting 20 2019-11-17";
        assert_eq!(reorder_filename_date(name, true, false), None);
    }
}

#[cfg(test)]
mod directory_tests {
    use super::*;

    #[test]
    fn dd_mm_yyyy_format() {
        let dirname = "photos_31.12.2023";
        let correct = "2023-12-31_photos";
        assert_eq!(reorder_directory_date(dirname), Some(correct.to_string()));

        let dirname = "31.12.2023 some files";
        let correct = "2023-12-31 some files";
        assert_eq!(reorder_directory_date(dirname), Some(correct.to_string()));
    }

    #[test]
    fn yyyy_mm_dd_format() {
        let dirname = "archive_2023.01.02";
        let correct = "2023-01-02_archive";
        assert_eq!(reorder_directory_date(dirname), Some(correct.to_string()));

        let dirname = "archive2003.01.02";
        let correct = "2003-01-02 archive";
        assert_eq!(reorder_directory_date(dirname), Some(correct.to_string()));

        let dirname = "2021.11.22  archive";
        let correct = "2021-11-22 archive";
        assert_eq!(reorder_directory_date(dirname), Some(correct.to_string()));

        let dirname = "2021.11.22-archive";
        let correct = "2021-11-22-archive";
        assert_eq!(reorder_directory_date(dirname), Some(correct.to_string()));

        let dirname = "2021.11.22archive";
        let correct = "2021-11-22 archive";
        assert_eq!(reorder_directory_date(dirname), Some(correct.to_string()));
    }

    #[test]
    fn single_digit_date() {
        let dirname = "event_2.7.2023";
        let correct = "2023-07-02_event";
        assert_eq!(reorder_directory_date(dirname), Some(correct.to_string()));

        let dirname = "event 1.2.2015";
        let correct = "2015-02-01 event";
        assert_eq!(reorder_directory_date(dirname), Some(correct.to_string()));

        let dirname = "2.2.2022event2";
        let correct = "2022-02-02 event2";
        assert_eq!(reorder_directory_date(dirname), Some(correct.to_string()));
    }

    #[test]
    fn no_date() {
        let dirname = "general archive";
        assert_eq!(reorder_directory_date(dirname), None);

        let dirname = "general archive 123456";
        assert_eq!(reorder_directory_date(dirname), None);

        let dirname = "archive 2021";
        assert_eq!(reorder_directory_date(dirname), None);

        let dirname = "2021";
        assert_eq!(reorder_directory_date(dirname), None);
    }

    #[test]
    fn unrecognized_date_format() {
        let dirname = "backup_2023-12";
        assert_eq!(reorder_directory_date(dirname), None);

        let dirname = "backup_20001031";
        assert_eq!(reorder_directory_date(dirname), None);
    }

    #[test]
    fn correct_format_with_different_separators() {
        let dirname = "meeting 2023-02-03";
        assert_eq!(reorder_directory_date(dirname), None);
        let dirname = "something2000-2000-09-09";
        assert_eq!(reorder_directory_date(dirname), None);
        let dirname = "99 meeting 2019-11-17";
        assert_eq!(reorder_directory_date(dirname), None);
    }
}
