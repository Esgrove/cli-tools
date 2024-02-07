extern crate colored;

use anyhow::{Context, Result};
use chrono::{Datelike, Local, NaiveDate};
use clap::Parser;
use colored::Colorize;
use lazy_static::lazy_static;
use regex::Regex;
use rust_xlsxwriter::{Format, FormatAlign, FormatBorder, RowNum, Workbook};
use serde::ser::{Serialize, SerializeStruct, Serializer};
use walkdir::WalkDir;

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::Write;
use std::io::{BufRead, BufReader};

use std::path::{Path, PathBuf};

// Static variables that are initialized at runtime the first time they are accessed.
lazy_static! {
    static ref RE_FINNAIR: Regex = Regex::new(r"(?i)finnair").expect("Failed to create regex pattern for Finnair");
    static ref RE_HTML_AND: Regex = Regex::new(r"(?i)&amp;").expect("Failed to create regex pattern for html");
    static ref RE_SEPARATORS: Regex = Regex::new(r"[\r\n\t]+").expect("Failed to create regex pattern for separators");
    static ref RE_WHITESPACE: Regex = Regex::new(r"\s{2,}").expect("Failed to create regex pattern for whitespace");
    static ref RE_WOLT: Regex = Regex::new(r"(?i)wolt ").expect("Failed to create regex pattern for Wolt");
    static ref RE_ITEM_DATE: Regex =
        Regex::new(r"^(\d{2}\.\d{2}\.)(.*)").expect("Failed to create regex pattern for item date");
    static ref RE_START_DATE: Regex = Regex::new(r#"<StartDate Format="CCYYMMDD">(\d{4})\d{4}</StartDate>"#)
        .expect("Failed to create regex pattern for start date");
    static ref RE_SPECIFICATION_FREE_TEXT: Regex =
        Regex::new(r"^\s*<SpecificationFreeText>(.*?)</SpecificationFreeText>")
            .expect("Failed to create regex pattern for SpecificationFreeText");
    static ref FILTER_PREFIXES: [&'static str; 51] = [
        "1BAR",
        "45 SPECIAL",
        "ALEPA",
        "ALKO",
        "ALLAS SEA POOL",
        "AVECRA",
        "BAR ",
        "BASTARD BURGERS",
        "CLAS OHLSON",
        "CLASSIC TROIJA",
        "DIF DONER",
        "EPASSI",
        "EVENTUAL",
        "F1.COM",
        "FAZER RAVINTOLAT",
        "FINNAIR",
        "FINNKINO",
        "FLOW FESTIVAL ",
        "HENRY'S PUB",
        "HOTEL ",
        "IPA GROUP",
        "K-MARKET",
        "K-RAUTA",
        "KAIKU HELSINKI",
        "KAMPIN ",
        "KUUDESLINJA",
        "LA TORREFAZIONE",
        "LUNDIA",
        "MCD ",
        "MCDONALD",
        "MUJI",
        "PAYPAL EPIC GAMES",
        "PAYPAL LEVISTRAUSS",
        "PAYPAL MCOMPANY",
        "PAYPAL MISTERB",
        "PAYPAL NIKE",
        "PAYPAL STEAM GAMES",
        "PISTE SKI LODGE",
        "RAVINTOLA",
        "RIVIERA",
        "RUKASTORE",
        "S-MARKET",
        "SEISKATUUMA",
        "SMARTUM",
        "SOOSIKAUPPA",
        "SP TOPPED",
        "STADIUM",
        "STOCKMANN",
        "TEERENPELI",
        "TIKETTI.FI",
        "WOLT",
    ];
}

#[derive(Parser, Debug)]
#[command(author, version, name = "visa-parse", about = "Parse credit card Finvoice XML files")]
struct Args {
    /// Input directory or XML file path
    path: String,

    /// Optional output path (default is same as input dir)
    #[arg(short, long, name = "OUTPUT_PATH")]
    output: Option<String>,

    /// Only print items, don't write to file
    #[arg(short, long)]
    print: bool,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

/// One credit card purchase.
#[derive(Debug, Clone, PartialEq)]
struct VisaItem {
    date: NaiveDate,
    name: String,
    sum: f64,
}

impl VisaItem {
    pub fn finnish_sum(&self) -> String {
        format!("{:.2}", self.sum).replace('.', ",")
    }

    pub fn finnish_date(&self) -> String {
        self.date.format("%Y.%m.%d").to_string()
    }
}

impl fmt::Display for VisaItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}   {:>7}€   {}",
            self.finnish_date(),
            self.finnish_sum(),
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

        state.serialize_field("Date", &self.finnish_date())?;
        state.serialize_field("Name", &self.name)?;
        state.serialize_field("Sum", &self.finnish_sum())?;

        state.end()
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let input_path = cli_tools::resolve_input_path(&args.path)?;
    let output_path = cli_tools::resolve_output_path(args.output, &input_path)?;
    visa_parse(input_path, output_path, args.verbose, args.print)
}

