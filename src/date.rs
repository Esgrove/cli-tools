use std::sync::LazyLock;

use chrono::Datelike;
use colored::Colorize;
use regex::{Captures, Regex};

// Static variables that are initialised at runtime the first time they are accessed.

pub static CURRENT_YEAR: LazyLock<i32> = LazyLock::new(|| chrono::Utc::now().year());

pub static RE_YEAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(20\d{2})\b").expect("Failed to create regex pattern for yyyy year"));

pub static RE_CORRECT_DATE_FORMAT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?P<year>[12]\d{3})\.(?P<month>[12]\d|3[01]|0?[1-9])\.(?P<day>[12]\d|3[01]|0?[1-9])\b")
        .expect("Failed to create regex pattern for correct date")
});

static RE_DD_MM_YYYY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?P<day>[12]\d|3[01]|0?[1-9])\.(?P<month>1[0-2]|0?[1-9])\.(?P<year>\d{4})")
        .expect("Failed to create regex pattern for dd.mm.yyyy")
});

static RE_YYYY_MM_DD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?P<year>[12]\d{3})\.(?P<month>1[0-2]|0?[1-9])\.(?P<day>[12]\d|3[01]|0?[1-9])")
        .expect("Failed to create regex pattern for yyyy.mm.dd")
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

pub struct Date {
    pub year: i32,
    pub month: i32,
    pub day: i32,
}

impl Date {
    /// Check that date is valid
    #[must_use]
    pub fn try_from(year: i32, month: i32, day: i32) -> Option<Self> {
        if year <= *CURRENT_YEAR && year >= 2000 && month > 0 && month <= 12 && day > 0 && day <= 31 {
            Some(Self { year, month, day })
        } else {
            None
        }
    }

    #[must_use]
    pub fn parse_from_short(year: &str, month: &str, day: &str) -> Option<Self> {
        if year.chars().count() != 2 {
            return None;
        }
        let year = format!("20{year}");
        let year = year.parse::<i32>().ok()?;
        let month = month.parse::<i32>().ok()?;
        let day = day.parse::<i32>().ok()?;
        Self::try_from(year, month, day)
    }

    /// Swap year and day fields.
    /// Assumes the current year is actually the day and the current day is a 2-digit suffix for the year.
    ///
    /// For example: `2005.12.23` → `2023.12.05`
    #[must_use]
    pub fn swap_year(&self) -> Option<Self> {
        // Use the last two digits of `year` as the new day
        let new_day = self.year % 100;

        // Prepend "20" to the existing day field to form a new year
        let new_year = 2000 + self.day;

        Self::try_from(new_year, self.month, new_day)
    }

    #[must_use]
    pub fn dash_format(&self) -> String {
        format!("{}-{:02}-{:02}", self.year, self.month, self.day)
    }

    #[allow(unused)]
    #[must_use]
    pub fn dot_format(&self) -> String {
        self.to_string()
    }
}

impl std::fmt::Display for Date {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}.{:02}.{:02}", self.year, self.month, self.day)
    }
}

