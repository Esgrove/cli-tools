extern crate colored;

use anyhow::{Context, Result};
use chrono::{Datelike, Local, NaiveDate};
use clap::Parser;
use colored::Colorize;
use lazy_static::lazy_static;
use regex::Regex;
use serde::ser::{Serialize, SerializeStruct, Serializer};
use walkdir::{DirEntry, WalkDir};

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fs::File;
use std::io::Write;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::{fmt, fs};

// Static variables that are initialized at runtime the first time they are accessed.
lazy_static! {
    static ref RE_SEPARATORS: Regex =
        Regex::new(r"[\r\n\t]+").expect("Failed to create regex pattern for separators");
    static ref RE_WHITESPACE: Regex =
        Regex::new(r"\s{2,}").expect("Failed to create regex pattern for whitespace");
    static ref RE_FINNAIR: Regex =
        Regex::new(r"(?i)finnair").expect("Failed to create regex pattern for Finnair");
    static ref RE_WOLT: Regex =
        Regex::new(r"(?i)wolt ").expect("Failed to create regex pattern for Wolt");
    static ref RE_SPECIFICATION_FREE_TEXT: Regex =
        Regex::new(r"^\s*<SpecificationFreeText>(.*?)</SpecificationFreeText>")
            .expect("Failed to create regex pattern for SpecificationFreeText");
    static ref RE_ITEM_DATE: Regex =
        Regex::new(r"^(\d{2}\.\d{2}\.)(.*)").expect("Failed to create regex pattern for item date");
}

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    name = "visa-parse",
    about = "Parse credit card Finvoice XML files"
)]
struct Args {
    /// Input directory or XML file path
    path: String,

    /// Optional output path (default is same as input dir)
    #[arg(short, long, name = "OUTPUT_PATH")]
    output: Option<String>,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct VisaItem {
    date: NaiveDate,
    name: String,
    sum: f64,
}

impl fmt::Display for VisaItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}   {:>7.2}€   {}",
            self.date.format("%Y.%m.%d"),
            self.sum,
            self.name
        )
    }
}

// f64 does not have Eq so this way it uses PartialEq
impl Eq for VisaItem {}

impl PartialOrd for VisaItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for VisaItem {
    fn cmp(&self, other: &Self) -> Ordering {
        self.date
            .cmp(&other.date)
            .then_with(|| self.name.cmp(&other.name))
            .then_with(|| self.sum.partial_cmp(&other.sum).unwrap_or(Ordering::Equal))
    }
}

impl Serialize for VisaItem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("VisaItem", 3)?;

        let formatted_date = self.date.format("%Y.%m.%d").to_string();
        state.serialize_field("date", &formatted_date)?;

        let formatted_sum = format!("{:.2}€", self.sum);
        state.serialize_field("sum", &formatted_sum)?;

        state.serialize_field("name", &self.name)?;

        state.end()
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let path = args.path.trim();
    let input_path = get_absolute_input_path(path)?;
    let output_path = get_output_path(args.output, &input_path)?;
    visa_parse(input_path, output_path, args.verbose)
}

fn get_absolute_input_path(input_path: &str) -> Result<PathBuf> {
    if input_path.is_empty() {
        anyhow::bail!("empty input path");
    }
    let filepath = Path::new(input_path);
    if !filepath.exists() {
        anyhow::bail!(
            "Input path does not exist or is not accessible: '{}'",
            filepath.display()
        );
    }
    let absolute_input_path = fs::canonicalize(filepath)?;
    Ok(absolute_input_path)
}

fn get_output_path(output: Option<String>, absolute_input_path: &Path) -> Result<PathBuf> {
    let output_path = {
        let path = output.unwrap_or_default().trim().to_string();
        if path.is_empty() {
            if absolute_input_path.is_file() {
                absolute_input_path
                    .parent()
                    .context("Failed to get parent directory")?
                    .to_path_buf()
            } else {
                absolute_input_path.to_path_buf()
            }
        } else {
            Path::new(&path).to_path_buf()
        }
    };
    Ok(output_path)
}