fn visa_parse(input: PathBuf, output: PathBuf, verbose: bool, dryrun: bool) -> Result<()> {
    let files = if input.is_file() {
        println!("{}", format!("Parsing file: {}", input.display()).bold().magenta());
        if input.extension() == Some(OsStr::new("xml")) {
            vec![input.clone()]
        } else {
            Vec::new()
        }
    } else {
        println!(
            "{}",
            format!("Parsing files from: {}", input.display()).bold().magenta()
        );
        get_xml_files(&input)
    };

    if files.is_empty() {
        anyhow::bail!("No XML files to parse".red());
    }

    let items = parse_files(files, verbose)?;
    println!("Found {} items in total", items.len());
    print_totals(&items);

    if !dryrun {
        write_to_csv(&items, &output)?;
        write_to_excel(&items, &output)?;
    }

    Ok(())
}

fn get_xml_files<P: AsRef<Path>>(root: P) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !cli_tools::is_hidden(e))
        .filter_map(|e| e.ok())
        .map(|e| e.path().to_owned())
        .filter(|path| path.is_file() && path.extension() == Some(OsStr::new("xml")))
        .collect();

    files.sort_by(|a, b| {
        let a_str = a.to_string_lossy().to_lowercase();
        let b_str = b.to_string_lossy().to_lowercase();
        a_str.cmp(&b_str)
    });
    files
}

fn parse_files(files: Vec<PathBuf>, verbose: bool) -> Result<Vec<VisaItem>> {
    let mut result: Vec<VisaItem> = Vec::new();
    let digits = if files.len() < 10 {
        1
    } else {
        ((files.len() as f64).log10() as usize) + 1
    };
    for (number, file) in files.iter().enumerate() {
        println!("{:>0width$}: {}", number + 1, file.display(), width = digits);
        let (raw_lines, year) = read_xml_file(file);
        let items = extract_items(&raw_lines, year);
        if verbose {
            for item in &items {
                println!("{}", item);
            }
        }
        if items.is_empty() {
            println!("{}", "No items found...".yellow())
        } else {
            result.extend(items);
        }
    }
    result.sort();
    Ok(result)
}

/// Read transaction lines from an XML file.
fn read_xml_file(file: &Path) -> (Vec<String>, i32) {
    let mut lines: Vec<String> = Vec::new();
    let mut year = Local::now().year();
    let xml_file = match File::open(file) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{}", format!("Failed to open file: {}\n{}", file.display(), e).red());
            return (lines, year);
        }
    };

    let reader = BufReader::new(xml_file);
    for line in reader.lines().flatten() {
        if let Some(caps) = RE_START_DATE.captures(&line) {
            if let Some(matched) = caps.get(1) {
                match matched.as_str().parse::<i32>() {
                    Ok(y) => {
                        year = y;
                    }
                    Err(e) => {
                        eprintln!("{}", format!("Failed to parse year from start date: {e}").red())
                    }
                }
            }
        }
        if let Some(caps) = RE_SPECIFICATION_FREE_TEXT.captures(&line) {
            if let Some(matched) = caps.get(1) {
                let text = matched.as_str();
                if RE_ITEM_DATE.is_match(text) {
                    lines.push(text.to_string());
                }
            }
        }
    }
    (lines, year)
}

