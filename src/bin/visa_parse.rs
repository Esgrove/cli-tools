use std::cmp::Ordering;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::Write;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::{Datelike, Local, NaiveDate};
use clap::Parser;
use colored::Colorize;
use lazy_static::lazy_static;
use regex::Regex;
use rust_xlsxwriter::{Format, FormatAlign, FormatBorder, RowNum, Workbook};
use serde::ser::{Serialize, SerializeStruct, Serializer};
use walkdir::WalkDir;

// Static variables that are initialized at runtime the first time they are accessed.
lazy_static! {
    static ref RE_BRACKETS: Regex = Regex::new(r"[\[\({\]}\)]+").expect("Failed to create regex pattern for brackets");
    static ref RE_HTML_AND: Regex = Regex::new(r"(?i)&amp;").expect("Failed to create regex pattern for html");
    static ref RE_SEPARATORS: Regex = Regex::new(r"[\r\n\t]+").expect("Failed to create regex pattern for separators");
    static ref RE_WHITESPACE: Regex = Regex::new(r"\s{2,}").expect("Failed to create regex pattern for whitespace");
    static ref RE_ITEM_DATE: Regex =
        Regex::new(r"^(\d{2}\.\d{2}\.)(.*)").expect("Failed to create regex pattern for item date");
    static ref RE_START_DATE: Regex = Regex::new(r#"<StartDate Format="CCYYMMDD">(\d{4})\d{4}</StartDate>"#)
        .expect("Failed to create regex pattern for start date");
    static ref RE_SPECIFICATION_FREE_TEXT: Regex =
        Regex::new(r"^\s*<SpecificationFreeText>(.*?)</SpecificationFreeText>")
            .expect("Failed to create regex pattern for SpecificationFreeText");
    // Replace each pattern with given substitute
    static ref REPLACE_PAIRS: [(&'static str, &'static str); 10] = [
        ( "4029357733", ""),
        (" - ", " "),
        (" . ", " "),
        (" 35314369001", ""),
        (" 402-935-7733", ""),
        (" DRI ", ""),
        (" LEVISTRAUSS ", " LEVIS "),
        (".COMFI", ".COM"),
        ("CHATGPT SUBSCRIPTION HTTPSOPENAI.C", "CHATGPT SUBSCRIPTION OPENAI.COM"),
        ("VFI ", ""),
    ];
    // Replace names starting with these with just the prefix given here
    static ref REPLACE_WITH_START: [&'static str; 5] = [
        "PAYPAL BANDCAMP",
        "PAYPAL BEATPORT",
        "PAYPAL DJCITY",
        "PAYPAL MISTERB",
        "PAYPAL PATREON",
    ];
    static ref FILTER_PREFIXES: [&'static str; 71] = [
        "1BAR",
        "45 SPECIAL",
        "ALEPA",
        "ALKO",
        "ALLAS SEA POOL",
        "AVECRA",
        "BAR ",
        "BASTARD BURGERS",
        "CHATGPT SUBSCRIPTION",
        "CLAS OHLSON",
        "CLASSIC TROIJA",
        "COCKTAIL TRADING COMPA",
        "CRAFT BEER HELSINKI",
        "DIF DONER",
        "EPASSI",
        "EVENTUAL",
        "F1.COM",
        "FAZER RAVINTOLAT",
        "FINNAIR",
        "FINNKINO",
        "FISKARS FINLAND",
        "FLOW FESTIVAL ",
        "HANKI BAARI",
        "HENRY'S PUB",
        "HESBURGER",
        "HOTEL ",
        "IPA GROUP",
        "K-MARKET",
        "K-RAUTA",
        "KAIKU HELSINKI",
        "KAMPIN ",
        "KELTAINEN RUUSU",
        "KULTTUURITALO",
        "KULUTTAJA.FI",
        "KUUDESLINJA",
        "LA TORREFAZIONE",
        "LAMINA SKATE SH",
        "LOPEZ KALLIO",
        "LUNDIA",
        "MAKIA CLOTHING",
        "MCD ",
        "MCDONALD",
        "MONOMESTA",
        "MUJI",
        "PAYPAL EPIC GAMES",
        "PAYPAL LEVISTRAUSS",
        "PAYPAL MCOMPANY",
        "PAYPAL MISTERB",
        "PAYPAL NIKE",
        "PAYPAL STEAM GAMES",
        "PISTE SKI LODGE",
        "PIZZALA",
        "RAVINTOLA",
        "RIIPINEN RESTAURANTS",
        "RIIPISEN RIISTA",
        "RIVIERA",
        "RUKAHUOLTO",
        "RUKAN CAMP",
        "RUKASTORE",
        "S-MARKET",
        "SEISKATUUMA",
        "SMARTUM",
        "SOOSIKAUPPA",
        "SOUP&MORE",
        "SP TOPPED",
        "STADIUM",
        "STOCKMANN",
        "TEERENPELI",
        "TIKETTI.FI",
        "TUNTUR.MAX OY SKI BISTRO",
        "WOLT",
    ];
}

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    name = "visa-parse",
    about = "Parse Finvoice XML credit card statement files"
)]
struct Args {
    /// Optional input directory or XML file path
    path: Option<String>,