fn visa_parse(input: PathBuf, output: PathBuf, verbose: bool) -> Result<()> {
    let files = if input.is_file() {
        println!(
            "{}",
            format!("Parsing file: {}", input.display())
                .bold()
                .magenta()
        );
        if input.extension() == Some(OsStr::new("xml")) {
            vec![input.clone()]
        } else {
            Vec::new()
        }
    } else {
        println!(
            "{}",
            format!("Parsing files from: {}", input.display())
                .bold()
                .magenta()
        );
        get_xml_files(&input)
    };

    if files.is_empty() {
        anyhow::bail!("No XML files to parse".red());
    }

    let items = parse_files(files, verbose)?;
    println!("Found {} items in total", items.len());
    print_totals(&items);
    write_to_csv(&items, &output)?;

    Ok(())
}

fn parse_files(files: Vec<PathBuf>, verbose: bool) -> Result<Vec<VisaItem>> {
    let mut result: Vec<VisaItem> = Vec::new();
    let digits = if files.len() < 10 {
        1
    } else {
        ((files.len() as f64).log10() as usize) + 1
    };
    for (number, file) in files.iter().enumerate() {
        if verbose {
            println!(
                "{:>0width$}: {}",
                number + 1,
                file.display(),
                width = digits
            );
        }
        let raw_lines = read_xml_file(file);
        let items = extract_items(raw_lines);
        if verbose {
            for item in &items {
                println!("{}", item);
            }
        }
        result.extend(items);
    }
    result.sort();
    Ok(result)
}

/// Convert Finnish currency value string to float
fn format_sum(value: &str) -> Result<f64> {
    value
        .trim()
        .replace(',', ".")
        .parse::<f64>()
        .context(format!("Failed to parse sum as float: {}", value))
}

fn format_name(text: &str) -> String {
    let mut name = text.replace("Osto ", "").replace(['*', '/', '_'], " ");

    if RE_FINNAIR.is_match(&name) {
        name = "Finnair".to_string();
    }
    if RE_WOLT.is_match(&name) {
        name = "Wolt".to_string();
    }

    name = name.to_uppercase();
    name = name
        .replace("VFI*", "")
        .replace(" DRI ", "")
        .replace(" . ", " ");

    if name.starts_with("CHF ") {
        name = name.replacen("CHF ", "", 1);
    }
    if name.starts_with("CHF") {
        name = name.replacen("CHF", "", 1);
    }
    if name.starts_with("WWW.") {
        name = name.replacen("WWW.", "", 1);
    }
    if name.starts_with("PAYPAL PATREON") {
        name = "PAYPAL PATREON".to_string();
    }
    if name.starts_with("PAYPAL DJCITY") {
        name = "PAYPAL DJCITY".to_string();
    }

    name = name.replace(
        "CHATGPT SUBSCRIPTION HTTPSOPENAI.C",
        "CHATGPT SUBSCRIPTION OPENAI.COM",
    );

    name = name.trim().to_string();
    name = RE_WHITESPACE.replace_all(&name, " ").to_string();
    name
}

fn print_totals(items: &[VisaItem]) {
    let total_sum: f64 = items.iter().map(|item| item.sum).sum();
    let count = items.len() as f64;
    let average = if count > 0.0 { total_sum / count } else { 0.0 };

    println!("Sum: {:.2}€", total_sum);
    println!("Average: {:.2}€", average);
}

fn clean_whitespaces(text: &str) -> String {
    RE_WHITESPACE
        .replace_all(RE_SEPARATORS.replace_all(text, " ").as_ref(), " ")
        .to_string()
}

fn split_from_last_whitespace(s: &str) -> (String, String) {
    let mut parts = s.rsplitn(2, char::is_whitespace);
    let after = parts.next().unwrap_or("").to_string();
    let before = parts.next().unwrap_or("").to_string();

    (before, after)
}

fn is_hidden(entry: &DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|s| s.starts_with('.'))
        .unwrap_or(false)
}

fn get_xml_files<P: AsRef<Path>>(root: P) -> Vec<PathBuf> {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_hidden(e))
        .filter_map(|e| e.ok())
        .map(|e| e.path().to_owned())
        .filter(|path| path.is_file() && path.extension() == Some(OsStr::new("xml")))
        .collect()
}