/// Convert text lines to visa items.
fn extract_items(rows: &[String], year: i32) -> Vec<VisaItem> {
    let mut formatted_data: Vec<(i32, i32, String, f64)> = Vec::new();
    for line in rows.iter() {
        let (date, name, sum) = split_item_text(line);
        let (day, month) = date.split_once('.').unwrap();
        let month: i32 = month.replace('.', "").parse().unwrap();
        let day: i32 = day.parse().unwrap();
        let name = format_name(&name);
        let sum = format_sum(&sum).unwrap();
        formatted_data.push((day, month, name, sum));
    }

    // Determine if there's a transition from December to January.
    let mut year_transition_detected = false;
    let mut last_month: i32 = 0;
    for (_, month, _, _) in formatted_data.iter() {
        if *month == 1 && last_month == 12 {
            year_transition_detected = true;
            break;
        }
        last_month = *month;
    }

    let previous_year = year - 1;
    let mut result: Vec<VisaItem> = Vec::new();
    for (day, month, name, sum) in formatted_data.into_iter() {
        let year = if month == 12 && year_transition_detected {
            previous_year
        } else {
            year
        };

        let date_str = format!("{:02}.{:02}.{}", day, month, year);
        if let Ok(date) = NaiveDate::parse_from_str(&date_str, "%d.%m.%Y") {
            result.push(VisaItem { date, name, sum });
        } else {
            eprintln!("{}", format!("Failed to parse date: {}", date_str).red())
        }
    }

    result
}

fn clean_whitespaces(text: &str) -> String {
    RE_WHITESPACE
        .replace_all(RE_SEPARATORS.replace_all(text, " ").as_ref(), " ")
        .to_string()
}

fn format_name(text: &str) -> String {
    let mut name = text.replace("Osto ", "").replace(['*', '/', '_'], " ");

    if RE_FINNAIR.is_match(&name) {
        name = "FINNAIR".to_string();
    }
    if RE_WOLT.is_match(&name) {
        name = "WOLT".to_string();
    }

    name = RE_HTML_AND.replace_all(&name, "&").to_string();
    name = name.to_uppercase();
    name = name
        .replace("VFI*", "")
        .replace("VFI ", "")
        .replace(" DRI ", "")
        .replace(" . ", " ")
        .replace("CHATGPT SUBSCRIPTION HTTPSOPENAI.C", "CHATGPT SUBSCRIPTION OPENAI.COM");

    name = name.trim().to_string();
    name = RE_WHITESPACE.replace_all(&name, " ").to_string();

    name = replace_from_start(&name, "CHF ", "");
    name = replace_from_start(&name, "CHF", "");
    name = replace_from_start(&name, "WWW.", "");
    name = replace_from_start(&name, "CHF", "");
    name = replace_from_start(&name, "CHF", "");

    if name.starts_with("PAYPAL PATREON") {
        name = "PAYPAL PATREON".to_string();
    }
    if name.starts_with("PAYPAL DJCITY") {
        name = "PAYPAL DJCITY".to_string();
    }
    if name.starts_with("PAYPAL BANDCAMP") {
        name = "PAYPAL BANDCAMP".to_string();
    }

    name = name.trim().to_string();
    name = RE_WHITESPACE.replace_all(&name, " ").to_string();
    name
}

fn replace_from_start(name: &str, pattern: &str, replacement: &str) -> String {
    if name.starts_with(pattern) {
        name.replacen(pattern, replacement, 1)
    } else {
        name.to_string()
    }
}

/// Convert Finnish currency value string to float
fn format_sum(value: &str) -> Result<f64> {
    value
        .trim()
        .replace(',', ".")
        .parse::<f64>()
        .context(format!("Failed to parse sum as float: {}", value))
}

