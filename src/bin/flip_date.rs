use std::fs;
use std::path::PathBuf;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use clap::Parser;
use colored::Colorize;
use regex::{Captures, Regex};
use walkdir::WalkDir;

static FILE_EXTENSIONS: [&str; 7] = ["m4a", "mp3", "txt", "rtf", "csv", "mp4", "mkv"];

// Static variables that are initialised at runtime the first time they are accessed.
static RE_DD_MM_YYYY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?P<day>\d{1,2})\.(?P<month>\d{1,2})\.(?P<year>\d{4})")
        .expect("Failed to create regex pattern for dd.mm.yyyy")
});

static RE_YYYY_MM_DD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?P<year>\d{4})\.(?P<month>\d{1,2})\.(?P<day>\d{1,2})")
        .expect("Failed to create regex pattern for yyyy.mm.dd")
});

static RE_CORRECT_DATE_FORMAT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d{4}\.\d{1,2}\.\d{1,2}").expect("Failed to create regex pattern for correct date"));

static RE_FULL_DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d{1,2}\.\d{1,2}\.\d{4}").expect("Failed to create regex pattern for full date"));

static RE_SHORT_DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d{1,2}\.\d{1,2}\.\d{2}").expect("Failed to create regex pattern for short date"));

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

    /// Assume year is first
    #[arg(short, long)]
    year: bool,

    /// Only print changes without renaming
    #[arg(short, long)]
    print: bool,

    /// Use recursive path handling
    #[arg(short, long)]
    recursive: bool,
}

#[derive(Debug)]
struct RenameItem {
    path: PathBuf,
    filename: String,
    new_name: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let path = cli_tools::resolve_input_path(args.path.as_deref())?;
    if args.dir {
        date_flip_directories(path, args.recursive, args.print)
    } else {
        date_flip_files(&path, args.recursive, args.print, args.year)
    }
}