/// Read transaction lines from an XML file.
fn read_xml_file(file: &Path) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let xml_file = match File::open(file) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "{}",
                format!("Failed to open file: {}\n{}", file.display(), e).red()
            );
            return lines;
        }
    };

    let reader = BufReader::new(xml_file);
    for line in reader.lines().flatten() {
        if let Some(caps) = RE_SPECIFICATION_FREE_TEXT.captures(&line) {
            if let Some(matched) = caps.get(1) {
                let text = matched.as_str();
                if RE_ITEM_DATE.is_match(text) {
                    lines.push(text.to_string());
                }
            }
        }
    }
    lines
}

fn split_item_row(input: &str) -> (String, String, String) {
    // Split the string at the first whitespace
    let mut parts = input.splitn(2, ' ');
    let first_part = parts.next().unwrap_or("").trim().to_string();
    let remainder = parts.next().unwrap_or("");
    let cleaned_remainder = clean_whitespaces(remainder);
    let (second_part, third_part) = split_from_last_whitespace(&cleaned_remainder);

    (first_part, second_part, third_part)
}

/// Convert text lines to visa items.
fn extract_items(rows: Vec<String>) -> Vec<VisaItem> {
    let mut formatted_data: Vec<(u32, u32, String, f64)> = Vec::new();
    for line in rows.iter() {
        let (date, name, sum) = split_item_row(line);
        let (day, month) = date.split_once('.').unwrap();
        let month: u32 = month.replace('.', "").parse().unwrap();
        let day: u32 = day.parse().unwrap();
        let name = format_name(&name);
        let sum = format_sum(&sum).unwrap();
        formatted_data.push((day, month, name, sum));
    }

    // Determine if there's a transition from December to January.
    let mut year_transition_detected = false;
    let mut last_month: u32 = 0;
    for (_, month, _, _) in formatted_data.iter() {
        if *month == 1u32 && last_month == 12u32 {
            year_transition_detected = true;
            break;
        }
        last_month = *month;
    }

    let current_year = Local::now().year();
    let previous_year = current_year - 1;
    let mut result: Vec<VisaItem> = Vec::new();
    for (day, month, name, sum) in formatted_data.into_iter() {
        let year = if month == 12 && year_transition_detected {
            previous_year
        } else {
            current_year
        };

        let new_date_str = format!("{:02}.{:02}.{}", day, month, year);
        if let Ok(date) = NaiveDate::parse_from_str(&new_date_str, "%d.%m.%Y") {
            result.push(VisaItem { date, name, sum });
        } else {
            eprintln!(
                "{}",
                format!("Failed to parse date: {}", new_date_str).red()
            )
        }
    }

    result
}

fn write_to_csv(items: &[VisaItem], output_path: &Path) -> Result<()> {
    let output_file = if output_path
        .extension()
        .and_then(|ext| ext.to_str())
        .map_or(false, |ext| ext.eq_ignore_ascii_case("csv"))
    {
        output_path.to_path_buf()
    } else {
        output_path.join("visa.csv")
    };
    println!(
        "{}",
        format!("Writing data to {}", output_file.display())
            .green()
            .bold()
    );
    let mut file = File::create(output_file)?;
    writeln!(file, "Date,Sum,Name")?;
    for item in items {
        writeln!(
            file,
            "{},{:.2}€,{}",
            item.date.format("%Y.%m.%d"),
            item.sum,
            item.name
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod test_format_sum {
    use super::*;

    #[test]
    fn test_normal_value() {
        assert_eq!(format_sum("123,45").unwrap(), 123.45);
    }

    #[test]
    fn test_value_with_whitespace() {
        assert_eq!(format_sum("  678,90  ").unwrap(), 678.90);
    }

    #[test]
    fn test_invalid_format() {
        assert!(format_sum("invalid").is_err());
    }

    #[test]
    fn test_large_number() {
        assert_eq!(format_sum("1234567,89").unwrap(), 1234567.89);
    }

    #[test]
    fn test_small_number() {
        assert_eq!(format_sum("0,01").unwrap(), 0.01);
    }

    #[test]
    fn test_number_with_many_decimal_places() {
        assert_eq!(format_sum("1,234567").unwrap(), 1.234567);
    }

    #[test]
    fn test_negative_number() {
        assert_eq!(format_sum("-123,45").unwrap(), -123.45);
    }

    #[test]
    fn test_zero_value() {
        assert_eq!(format_sum("0").unwrap(), 0.0);
    }

    #[test]
    fn test_number_without_decimal() {
        assert_eq!(format_sum("1234").unwrap(), 1234.0);
    }
}