/// Check if filename contains a matching date and reorder it.
pub fn reorder_filename_date(filename: &str, year_first: bool, swap_year: bool, verbose: bool) -> Option<String> {
    if RE_CORRECT_DATE_FORMAT.is_match(filename) {
        if swap_year {
            let captures = RE_CORRECT_DATE_FORMAT.captures(filename)?;
            let original_date = captures.get(0)?.as_str();
            let swapped_date = parse_date_from_match(filename, &captures)?.swap_year()?;
            let updated_filename = filename.replacen(original_date, &swapped_date.dot_format(), 1);
            return Some(updated_filename);
        }
        // Correctly formatted, skip...
        if verbose {
            println!("Skipping: {}", filename.yellow());
        }
        return None;
    }

    // Check for full dates
    let mut best_match = None;
    for caps in RE_FULL_DATE.captures_iter(filename) {
        if let Some(date_match) = caps.name("date") {
            let date_str = date_match.as_str();

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
        if let Some(date_match) = caps.name("date") {
            let date_str = date_match.as_str();

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
        if let Some(date_match) = caps.name("date") {
            let date_str = date_match.as_str();

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
pub fn reorder_directory_date(name: &str) -> Option<String> {
    // Handle dd.mm.yyyy format
    if let Some(caps) = RE_DD_MM_YYYY.captures(name)
        && let Some(date) = parse_date_from_match(name, &caps)
    {
        let name_part = RE_DD_MM_YYYY.replace(name, "").to_string();
        let name = get_directory_separator(&name_part);
        return Some(format!("{}{name}", date.dash_format()));
    }
    // Handle yyyy.mm.dd format
    if let Some(caps) = RE_YYYY_MM_DD.captures(name)
        && let Some(date) = parse_date_from_match(name, &caps)
    {
        let name_part = RE_YYYY_MM_DD.replace(name, "").to_string();
        let name = get_directory_separator(&name_part);
        return Some(format!("{}{name}", date.dash_format()));
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
    let Ok(year) = year_str.parse::<i32>() else {
        eprintln!("{}", format!("Failed to parse 'year' in '{name}'").red());
        return None;
    };
    let Ok(month) = month_str.parse::<i32>() else {
        eprintln!("{}", format!("Failed to parse 'month' in '{name}'").red());
        return None;
    };
    let Ok(day) = day_str.parse::<i32>() else {
        eprintln!("{}", format!("Failed to parse 'day' in '{name}'").red());
        return None;
    };
    Date::try_from(year, month, day)
}

fn parse_date_from_dd_mm_yyyy(day: &str, month: &str, year: &str) -> Option<Date> {
    let Ok(year) = year.parse::<i32>() else {
        eprintln!("{}", format!("Failed to parse year from '{year}'").red());
        return None;
    };
    let Ok(month) = month.parse::<i32>() else {
        eprintln!("{}", format!("Failed to parse month from '{month}'").red());
        return None;
    };
    let Ok(day) = day.parse::<i32>() else {
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
        assert_eq!(
            reorder_filename_date(filename, false, false, false),
            Some(correct.to_string())
        );

        let filename = "30.12.23.txt";
        let correct = "2023.12.30.txt";
        assert_eq!(
            reorder_filename_date(filename, false, false, false),
            Some(correct.to_string())
        );
        assert_eq!(
            reorder_filename_date(filename, true, false, false),
            Some(correct.to_string())
        );

        let filename = "ABCGIO1848.09.06.2022.720p.mp4";
        let correct = "ABCGIO1848.2022.06.09.720p.mp4";
        assert_eq!(
            reorder_filename_date(filename, false, false, false),
            Some(correct.to_string())
        );
    }

    #[test]
    fn full_date() {
        let filename = "report_20.12.2023.txt";
        let correct = "report_2023.12.20.txt";
        assert_eq!(
            reorder_filename_date(filename, false, false, false),
            Some(correct.to_string())
        );
    }

    #[test]
    fn short_date() {
        let filename = "report_20.12.23.txt";
        let correct = "report_2023.12.20.txt";
        assert_eq!(
            reorder_filename_date(filename, false, false, false),
            Some(correct.to_string())
        );
    }

    #[test]
    fn single_digit_date() {
        let filename = "report_1.2.23.txt";
        let correct = "report_2023.02.01.txt";
        assert_eq!(
            reorder_filename_date(filename, false, false, false),
            Some(correct.to_string())
        );
    }

    #[test]
    fn single_digit_date_with_full_year() {
        let filename = "report_8.7.2023.txt";
        let correct = "report_2023.07.08.txt";
        assert_eq!(
            reorder_filename_date(filename, false, false, false),
            Some(correct.to_string())
        );
    }

    #[test]
    fn no_date() {
        let filename = "report.txt";
        assert_eq!(reorder_filename_date(filename, false, false, false), None);
        assert_eq!(reorder_filename_date(filename, true, false, false), None);
        let filename = "123.txt";
        assert_eq!(reorder_filename_date(filename, false, false, false), None);
        assert_eq!(reorder_filename_date(filename, true, false, false), None);
        let filename = "00.11.22.txt";
        assert_eq!(reorder_filename_date(filename, false, false, false), None);
        assert_eq!(reorder_filename_date(filename, true, false, false), None);
        let filename = "name1000.5.22.txt";
        assert_eq!(reorder_filename_date(filename, false, false, false), None);
        assert_eq!(reorder_filename_date(filename, true, false, false), None);
    }

    #[test]
    fn correct_date_format() {
        let filename = "report_2023.12.20.txt";
        assert_eq!(reorder_filename_date(filename, false, false, false), None);
    }

    #[test]
    fn correct_date_format_year_first() {
        let filename = "report_2023.12.20.txt";
        assert_eq!(reorder_filename_date(filename, true, false, false), None);
    }

    #[test]
    fn full_date_year_first() {
        let filename = "report_23.12.20.txt";
        let correct = "report_2023.12.20.txt";
        assert_eq!(
            reorder_filename_date(filename, true, false, false),
            Some(correct.to_string())
        );
    }

    #[test]
    fn not_a_valid_date() {
        let filename = "test-EKS510.13.720p.mp4";
        assert_eq!(reorder_filename_date(filename, false, false, false), None);
        assert_eq!(reorder_filename_date(filename, true, false, false), None);

        let filename = "testing08.12.1080p.mp4";
        assert_eq!(reorder_filename_date(filename, false, false, false), None);
        assert_eq!(reorder_filename_date(filename, true, false, false), None);
    }

    #[test]
    fn extra_numbers() {
        let name = "meeting.500.2023.02.03";
        assert_eq!(reorder_filename_date(name, true, false, false), None);
        let name = "something.500.24.07.12";
        let correct = "something.500.2012.07.24";
        assert_eq!(
            reorder_filename_date(name, false, false, false),
            Some(correct.to_string())
        );
        let name = "something.500.24.07.12";
        let correct = "something.500.2024.07.12";
        assert_eq!(
            reorder_filename_date(name, true, false, false),
            Some(correct.to_string())
        );
        let name = "meeting 0000.2019-11-17";
        assert_eq!(reorder_filename_date(name, true, false, false), None);
        let name = "meeting 0000.11.22.pdf";
        assert_eq!(reorder_filename_date(name, true, false, false), None);
        let name = "meeting 00.11.2022.pdf";
        assert_eq!(reorder_filename_date(name, false, false, false), None);
        let name = "2000.11.2022.pdf";
        assert_eq!(reorder_filename_date(name, false, false, false), None);
        let name = "2000.11.200.pdf";
        assert_eq!(reorder_filename_date(name, false, false, false), None);
        let name = "1080.11.200.pdf";
        assert_eq!(reorder_filename_date(name, false, false, false), None);
        let name = "600.00.11.2222.pdf";
        assert_eq!(reorder_filename_date(name, false, false, false), None);
        let name = "99 meeting 20 2019-11-17";
        assert_eq!(reorder_filename_date(name, true, false, false), None);
    }

    #[test]
    fn swap_year() {
        let filename = "2023.12.05.jpg";
        let correct = "2005.12.23.jpg";
        let result = reorder_filename_date(filename, false, true, true);
        assert_eq!(result, Some(correct.to_string()));

        let filename = "photo 2023.12.05.jpg";
        let correct = "photo 2005.12.23.jpg";
        let result = reorder_filename_date(filename, false, true, true);
        assert_eq!(result, Some(correct.to_string()));

        let filename = "photo-2001.09.24.jpg";
        let correct = "photo-2024.09.01.jpg";
        let result = reorder_filename_date(filename, true, true, true);
        assert_eq!(result, Some(correct.to_string()));

        let filename = "clip.2001.01.09.mov";
        let correct = "clip.2009.01.01.mov";
        let result = reorder_filename_date(filename, true, true, true);
        assert_eq!(result, Some(correct.to_string()));
    }

    #[test]
    fn swap_year_invalid_future_year() {
        let future_suffix = (*CURRENT_YEAR + 1) % 100;
        let filename = format!("sample_2024.06.{future_suffix:02}.mp4");
        // Should return None because new_year = 20{suffix} > CURRENT_YEAR
        assert_eq!(reorder_filename_date(&filename, true, true, false), None);
    }

    #[test]
    fn swap_year_does_not_trigger_without_flag() {
        let filename = "photo_2023.12.05.jpg";
        assert_eq!(reorder_filename_date(filename, true, false, false), None);
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