/// Flip date to start with year for all matching files from the given path.
fn date_flip_files(path: &PathBuf, recursive: bool, dryrun: bool, starts_with_year: bool) -> Result<()> {
    let (files, root) = files_to_rename(path, recursive)?;
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

        if let Some(new_name) = reorder_filename_date(&filename, starts_with_year) {
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
            fs::rename(item.path, root.join(item.new_name)).context("Failed to rename file")?;
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
fn files_to_rename(path: &PathBuf, recursive: bool) -> Result<(Vec<PathBuf>, PathBuf)> {
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
                    && path.extension().map_or(false, |ext| {
                        FILE_EXTENSIONS.contains(
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
fn reorder_filename_date(filename: &str, starts_with_year: bool) -> Option<String> {
    if RE_CORRECT_DATE_FORMAT.is_match(filename) {
        println!("Skipping: {}", filename.yellow());
        return None;
    }

    if let Some(date_match) = RE_FULL_DATE.find(filename) {
        let date = date_match.as_str();
        let numbers: Vec<&str> = date.split('.').map(str::trim).filter(|s| !s.is_empty()).collect();

        let mut fixed_numbers: Vec<String> = vec![];
        for part in numbers {
            if part.chars().count() == 1 {
                fixed_numbers.push(format!("0{part}"));
            } else {
                fixed_numbers.push(part.to_string());
            }
        }
        
        if !fixed_numbers.iter().any(|part| part.chars().all(|c| c == '0')) {
            if fixed_numbers[2].len() == 2 {
                fixed_numbers[2] = format!("20{}", fixed_numbers[2]);
            }

            let flip_date = fixed_numbers.iter().rev().cloned().collect::<Vec<_>>().join(".");
            let new_name = filename.replace(date, &flip_date);

            return Some(new_name);
        }
    }
    
    if let Some(date_match) = RE_SHORT_DATE.find(filename) {
        let date = date_match.as_str();
        let numbers: Vec<&str> = date.split('.').map(str::trim).filter(|s| !s.is_empty()).collect();

        let mut fixed_numbers: Vec<String> = vec![];
        for number in numbers {
            if number.chars().count() == 1 {
                fixed_numbers.push(format!("0{number}"));
            } else {
                fixed_numbers.push(number.to_string());
            }
        }
        
        if !fixed_numbers.iter().any(|part| part.chars().all(|c| c == '0')) {
            if starts_with_year && fixed_numbers[0].len() == 2 {
                fixed_numbers[0] = format!("20{}", fixed_numbers[0]);
            } else if fixed_numbers[2].len() == 2 {
                fixed_numbers[2] = format!("20{}", fixed_numbers[2]);
            }

            let flip_date = if starts_with_year {
                fixed_numbers.clone().join(".")
            } else {
                fixed_numbers.iter().rev().cloned().collect::<Vec<_>>().join(".")
            };
            let new_name = filename.replace(date, &flip_date);

            return Some(new_name);
        }
    }

    None
}

/// Check if directory name contains a matching date and reorder it.
fn reorder_directory_date(filename: &str) -> Option<String> {
    if let Some(caps) = RE_DD_MM_YYYY.captures(filename) {
        // Handle dd.mm.yyyy format
        let (year, month, day) = parse_date_from_match(filename, &caps)?;
        let name_part = RE_DD_MM_YYYY.replace(filename, "").to_string();
        let name = get_directory_separator(&name_part);
        return Some(format!("{year}-{month:02}-{day:02}{name}"));
    } else if let Some(caps) = RE_YYYY_MM_DD.captures(filename) {
        // Handle yyyy.mm.dd format
        let (year, month, day) = parse_date_from_match(filename, &caps)?;
        let name_part = RE_YYYY_MM_DD.replace(filename, "").to_string();
        let name = get_directory_separator(&name_part);
        return Some(format!("{year}-{month:02}-{day:02}{name}"));
    }
    None
}

fn parse_date_from_match(filename: &str, caps: &Captures) -> Option<(String, u32, u32)> {
    let year = if let Some(y) = caps.name("year") {
        y.as_str().to_string()
    } else {
        eprintln!("{}", format!("Failed to extract 'year' from '{filename}'").red());
        return None;
    };
    let month_str = if let Some(m) = caps.name("month") {
        m.as_str()
    } else {
        eprintln!("{}", format!("Failed to extract 'month' from '{filename}'").red());
        return None;
    };
    let day_str = if let Some(d) = caps.name("day") {
        d.as_str()
    } else {
        eprintln!("{}", format!("Failed to extract 'day' from '{filename}'").red());
        return None;
    };
    let Ok(month) = month_str.parse::<u32>() else {
        eprintln!("{}", format!("Failed to parse 'month' in '{filename}'").red());
        return None;
    };
    let Ok(day) = day_str.parse::<u32>() else {
        eprintln!("{}", format!("Failed to parse 'day' in '{filename}'").red());
        return None;
    };
    Some((year, month, day))
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
mod filename_tests {
    use super::*;

    #[test]
    fn test_date() {
        let filename = "20.12.2023.txt";
        let correct = "2023.12.20.txt";
        assert_eq!(reorder_filename_date(filename, false), Some(correct.to_string()));
    }
    
    #[test]
    fn test_full_date() {
        let filename = "report_20.12.2023.txt";
        let correct = "report_2023.12.20.txt";
        assert_eq!(reorder_filename_date(filename, false), Some(correct.to_string()));
    }

    #[test]
    fn test_short_date() {
        let filename = "report_20.12.23.txt";
        let correct = "report_2023.12.20.txt";
        assert_eq!(reorder_filename_date(filename, false), Some(correct.to_string()));
    }

    #[test]
    fn test_single_digit_date() {
        let filename = "report_1.2.23.txt";
        let correct = "report_2023.02.01.txt";
        assert_eq!(reorder_filename_date(filename, false), Some(correct.to_string()));
    }

    #[test]
    fn test_single_digit_date_with_full_year() {
        let filename = "report_8.7.2023.txt";
        let correct = "report_2023.07.08.txt";
        assert_eq!(reorder_filename_date(filename, false), Some(correct.to_string()));
    }

    #[test]
    fn test_no_date() {
        let filename = "report.txt";
        assert_eq!(reorder_filename_date(filename, false), None);
    }

    #[test]
    fn test_correct_date_format() {
        let filename = "report_2023.12.20.txt";
        assert_eq!(reorder_filename_date(filename, false), None);
    }

    #[test]
    fn test_correct_date_format_year_first() {
        let filename = "report_2023.12.20.txt";
        assert_eq!(reorder_filename_date(filename, true), None);
    }

    #[test]
    fn test_full_date_year_first() {
        let filename = "report_23.12.20.txt";
        let correct = "report_2023.12.20.txt";
        assert_eq!(reorder_filename_date(filename, true), Some(correct.to_string()));
    }
    
        #[test]
    fn test_extra_numbers() {
        let name = "meeting.500.2023.02.03";
        assert_eq!(reorder_filename_date(name, true), None);
        let name = "something.500.24.07.12";
        assert_eq!(reorder_filename_date(name, true), None);
        let name = "99 meeting 20 2019-11-17";
        assert_eq!(reorder_filename_date(name, true), None);
    }
}

#[cfg(test)]
mod directory_tests {
    use super::*;

    #[test]
    fn test_dd_mm_yyyy_format() {
        let dirname = "photos_31.12.2023";
        let correct = "2023-12-31_photos";
        assert_eq!(reorder_directory_date(dirname), Some(correct.to_string()));

        let dirname = "31.12.2023 some files";
        let correct = "2023-12-31 some files";
        assert_eq!(reorder_directory_date(dirname), Some(correct.to_string()));
    }

    #[test]
    fn test_yyyy_mm_dd_format() {
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
    fn test_single_digit_date() {
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
    fn test_no_date() {
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
    fn test_unrecognized_date_format() {
        let dirname = "backup_2023-12";
        assert_eq!(reorder_directory_date(dirname), None);

        let dirname = "backup_20001031";
        assert_eq!(reorder_directory_date(dirname), None);
    }

    #[test]
    fn test_correct_format_with_different_separators() {
        let dirname = "meeting 2023-02-03";
        assert_eq!(reorder_directory_date(dirname), None);
        let dirname = "something2000-2000-09-09";
        assert_eq!(reorder_directory_date(dirname), None);
        let dirname = "99 meeting 2019-11-17";
        assert_eq!(reorder_directory_date(dirname), None);
    }
}