    /// Optional output path (default is the input directory)
    #[arg(short, long, name = "OUTPUT_PATH")]
    output: Option<String>,

    /// Only print information without writing to file
    #[arg(short, long)]
    print: bool,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

/// Represents one credit card purchase.
#[derive(Debug, Clone, PartialEq)]
struct VisaItem {
    date: NaiveDate,
    name: String,
    sum: f64,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let input_path = cli_tools::resolve_input_path(args.path)?;
    let output_path = cli_tools::resolve_output_path(args.output, &input_path)?;
    visa_parse(input_path, output_path, args.verbose, args.print)
}

/// Parse data from files and write formatted items to CSV and Excel.
fn visa_parse(input: PathBuf, output: PathBuf, verbose: bool, dryrun: bool) -> Result<()> {
    let (root, files) = get_file_list(&input)?;
    if files.is_empty() {
        anyhow::bail!("No XML files to parse".red());
    }

    let num_files = files.len();
    let items = parse_files(&root, files, verbose)?;
    let totals = calculate_totals_for_each_name(&items);
    print_statistics(&items, &totals, num_files, verbose);

    if !dryrun {
        write_to_csv(&items, &output)?;
        write_to_excel(&items, &totals, &output)?;
    }

    Ok(())
}

/// Return file root and list of files from the input path that can be either a directory or single file.
fn get_file_list(input: &PathBuf) -> Result<(PathBuf, Vec<PathBuf>)> {
    if input.is_file() {
        println!("{}", format!("Parsing file: {}", input.display()).bold().magenta());
        if input.extension() == Some(OsStr::new("xml")) {
            let parent = input.parent().context("Failed to get parent directory")?.to_path_buf();
            Ok((parent, vec![input.clone()]))
        } else {
            Err(anyhow!("Input path is not an XML file: {}", input.display()))
        }
    } else {
        println!(
            "{}",
            format!("Parsing files from: {}", input.display()).bold().magenta()
        );
        Ok((input.to_path_buf(), get_xml_files(input)))
    }
}

/// Collect all XML files recursively from the given root path.
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

/// Parse raw XML files.
fn parse_files(root: &Path, files: Vec<PathBuf>, verbose: bool) -> Result<Vec<VisaItem>> {
    let mut result: Vec<VisaItem> = Vec::new();
    let digits = if files.len() < 10 {
        1
    } else {
        ((files.len() as f64).log10() as usize) + 1
    };
    for (number, file) in files.iter().enumerate() {
        print!(
            "{}",
            format!(
                "{:>0width$}: {}",
                number + 1,
                cli_tools::get_relative_path_or_filename(file, root),
                width = digits
            )
            .bold()
        );
        let (raw_lines, year) = read_xml_file(file);
        let items = extract_items(&raw_lines, year);
        if items.is_empty() {
            println!(" ({})", "0".yellow());
        } else {
            println!(" ({})", format!("{}", items.len()).cyan());
            if verbose {
                for item in &items {
                    println!("  {}", item);
                }
            }
            result.extend(items);
        }
    }
    result.sort();
    println!(
        "Found {} items from {}",
        result.len(),
        if files.len() > 1 {
            format!("{} files", files.len())
        } else {
            "1 file".to_string()
        }
    );
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
    for line in reader.lines().map_while(Result::ok) {
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

/// Calculate the total sum for each unique name and return sorted in descending order.
fn calculate_totals_for_each_name(items: &[VisaItem]) -> Vec<(String, f64)> {
    let mut totals: HashMap<String, f64> = HashMap::new();
    for item in items {
        *totals.entry(item.name.clone()).or_insert(0.0) += item.sum;
    }
    let mut totals_vec: Vec<(String, f64)> = totals.into_iter().collect();

    // Sort the vector in descending order based on the sum
    totals_vec.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
    totals_vec
}

/// Remove extra whitespaces and separators.
fn clean_whitespaces(text: &str) -> String {
    RE_WHITESPACE
        .replace_all(RE_SEPARATORS.replace_all(text, " ").as_ref(), " ")
        .to_string()
}

/// Format item names to consistent style.
fn format_name(text: &str) -> String {
    let mut name = text.replace("Osto ", "").replace(['*', '/', '_'], " ");
    name = RE_HTML_AND.replace_all(&name, "&").to_string();
    name = RE_BRACKETS.replace_all(&name, "").to_string();
    name = RE_WHITESPACE.replace_all(&name, " ").trim().to_string();
    name = name.to_uppercase();

    if name.contains("FINNAIR") {
        name = "FINNAIR".to_string();
    }
    if name.contains("WOLT") {
        name = "WOLT".to_string();
    }
    if name.contains("ITUNES.COM") {
        name = "APPLE ITUNES".to_string();
    }

    for (pattern, replacement) in REPLACE_PAIRS.iter() {
        name = name.replace(pattern, replacement);
    }

    replace_from_start(&mut name, "CHF ", "");
    replace_from_start(&mut name, "CHF", "");
    replace_from_start(&mut name, "WWW.", "");
    replace_from_start(&mut name, "MOB.PAY", "MOBILEPAY");

    for prefix in REPLACE_WITH_START.iter() {
        if name.starts_with(prefix) {
            name = prefix.to_string();
        }
    }

    name = RE_WHITESPACE.replace_all(&name, " ").trim().to_string();
    name
}

/// Helper method to remove a given pattern from the start of a string.
fn replace_from_start(name: &mut String, pattern: &str, replacement: &str) {
    if name.starts_with(pattern) {
        *name = name.replacen(pattern, replacement, 1)
    }
}

/// Convert Finnish currency value strings using a comma as the decimal separator to float.
fn format_sum(value: &str) -> Result<f64> {
    value
        .trim()
        .replace(',', ".")
        .parse::<f64>()
        .context(format!("Failed to parse sum as float: {}", value))
}

/// Print item totals and some statistics.
fn print_statistics(items: &[VisaItem], totals: &[(String, f64)], num_files: usize, verbose: bool) {
    let total_sum: f64 = items.iter().map(|item| item.sum).sum();
    let count = items.len() as f64;
    let average = if count > 0.0 { total_sum / count } else { 0.0 };

    println!("Average items per file: {:.1}", items.len() / num_files);
    println!("Total sum: {:.2}€", total_sum);
    println!("Average sum: {:.2}€", average);
    println!("Unique names: {}", totals.len());

    if verbose {
        let num_to_print: usize = 10;
        let max_name_length = totals[..num_to_print]
            .iter()
            .map(|(name, _)| name.chars().count())
            .max()
            .unwrap_or(20)
            + 1;

        println!("\n{}", format!("Top {num_to_print} totals:").bold());
        for (name, sum) in totals[..num_to_print].iter() {
            println!("{:width$}   {:.2}€", format!("{name}:"), sum, width = max_name_length);
        }
    }
    println!();
}

/// Split item line to separate parts.
fn split_item_text(input: &str) -> (String, String, String) {
    // Split the string at the first whitespace
    let mut parts = input.splitn(2, ' ');
    let first_part = parts.next().unwrap_or("").trim().to_string();
    let remainder = parts.next().unwrap_or("");
    let cleaned_remainder = clean_whitespaces(remainder);
    let (second_part, third_part) = split_from_last_whitespace(&cleaned_remainder);

    (first_part, second_part, third_part)
}

/// Split to two parts from the last whitespace character.
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
        format!("Writing data to CSV:   {}", output_file.display()).green()
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

/// Save parsed data to an Excel file.
fn write_to_excel(items: &[VisaItem], totals: &[(String, f64)], output_path: &Path) -> Result<()> {
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
        format!("Writing data to Excel: {}", output_file.display()).green()
    );
    let mut workbook = Workbook::new();
    let sheet = workbook.add_worksheet().set_name("VISA")?;
    let header_format = Format::new()
        .set_bold()
        .set_border(FormatBorder::Thin)
        .set_background_color("C6E0B4");

    sheet.serialize_headers_with_format::<VisaItem>(0, 0, &items[0], &header_format)?;
    sheet.serialize(&items)?;
    sheet.autofit();

    let dj_sheet = workbook.add_worksheet().set_name("DJ")?;
    let sum_format = Format::new().set_align(FormatAlign::Right).set_num_format("0,00");
    dj_sheet.serialize_headers_with_format::<VisaItem>(0, 0, &items[0], &header_format)?;
    let mut row: usize = 1;
    for item in items.iter() {
        // Filter out common non-DJ items
        if FILTER_PREFIXES.iter().any(|&prefix| item.name.starts_with(prefix)) {
            continue;
        }
        dj_sheet.write_string(row as RowNum, 0, item.finnish_date())?;
        dj_sheet.write_string(row as RowNum, 1, item.name.clone())?;
        dj_sheet.write_string_with_format(row as RowNum, 2, item.finnish_sum(), &sum_format)?;
        row += 1;
    }
    dj_sheet.autofit();

    let totals_sheet = workbook.add_worksheet().set_name("TOTALS")?;
    totals_sheet.write_string_with_format(0, 0, "Name", &header_format)?;
    totals_sheet.write_string_with_format(0, 1, "Total sum", &header_format)?;
    row = 1;
    for (name, sum) in totals.iter() {
        totals_sheet.write_string(row as RowNum, 0, name)?;
        totals_sheet.write_string_with_format(
            row as RowNum,
            1,
            format!("{:.2}", sum).replace('.', ","),
            &sum_format,
        )?;
        row += 1;
    }
    totals_sheet.autofit();

    if output_file.exists() {
        if let Err(e) = std::fs::remove_file(&output_file) {
            eprintln!("{}", format!("Failed to remove existing xlsx file: {e}").red())
        }
    }
    workbook.save(output_file)?;
    Ok(())
}

impl VisaItem {
    /// Float value formatted with a comma as the decimal separator.
    pub fn finnish_sum(&self) -> String {
        format!("{:.2}", self.sum).replace('.', ",")
    }

    /// Date in format "yyyy.mm.dd"
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