fn print_totals(items: &[VisaItem]) {
    let total_sum: f64 = items.iter().map(|item| item.sum).sum();
    let count = items.len() as f64;
    let average = if count > 0.0 { total_sum / count } else { 0.0 };

    println!("Sum: {:.2}€", total_sum);
    println!("Average: {:.2}€", average);
}

fn split_item_text(input: &str) -> (String, String, String) {
    // Split the string at the first whitespace
    let mut parts = input.splitn(2, ' ');
    let first_part = parts.next().unwrap_or("").trim().to_string();
    let remainder = parts.next().unwrap_or("");
    let cleaned_remainder = clean_whitespaces(remainder);
    let (second_part, third_part) = split_from_last_whitespace(&cleaned_remainder);

    (first_part, second_part, third_part)
}

fn split_from_last_whitespace(s: &str) -> (String, String) {
    let mut parts = s.rsplitn(2, char::is_whitespace);
    let after = parts.next().unwrap_or("").to_string();
    let before = parts.next().unwrap_or("").to_string();

    (before, after)
}

/// Save parsed data to a CSV file
fn write_to_csv(items: &[VisaItem], output_path: &Path) -> Result<()> {
    let output_file = if output_path
        .extension()
        .and_then(|ext| ext.to_str())
        .map_or(false, |ext| ext.eq_ignore_ascii_case("csv"))
    {
        output_path.to_path_buf()
    } else {
        output_path.join("VISA.csv")
    };
    println!(
        "{}",
        format!("Writing data to {}", output_file.display()).green().bold()
    );
    if output_file.exists() {
        if let Err(e) = std::fs::remove_file(&output_file) {
            eprintln!("{}", format!("Failed to remove existing csv file: {e}").red())
        }
    }
    let mut file = File::create(output_file)?;
    writeln!(file, "Date,Sum,Name")?;
    for item in items {
        writeln!(file, "{},{:.2},{}", item.finnish_date(), item.sum, item.name)?;
    }
    Ok(())
}

fn write_to_excel(items: &[VisaItem], output_path: &Path) -> Result<()> {
    let output_file = if output_path
        .extension()
        .and_then(|ext| ext.to_str())
        .map_or(false, |ext| {
            ext.eq_ignore_ascii_case("csv") || ext.eq_ignore_ascii_case("xlsx")
        }) {
        output_path.with_extension("xlsx")
    } else {
        output_path.join("VISA.xlsx")
    };
    println!(
        "{}",
        format!("Writing data to {}", output_file.display()).green().bold()
    );
    let mut workbook = Workbook::new();
    let worksheet = workbook.add_worksheet().set_name("VISA")?;
    let header_format = Format::new()
        .set_bold()
        .set_border(FormatBorder::Thin)
        .set_background_color("C6E0B4");

    worksheet.serialize_headers_with_format::<VisaItem>(0, 0, &items[0], &header_format)?;
    worksheet.serialize(&items)?;
    worksheet.autofit();

    let dj = workbook.add_worksheet().set_name("DJ")?;
    let sum_format = Format::new().set_align(FormatAlign::Right).set_num_format("0,00");
    dj.serialize_headers_with_format::<VisaItem>(0, 0, &items[0], &header_format)?;
    let mut row: usize = 1;
    for item in items.iter() {
        // Filter out common non-DJ items
        if FILTER_PREFIXES.iter().any(|&prefix| item.name.starts_with(prefix)) {
            continue;
        }
        dj.write_string(row as RowNum, 0, item.finnish_date())?;
        dj.write_string(row as RowNum, 1, item.name.clone())?;
        dj.write_string_with_format(row as RowNum, 2, item.finnish_sum(), &sum_format)?;
        row += 1;
    }
    dj.autofit();

    if output_file.exists() {
        if let Err(e) = std::fs::remove_file(&output_file) {
            eprintln!("{}", format!("Failed to remove existing xlsx file: {e}").red())
        }
    }
    workbook.save(output_file)?;
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
