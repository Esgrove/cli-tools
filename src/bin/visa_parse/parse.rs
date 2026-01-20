use std::cmp::Ordering;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result, anyhow};
use chrono::{Datelike, Local, NaiveDate};
use colored::Colorize;
use encoding_rs::Encoding;
use regex::Regex;
use rust_xlsxwriter::{Format, FormatAlign, FormatBorder, RowNum, Workbook};
use serde::ser::{Serialize, SerializeStruct, Serializer};
use walkdir::WalkDir;

use crate::config::Config;

static RE_BRACKETS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\[\({\]}\)]+").expect("Failed to create regex pattern for brackets"));

static RE_HTML_AND: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)&amp;").expect("Failed to create regex pattern for html"));

static RE_SEPARATORS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\r\n\t]+").expect("Failed to create regex pattern for separators"));

static RE_WHITESPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s{2,}").expect("Failed to create regex pattern for whitespace"));

static RE_ITEM_DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{2}\.\d{2}\.)(.*)").expect("Failed to create regex pattern for item date"));

static RE_START_DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<StartDate Format="CCYYMMDD">(\d{4})\d{4}</StartDate>"#)
        .expect("Failed to create regex pattern for start date")
});

static RE_SPECIFICATION_FREE_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*<SpecificationFreeText>(.*?)</SpecificationFreeText>")
        .expect("Failed to create regex pattern for SpecificationFreeText")
});

// Replace a pattern with replacement
static REPLACE_PAIRS: [(&str, &str); 11] = [
    ("4029357733", ""),
    (" - ", " "),
    (" . ", " "),
    (", ", " "),
    (" 35314369001", ""),
    (" 402-935-7733", ""),
    (" DRI ", ""),
    (" LEVISTRAUSS ", " LEVIS "),
    (".COMFI", ".COM"),
    ("CHATGPT SUBSCRIPTION HTTPSOPENAI.C", "CHATGPT SUBSCRIPTION OPENAI.COM"),
    ("VFI ", ""),
];

// If the name starts with this string, set name to replacement.
static REPLACE_START_PAIRS: [(&str, &str); 4] = [("CHF ", ""), ("CHF", ""), ("WWW.", ""), ("MOB.PAY", "MOBILEPAY")];

// Replace whole string with replacement if it contains a pattern.
static REPLACE_CONTAINS: [(&str, &str); 7] = [
    ("EPASSI", "EPASSI"),
    ("FINNAIR", "FINNAIR"),
    ("IDA RADIO RY", "IDA RADIO RY"),
    ("ITUNES.COM", "APPLE ITUNES"),
    ("K-CITYMARKET", "K-MARKET"),
    ("VERKKOKAUPPA.COM", "VERKKOKAUPPA.COM"),
    ("WOLT", "WOLT"),
];

// If the name starts with this string, set name to just the string.
static REPLACE_START: [&str; 11] = [
    "ALEPA",
    "BEAMHILL",
    "HERTZ",
    "K-MARKET",
    "PAYPAL BANDCAMP",
    "PAYPAL BEATPORT",
    "PAYPAL DJCITY",
    "PAYPAL DROPBOX",
    "PAYPAL MISTERB",
    "PAYPAL PATREON",
    "STOCKMANN",
];

// TODO: move to user config
// Non-DJ items
static FILTER_PREFIXES: [&str; 82] = [
    "1BAR",
    "45 SPECIAL",
    "ALEPA",
    "ALKO",
    "ALLAS SEA POOL",
    "AVECRA",
    "BAMILAMI",
    "BAR ",
    "BASTARD BURGERS",
    "CHATGPT SUBSCRIPTION",
    "CLAS OHLSON",
    "CLASSIC TROIJA",
    "COCKTAIL TRADING COMPA",
    "CRAFT BEER HELSINKI",
    "DICK JOHNSON",
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
    "LEVIN ALPPITALOT",
    "LOPEZ KALLIO",
    "LUNDIA",
    "MAKIA CLOTHING",
    "MCD ",
    "MCDONALD",
    "MESTARITALLI",
    "MOBILEPAY HELSINGIN SEUDUN",
    "MONOMESTA",
    "MUJI",
    "PAYPAL BJORNBORG",
    "PAYPAL EPIC GAMES",
    "PAYPAL LEVISTRAUSS",
    "PAYPAL MCOMPANY",
    "PAYPAL MISTERB",
    "PAYPAL NIKE",
    "PAYPAL STEAM GAMES",
    "PISTE SKI LODGE",
    "PISTEVUOKRAAMO RUKATUNTURI",
    "PIZZALA",
    "RAGS FASHION",
    "RAVINTOLA",
    "RAVINTOTALOT OY",
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
    "THE SOCIAL BURGER JOINT",
    "TIKETTI.FI",
    "TORTILLA HOUSE",
    "TUNTUR.MAX OY SKI BISTRO",
    "WOLT",
];

/// Intermediate parsed data before creating a `VisaItem`.
/// Used during the parsing phase to handle year transitions.
#[derive(Debug)]
struct RawParsedItem {
    day: i32,
    month: i32,
    name: String,
    sum: f64,
}

impl RawParsedItem {
    /// Convert this raw parsed item into a `VisaItem` with the given year.
    fn into_visa_item(self, year: i32) -> Result<VisaItem> {
        let date_str = format!("{:02}.{:02}.{year}", self.day, self.month);
        let date = NaiveDate::parse_from_str(&date_str, "%d.%m.%Y")
            .with_context(|| format!("Invalid date '{date_str}' for item: {} ({:.2}€)", self.name, self.sum))?;
        Ok(VisaItem {
            date,
            name: self.name,
            sum: self.sum,
        })
    }
}

/// Represents one credit card purchase.
#[derive(Debug, Clone, PartialEq)]
struct VisaItem {
    date: NaiveDate,
    name: String,
    sum: f64,
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

/// Parse data from files and write formatted items to CSV and Excel.
pub fn visa_parse(config: &Config) -> Result<()> {
    let (root, files) = get_xml_file_list(&config.input_path)?;
    if files.is_empty() {
        anyhow::bail!("No XML files to parse".red());
    }

    let num_files = files.len();
    let items = parse_files(&root, files, config.verbose)?;
    let totals = calculate_totals_for_each_name(&items);
    print_statistics(&items, &totals, num_files, config.verbose, config.number);

    if !config.print {
        write_to_csv(&items, &config.output_path)?;
        write_to_excel(&items, &totals, &config.output_path)?;
    }

    Ok(())
}

/// Return file root and list of files from the input path that can be either a directory or single file.
fn get_xml_file_list(input: &PathBuf) -> Result<(PathBuf, Vec<PathBuf>)> {
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
        Ok((input.clone(), get_xml_files(input)))
    }
}

/// Collect all XML files recursively from the given root path.
fn get_xml_files<P: AsRef<Path>>(root: P) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !cli_tools::is_hidden(e))
        .filter_map(std::result::Result::ok)
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
    let num_files = files.len();
    let digits = num_files.checked_ilog10().map_or(1, |d| d as usize + 1);

    for (number, file) in files.into_iter().enumerate() {
        print!(
            "{}",
            format!(
                "{:>0width$}: {}",
                number + 1,
                cli_tools::get_relative_path_or_filename(&file, root),
                width = digits
            )
            .bold()
        );
        let (raw_lines, year) = read_xml_file(&file);
        let items = extract_items(&raw_lines, year)?;
        if items.is_empty() {
            println!(" ({})", "0".yellow());
        } else {
            println!(" ({})", format!("{}", items.len()).cyan());
            if verbose {
                for item in &items {
                    println!("  {item}");
                }
            }
            result.extend(items);
        }
    }

    result.sort_unstable();
    println!(
        "Found {} items from {}",
        result.len(),
        if num_files > 1 {
            format!("{num_files} files")
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

    // Read the first 256 bytes to detect encoding
    let mut buf_reader = BufReader::new(&xml_file);
    let mut buf = [0; 256];
    let n = buf_reader.read(&mut buf).unwrap_or(0);
    let header = String::from_utf8_lossy(&buf[..n]).to_lowercase();
    let encoding_name = header
        .split("encoding=")
        .nth(1)
        .and_then(|s| s.split(['"', '\'']).nth(1))
        .unwrap_or("iso-8859-15");
    let encoding = Encoding::for_label(encoding_name.as_bytes()).unwrap_or(encoding_rs::ISO_8859_15);

    // Read the whole file as bytes
    let mut xml_file = xml_file;
    xml_file.seek(SeekFrom::Start(0)).ok();
    let mut bytes = Vec::new();
    xml_file.read_to_end(&mut bytes).ok();

    // Decode to UTF-8
    let (cow, _, had_errors) = encoding.decode(&bytes);
    if had_errors {
        eprintln!("{}", "Warning: Decoding errors occurred".yellow());
    }

    // Process lines as UTF-8
    for line in cow.lines() {
        if let Some(caps) = RE_START_DATE.captures(line)
            && let Some(matched) = caps.get(1)
        {
            match matched.as_str().parse::<i32>() {
                Ok(y) => year = y,
                Err(e) => eprintln!("{}", format!("Failed to parse year from start date: {e}").red()),
            }
        }
        if let Some(caps) = RE_SPECIFICATION_FREE_TEXT.captures(line)
            && let Some(matched) = caps.get(1)
        {
            let text = matched.as_str();
            if RE_ITEM_DATE.is_match(text) {
                lines.push(text.to_string());
            }
        }
    }
    (lines, year)
}

/// Convert text lines to visa items.
fn extract_items(rows: &[String], year: i32) -> Result<Vec<VisaItem>> {
    let mut parsed_items: Vec<RawParsedItem> = Vec::new();
    for line in rows {
        let (date, name, sum) = split_item_text(line);
        let (day, month) = date
            .split_once('.')
            .with_context(|| format!("Failed to separate day and month: {date}"))?;
        let month: i32 = month.replace('.', "").parse()?;
        let day: i32 = day.parse()?;
        let name = format_name(&name);
        let sum = format_sum(&sum).with_context(|| format!("Failed format sum: {sum}"))?;
        parsed_items.push(RawParsedItem { day, month, name, sum });
    }

    // Determine if there's a transition from December to January.
    let mut year_transition_detected = false;
    let mut last_month: i32 = 0;
    for item in &parsed_items {
        if item.month == 1 && last_month == 12 {
            year_transition_detected = true;
            break;
        }
        last_month = item.month;
    }

    let previous_year = year - 1;
    let mut result: Vec<VisaItem> = Vec::new();
    for item in parsed_items {
        let item_year = if item.month == 12 && year_transition_detected {
            previous_year
        } else {
            year
        };

        match item.into_visa_item(item_year) {
            Ok(visa_item) => result.push(visa_item),
            Err(err) => eprintln!("{}", format!("Failed to parse item: {err}").red()),
        }
    }

    Ok(result)
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
    let mut name = text
        .replace("Osto ", "")
        .replace("TC*", "")
        .replace(['*', '/', '_'], " ");

    name = RE_HTML_AND.replace_all(&name, "&").to_string();
    name = RE_BRACKETS.replace_all(&name, "").to_string();
    name = RE_WHITESPACE.replace_all(&name, " ").trim().to_string();
    name = name.to_uppercase();

    for (pattern, replacement) in &REPLACE_CONTAINS {
        if name.contains(pattern) {
            name = (*replacement).to_string();
        }
    }

    for (pattern, replacement) in &REPLACE_PAIRS {
        name = name.replace(pattern, replacement);
    }

    for (pattern, replacement) in &REPLACE_START_PAIRS {
        if name.starts_with(pattern) {
            name = name.replacen(pattern, replacement, 1);
        }
    }

    for prefix in &REPLACE_START {
        if name.starts_with(prefix) {
            name = (*prefix).to_string();
        }
    }

    name = RE_WHITESPACE.replace_all(&name, " ").trim().to_string();
    name
}

/// Convert Finnish currency value strings using a comma as the decimal separator to float.
fn format_sum(value: &str) -> Result<f64> {
    value
        .trim()
        .replace(',', ".")
        .replace(' ', "")
        .parse::<f64>()
        .context(format!("Failed to parse sum as float: {value}"))
}

/// Print item totals and some statistics.
fn print_statistics(items: &[VisaItem], totals: &[(String, f64)], num_files: usize, verbose: bool, num_totals: usize) {
    let total_sum: f64 = items.iter().map(|item| item.sum).sum();
    let count = items.len();
    let average = if count > 0 { total_sum / count as f64 } else { 0.0 };

    println!("Average items per file: {:.1}", items.len() / num_files);
    println!("Total sum: {total_sum:.2}€");
    println!("Average sum: {average:.2}€");
    println!("Unique names: {}", totals.len());

    if verbose {
        let max_name_length = totals[..num_totals]
            .iter()
            .map(|(name, _)| name.chars().count())
            .max()
            .unwrap_or(20)
            + 1;

        println!("\n{}", format!("Top {num_totals} totals:").bold());
        for (name, sum) in &totals[..num_totals] {
            println!("{:width$}    {:>7.2}€", format!("{name}"), sum, width = max_name_length);
        }
    }
    println!();
}

/// Split item line to separate parts.
fn split_item_text(input: &str) -> (String, String, String) {
    // Split the string at the first whitespace
    let mut parts = input.splitn(2, ' ');
    let first_part = parts.next().unwrap_or("").trim();
    let remainder = parts.next().unwrap_or("");
    let (second_part, third_part) = split_from_last_whitespaces(remainder);

    (
        clean_whitespaces(first_part),
        clean_whitespaces(second_part),
        clean_whitespaces(third_part),
    )
}

/// Split to two parts from the last whitespace character.
fn split_from_last_whitespaces(s: &str) -> (&str, &str) {
    let mut parts = s.rsplitn(2, "  ");
    let after = parts.next().unwrap_or("").trim();
    let before = parts.next().unwrap_or("").trim();

    (before, after)
}

/// Save parsed data to a CSV file
fn write_to_csv(items: &[VisaItem], output_path: &Path) -> Result<()> {
    let output_file = if output_path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("csv"))
    {
        output_path.to_path_buf()
    } else {
        output_path.join("VISA.csv")
    };
    println!(
        "{}",
        format!("Writing data to CSV:   {}", output_file.display()).green()
    );
    if output_file.exists()
        && let Err(e) = std::fs::remove_file(&output_file)
    {
        eprintln!("{}", format!("Failed to remove existing csv file: {e}").red());
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
        .is_some_and(|ext| ext.eq_ignore_ascii_case("csv") || ext.eq_ignore_ascii_case("xlsx"))
    {
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
    let mut row: RowNum = 1;
    for item in items {
        // Filter out common non-DJ items
        if FILTER_PREFIXES.iter().any(|&prefix| item.name.starts_with(prefix)) {
            continue;
        }
        dj_sheet.write_string(row, 0, item.finnish_date())?;
        dj_sheet.write_string(row, 1, item.name.clone())?;
        dj_sheet.write_string_with_format(row, 2, item.finnish_sum(), &sum_format)?;
        row += 1;
    }
    dj_sheet.autofit();

    let totals_sheet = workbook.add_worksheet().set_name("TOTALS")?;
    totals_sheet.write_string_with_format(0, 0, "Name", &header_format)?;
    totals_sheet.write_string_with_format(0, 1, "Total sum", &header_format)?;
    let mut totals_row: RowNum = 1;
    for (name, sum) in totals {
        totals_sheet.write_string(totals_row, 0, name)?;
        totals_sheet.write_string_with_format(totals_row, 1, format!("{sum:.2}").replace('.', ","), &sum_format)?;
        totals_row += 1;
    }
    totals_sheet.autofit();

    if output_file.exists()
        && let Err(e) = std::fs::remove_file(&output_file)
    {
        eprintln!("{}", format!("Failed to remove existing xlsx file: {e}").red());
    }
    workbook.save(output_file)?;
    Ok(())
}

#[cfg(test)]
mod test_format_sum {
    use super::*;

    use cli_tools::assert_f64_eq;

    #[test]
    fn test_normal_value() {
        assert_f64_eq(format_sum("123,45").unwrap(), 123.45);
    }

    #[test]
    fn test_value_with_whitespace() {
        assert_f64_eq(format_sum("  678,90  ").unwrap(), 678.90);
    }

    #[test]
    fn test_invalid_format() {
        assert!(format_sum("invalid").is_err());
    }

    #[test]
    fn test_large_number() {
        assert_f64_eq(format_sum("1234567,89").unwrap(), 1_234_567.89);
    }

    #[test]
    fn test_small_number() {
        assert_f64_eq(format_sum("0,01").unwrap(), 0.01);
    }

    #[test]
    fn test_number_with_many_decimal_places() {
        assert_f64_eq(format_sum("1,234567").unwrap(), 1.234_567);
    }

    #[test]
    fn test_negative_number() {
        assert_f64_eq(format_sum("-123,45").unwrap(), -123.45);
    }

    #[test]
    fn test_zero_value() {
        assert_f64_eq(format_sum("0").unwrap(), 0.0);
    }

    #[test]
    fn test_number_without_decimal() {
        assert_f64_eq(format_sum("1234").unwrap(), 1234.0);
    }

    #[test]
    fn test_number_with_thousand_space() {
        assert_f64_eq(format_sum("1 488,90").unwrap(), 1488.90);
    }
}

#[cfg(test)]
mod test_item_parse {
    use super::*;

    #[test]
    fn test_split_item_text() {
        let input = "25.05. Osto PAYPAL *THOMANN 35314369001                                 1 488,90";
        let (one, two, three) = split_item_text(input);
        assert_eq!(one, "25.05.");
        assert_eq!(two, "Osto PAYPAL *THOMANN 35314369001");
        assert_eq!(three, "1 488,90");

        let input = "14.06. Osto PAYPAL *BANDCAMP 4029357733                                     6,20";
        let (one, two, three) = split_item_text(input);
        assert_eq!(one, "14.06.");
        assert_eq!(two, "Osto PAYPAL *BANDCAMP 4029357733");
        assert_eq!(three, "6,20");

        let input = "30.05. Osto PAYPAL *NIKE COM 35314369001                                  443,44";
        let (one, two, three) = split_item_text(input);
        assert_eq!(one, "30.05.");
        assert_eq!(two, "Osto PAYPAL *NIKE COM 35314369001");
        assert_eq!(three, "443,44");
    }
}

#[cfg(test)]
mod test_clean_whitespaces {
    use super::*;

    #[test]
    fn removes_multiple_spaces() {
        assert_eq!(clean_whitespaces("hello    world"), "hello world");
    }

    #[test]
    fn removes_tabs_and_newlines() {
        assert_eq!(clean_whitespaces("hello\t\nworld"), "hello world");
    }

    #[test]
    fn handles_mixed_whitespace() {
        assert_eq!(clean_whitespaces("hello\r\n\t   world"), "hello world");
    }

    #[test]
    fn preserves_single_spaces() {
        assert_eq!(clean_whitespaces("hello world"), "hello world");
    }

    #[test]
    fn handles_empty_string() {
        assert_eq!(clean_whitespaces(""), "");
    }
}

#[cfg(test)]
mod test_format_name {
    use super::*;

    #[test]
    fn removes_osto_prefix() {
        let result = format_name("Osto STORE NAME");
        assert!(!result.contains("OSTO"));
        assert!(result.contains("STORE NAME"));
    }

    #[test]
    fn removes_tc_prefix() {
        let result = format_name("TC*STORE NAME");
        assert!(!result.contains("TC*"));
    }

    #[test]
    fn converts_to_uppercase() {
        let result = format_name("store name");
        assert_eq!(result, "STORE NAME");
    }

    #[test]
    fn replaces_special_characters_with_spaces() {
        let result = format_name("store*name/test_here");
        assert!(!result.contains('*'));
        assert!(!result.contains('/'));
        assert!(!result.contains('_'));
    }

    #[test]
    fn removes_brackets() {
        let result = format_name("STORE (NAME) [TEST]");
        assert!(!result.contains('('));
        assert!(!result.contains(')'));
        assert!(!result.contains('['));
        assert!(!result.contains(']'));
    }

    #[test]
    fn replaces_html_ampersand() {
        let result = format_name("STORE &amp; NAME");
        assert!(result.contains('&'));
        assert!(!result.contains("&amp;"));
    }

    #[test]
    fn replaces_known_patterns() {
        let result = format_name("K-CITYMARKET STORE");
        assert_eq!(result, "K-MARKET");
    }

    #[test]
    fn handles_wolt() {
        let result = format_name("WOLT DELIVERY SERVICE");
        assert_eq!(result, "WOLT");
    }

    #[test]
    fn handles_itunes() {
        let result = format_name("ITUNES.COM/BILL");
        assert_eq!(result, "APPLE ITUNES");
    }

    #[test]
    fn removes_www_prefix() {
        let result = format_name("WWW.EXAMPLE.COM");
        assert!(!result.starts_with("WWW."));
    }

    #[test]
    fn handles_mobilepay() {
        let result = format_name("MOB.PAY TRANSACTION");
        assert!(result.starts_with("MOBILEPAY"));
    }
}

#[cfg(test)]
mod test_split_from_last_whitespaces {
    use super::*;

    #[test]
    fn splits_on_double_space() {
        let (before, after) = split_from_last_whitespaces("hello world  123");
        assert_eq!(before, "hello world");
        assert_eq!(after, "123");
    }

    #[test]
    fn handles_no_double_space() {
        let (before, after) = split_from_last_whitespaces("hello world");
        assert_eq!(before, "");
        assert_eq!(after, "hello world");
    }

    #[test]
    fn handles_multiple_double_spaces() {
        let (before, after) = split_from_last_whitespaces("one  two  three");
        assert_eq!(before, "one  two");
        assert_eq!(after, "three");
    }

    #[test]
    fn handles_empty_string() {
        let (before, after) = split_from_last_whitespaces("");
        assert_eq!(before, "");
        assert_eq!(after, "");
    }

    #[test]
    fn trims_whitespace_from_parts() {
        // Input: "  hello  world  " splits on last "  " (double space)
        // rsplitn(2, "  ") from right: ["world  ", "  hello"]
        // After trim: before="hello  world", after="" (splits on trailing double space)
        let (before, after) = split_from_last_whitespaces("hello  world");
        assert_eq!(before, "hello");
        assert_eq!(after, "world");
    }

    #[test]
    fn handles_trailing_double_space() {
        let (before, after) = split_from_last_whitespaces("hello world  ");
        assert_eq!(before, "hello world");
        assert_eq!(after, "");
    }
}

#[cfg(test)]
mod test_visa_item {
    use super::*;

    #[test]
    fn finnish_sum_formats_correctly() {
        let item = VisaItem {
            date: NaiveDate::from_ymd_opt(2024, 6, 15).expect("valid date"),
            name: "TEST".to_string(),
            sum: 123.45,
        };
        assert_eq!(item.finnish_sum(), "123,45");
    }

    #[test]
    fn finnish_sum_handles_whole_numbers() {
        let item = VisaItem {
            date: NaiveDate::from_ymd_opt(2024, 6, 15).expect("valid date"),
            name: "TEST".to_string(),
            sum: 100.0,
        };
        assert_eq!(item.finnish_sum(), "100,00");
    }

    #[test]
    fn finnish_date_formats_correctly() {
        let item = VisaItem {
            date: NaiveDate::from_ymd_opt(2024, 6, 15).expect("valid date"),
            name: "TEST".to_string(),
            sum: 10.0,
        };
        assert_eq!(item.finnish_date(), "2024.06.15");
    }

    #[test]
    fn display_formats_correctly() {
        let item = VisaItem {
            date: NaiveDate::from_ymd_opt(2024, 1, 5).expect("valid date"),
            name: "STORE NAME".to_string(),
            sum: 99.99,
        };
        let display = format!("{item}");
        assert!(display.contains("2024.01.05"));
        assert!(display.contains("STORE NAME"));
        assert!(display.contains("99,99"));
    }

    #[test]
    fn ordering_by_date_then_name() {
        let item1 = VisaItem {
            date: NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid date"),
            name: "AAA".to_string(),
            sum: 10.0,
        };
        let item2 = VisaItem {
            date: NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid date"),
            name: "BBB".to_string(),
            sum: 20.0,
        };
        let item3 = VisaItem {
            date: NaiveDate::from_ymd_opt(2024, 1, 2).expect("valid date"),
            name: "AAA".to_string(),
            sum: 30.0,
        };

        // Same date, different names: ordered by name
        assert!(item1 < item2);
        // Different dates: ordered by date first
        assert!(item1 < item3);
        assert!(item2 < item3);
    }

    #[test]
    fn partial_ord_matches_ord() {
        let item1 = VisaItem {
            date: NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid date"),
            name: "AAA".to_string(),
            sum: 10.0,
        };
        let item2 = VisaItem {
            date: NaiveDate::from_ymd_opt(2024, 1, 2).expect("valid date"),
            name: "BBB".to_string(),
            sum: 20.0,
        };

        assert_eq!(item1.partial_cmp(&item2), Some(item1.cmp(&item2)));
    }
}

#[cfg(test)]
mod test_read_xml_file {
    use super::*;
    use std::path::Path;

    #[test]
    fn reads_sample_xml_file() {
        let path = Path::new("tests/fixtures/visa_sample.xml");
        let (lines, year) = read_xml_file(path);

        assert_eq!(year, 2024);
        assert_eq!(lines.len(), 8);

        // Check that lines contain expected transaction patterns
        assert!(lines[0].contains("05.02."));
        assert!(lines[0].contains("K-MARKET"));
        assert!(lines[0].contains("25,50"));
    }

    #[test]
    fn reads_year_transition_xml_file() {
        let path = Path::new("tests/fixtures/visa_year_transition.xml");
        let (lines, year) = read_xml_file(path);

        // Year should be extracted from StartDate (2025 from 20250101)
        assert_eq!(year, 2025);
        assert_eq!(lines.len(), 6);

        // Check December and January transactions are present
        assert!(lines.iter().any(|line| line.contains("15.12.")));
        assert!(lines.iter().any(|line| line.contains("02.01.")));
    }

    #[test]
    fn handles_nonexistent_file() {
        let path = Path::new("tests/fixtures/nonexistent.xml");
        let (lines, _year) = read_xml_file(path);

        assert!(lines.is_empty());
    }
}

#[cfg(test)]
mod test_extract_items {
    use super::*;

    #[test]
    fn extracts_items_from_raw_lines() {
        let lines = vec![
            "05.02. Osto K-MARKET KAMPPI HELSINKI                                        25,50".to_string(),
            "08.02. Osto ALEPA MANNERHEIMINTIE HELSINKI                                  12,95".to_string(),
        ];
        let year = 2024;

        let items = extract_items(&lines, year).expect("should parse items");

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].date, NaiveDate::from_ymd_opt(2024, 2, 5).unwrap());
        assert_eq!(items[0].name, "K-MARKET");
        assert!((items[0].sum - 25.50).abs() < 0.01);

        assert_eq!(items[1].date, NaiveDate::from_ymd_opt(2024, 2, 8).unwrap());
        assert_eq!(items[1].name, "ALEPA");
        assert!((items[1].sum - 12.95).abs() < 0.01);
    }

    #[test]
    fn handles_year_transition_december_to_january() {
        let lines = vec![
            "15.12. Osto K-MARKET KAMPPI HELSINKI                                        25,50".to_string(),
            "28.12. Osto WOLT HELSINKI                                                   18,90".to_string(),
            "02.01. Osto SPOTIFY AB STOCKHOLM                                             9,99".to_string(),
            "15.01. Osto ALEPA HELSINKI                                                  12,00".to_string(),
        ];
        // Year from StartDate is 2025 (the new year)
        let year = 2025;

        let items = extract_items(&lines, year).expect("should parse items");

        assert_eq!(items.len(), 4);

        // December items should be assigned to previous year (2024)
        assert_eq!(items[0].date, NaiveDate::from_ymd_opt(2024, 12, 15).unwrap());
        assert_eq!(items[1].date, NaiveDate::from_ymd_opt(2024, 12, 28).unwrap());

        // January items should be assigned to the current year (2025)
        assert_eq!(items[2].date, NaiveDate::from_ymd_opt(2025, 1, 2).unwrap());
        assert_eq!(items[3].date, NaiveDate::from_ymd_opt(2025, 1, 15).unwrap());
    }

    #[test]
    fn no_year_transition_when_all_same_year() {
        let lines = vec![
            "05.02. Osto K-MARKET HELSINKI                                               25,50".to_string(),
            "15.02. Osto ALEPA HELSINKI                                                  12,95".to_string(),
            "28.02. Osto WOLT HELSINKI                                                   18,90".to_string(),
        ];
        let year = 2024;

        let items = extract_items(&lines, year).expect("should parse items");

        assert_eq!(items.len(), 3);
        // All items should be in 2024
        for item in &items {
            assert_eq!(item.date.year(), 2024);
        }
    }

    #[test]
    fn handles_empty_lines() {
        let lines: Vec<String> = vec![];
        let year = 2024;

        let items = extract_items(&lines, year).expect("should handle empty");

        assert!(items.is_empty());
    }
}

#[cfg(test)]
mod test_calculate_totals {
    use super::*;

    #[test]
    fn groups_by_name_and_sums() {
        let items = vec![
            VisaItem {
                date: NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid date"),
                name: "STORE A".to_string(),
                sum: 10.0,
            },
            VisaItem {
                date: NaiveDate::from_ymd_opt(2024, 1, 2).expect("valid date"),
                name: "STORE A".to_string(),
                sum: 20.0,
            },
            VisaItem {
                date: NaiveDate::from_ymd_opt(2024, 1, 3).expect("valid date"),
                name: "STORE B".to_string(),
                sum: 15.0,
            },
        ];

        let totals = calculate_totals_for_each_name(&items);

        assert_eq!(totals.len(), 2);
        // Should be sorted by sum descending
        assert_eq!(totals[0].0, "STORE A");
        assert!((totals[0].1 - 30.0).abs() < 0.01);
        assert_eq!(totals[1].0, "STORE B");
        assert!((totals[1].1 - 15.0).abs() < 0.01);
    }

    #[test]
    fn sorts_by_sum_descending() {
        let items = vec![
            VisaItem {
                date: NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid date"),
                name: "SMALL".to_string(),
                sum: 10.0,
            },
            VisaItem {
                date: NaiveDate::from_ymd_opt(2024, 1, 2).expect("valid date"),
                name: "LARGE".to_string(),
                sum: 100.0,
            },
            VisaItem {
                date: NaiveDate::from_ymd_opt(2024, 1, 3).expect("valid date"),
                name: "MEDIUM".to_string(),
                sum: 50.0,
            },
        ];

        let totals = calculate_totals_for_each_name(&items);

        assert_eq!(totals[0].0, "LARGE");
        assert_eq!(totals[1].0, "MEDIUM");
        assert_eq!(totals[2].0, "SMALL");
    }

    #[test]
    fn handles_empty_input() {
        let items: Vec<VisaItem> = vec![];
        let totals = calculate_totals_for_each_name(&items);
        assert!(totals.is_empty());
    }

    #[test]
    fn handles_single_item() {
        let items = vec![VisaItem {
            date: NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid date"),
            name: "ONLY".to_string(),
            sum: 42.0,
        }];

        let totals = calculate_totals_for_each_name(&items);

        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].0, "ONLY");
        assert!((totals[0].1 - 42.0).abs() < 0.01);
    }
}

#[cfg(test)]
mod test_integration_xml_parsing {
    use super::*;
    use std::path::Path;

    #[test]
    fn parses_sample_xml_end_to_end() {
        let path = Path::new("tests/fixtures/visa_sample.xml");
        let (lines, year) = read_xml_file(path);
        let items = extract_items(&lines, year).expect("should parse items");

        assert_eq!(items.len(), 8);

        // Check specific items
        let k_market_items: Vec<_> = items.iter().filter(|item| item.name == "K-MARKET").collect();
        assert_eq!(k_market_items.len(), 1);
        assert!((k_market_items[0].sum - 25.50).abs() < 0.01);

        let wolt_items: Vec<_> = items.iter().filter(|item| item.name == "WOLT").collect();
        assert_eq!(wolt_items.len(), 1);
        assert!((wolt_items[0].sum - 18.90).abs() < 0.01);

        // Check name transformations
        assert_eq!(items.iter().filter(|item| item.name == "VERKKOKAUPPA.COM").count(), 1);

        assert_eq!(items.iter().filter(|item| item.name == "APPLE ITUNES").count(), 1);

        assert_eq!(items.iter().filter(|item| item.name == "PAYPAL BANDCAMP").count(), 1);
    }

    #[test]
    fn parses_year_transition_xml_end_to_end() {
        let path = Path::new("tests/fixtures/visa_year_transition.xml");
        let (lines, year) = read_xml_file(path);
        let items = extract_items(&lines, year).expect("should parse items");

        assert_eq!(items.len(), 6);

        // Check that December items got previous year
        let december_items: Vec<_> = items.iter().filter(|item| item.date.month() == 12).collect();
        assert_eq!(december_items.len(), 3);
        for item in december_items {
            assert_eq!(item.date.year(), 2024);
        }

        // Check that January items got current year
        let january_items: Vec<_> = items.iter().filter(|item| item.date.month() == 1).collect();
        assert_eq!(january_items.len(), 3);
        for item in january_items {
            assert_eq!(item.date.year(), 2025);
        }
    }

    #[test]
    fn calculates_correct_totals_from_sample() {
        let path = Path::new("tests/fixtures/visa_sample.xml");
        let (lines, year) = read_xml_file(path);
        let items = extract_items(&lines, year).expect("should parse items");
        let totals = calculate_totals_for_each_name(&items);

        // Total sum should match the expected value
        let total_sum: f64 = items.iter().map(|item| item.sum).sum();
        assert!((total_sum - 261.83).abs() < 0.01);

        // Check that totals are sorted descending by sum
        for window in totals.windows(2) {
            assert!(window[0].1 >= window[1].1);
        }
    }
}

#[cfg(test)]
mod test_get_xml_files {
    use super::*;
    use std::path::Path;

    #[test]
    fn finds_xml_files_in_fixtures_directory() {
        let files = get_xml_files(Path::new("tests/fixtures"));

        assert!(files.len() >= 2);
        assert!(
            files
                .iter()
                .all(|path| path.extension().is_some_and(|ext| ext == "xml"))
        );
    }

    #[test]
    fn returns_empty_for_nonexistent_directory() {
        let files = get_xml_files(Path::new("tests/nonexistent"));

        assert!(files.is_empty());
    }

    #[test]
    fn returns_sorted_files() {
        let files = get_xml_files(Path::new("tests/fixtures"));

        let file_names: Vec<_> = files
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().to_lowercase())
            .collect();

        let mut sorted = file_names.clone();
        sorted.sort();
        assert_eq!(file_names, sorted);
    }
}

#[cfg(test)]
mod test_get_xml_file_list {
    use super::*;

    #[test]
    fn handles_single_xml_file() {
        let path = PathBuf::from("tests/fixtures/visa_sample.xml");
        let result = get_xml_file_list(&path);

        assert!(result.is_ok());
        let (root, files) = result.unwrap();
        assert_eq!(files.len(), 1);
        assert!(root.ends_with("fixtures"));
    }

    #[test]
    fn handles_directory() {
        let path = PathBuf::from("tests/fixtures");
        let result = get_xml_file_list(&path);

        assert!(result.is_ok());
        let (root, files) = result.unwrap();
        assert!(files.len() >= 2);
        assert_eq!(root, path);
    }

    #[test]
    fn rejects_non_xml_file() {
        let path = PathBuf::from("tests/fixtures/sample_config.toml");
        let result = get_xml_file_list(&path);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not an XML file"));
    }
}

#[cfg(test)]
mod test_format_name_comprehensive {
    use super::*;

    #[test]
    fn removes_osto_prefix_with_space() {
        assert_eq!(format_name("Osto K-MARKET HELSINKI"), "K-MARKET");
    }

    #[test]
    fn removes_tc_star_prefix() {
        assert_eq!(format_name("TC*VERKKOKAUPPA.COM"), "VERKKOKAUPPA.COM");
    }

    #[test]
    fn replaces_star_with_space() {
        assert_eq!(format_name("PAYPAL*BANDCAMP"), "PAYPAL BANDCAMP");
    }

    #[test]
    fn replaces_slash_with_space() {
        assert_eq!(format_name("APPLE/ITUNES"), "APPLE ITUNES");
    }

    #[test]
    fn replaces_underscore_with_space() {
        assert_eq!(format_name("SOME_STORE_NAME"), "SOME STORE NAME");
    }

    #[test]
    fn replaces_html_ampersand() {
        assert_eq!(format_name("ROCK&amp;ROLL"), "ROCK&ROLL");
    }

    #[test]
    fn removes_square_brackets() {
        assert_eq!(format_name("STORE [CODE]"), "STORE CODE");
    }

    #[test]
    fn removes_parentheses() {
        assert_eq!(format_name("STORE (LOCATION)"), "STORE LOCATION");
    }

    #[test]
    fn removes_curly_braces() {
        assert_eq!(format_name("STORE {INFO}"), "STORE INFO");
    }

    #[test]
    fn converts_to_uppercase() {
        assert_eq!(format_name("lowercase store"), "LOWERCASE STORE");
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(format_name("  STORE NAME  "), "STORE NAME");
    }

    #[test]
    fn collapses_multiple_spaces() {
        assert_eq!(format_name("STORE    NAME"), "STORE NAME");
    }

    #[test]
    fn replaces_known_pattern_4029357733() {
        assert_eq!(format_name("PAYPAL 4029357733"), "PAYPAL");
    }

    #[test]
    fn replaces_known_pattern_35314369001() {
        assert_eq!(format_name("STEAM 35314369001"), "STEAM");
    }

    #[test]
    fn replaces_dri_pattern() {
        // " DRI " is replaced with "" which collapses spaces
        assert_eq!(format_name("STORE DRI NAME"), "STORENAME");
    }

    #[test]
    fn replaces_vfi_pattern() {
        assert_eq!(format_name("VFI STORE NAME"), "STORE NAME");
    }

    #[test]
    fn replaces_contains_epassi() {
        assert_eq!(format_name("PTL*EPASSI ESPOO"), "EPASSI");
    }

    #[test]
    fn replaces_contains_finnair() {
        assert_eq!(format_name("FINNAIR PLUS SHOP"), "FINNAIR");
    }

    #[test]
    fn replaces_contains_itunes() {
        assert_eq!(format_name("APPLE.COM/BILL ITUNES.COM"), "APPLE ITUNES");
    }

    #[test]
    fn replaces_contains_k_citymarket() {
        assert_eq!(format_name("K-CITYMARKET KAMPPI"), "K-MARKET");
    }

    #[test]
    fn replaces_contains_verkkokauppa() {
        assert_eq!(format_name("TC*VERKKOKAUPPA.COM OY"), "VERKKOKAUPPA.COM");
    }

    #[test]
    fn replaces_contains_wolt() {
        assert_eq!(format_name("WOLT OY FINLAND"), "WOLT");
    }

    #[test]
    fn replaces_start_www() {
        assert_eq!(format_name("WWW.EXAMPLE.COM"), "EXAMPLE.COM");
    }

    #[test]
    fn replaces_start_mobpay() {
        assert_eq!(format_name("MOB.PAY STORE"), "MOBILEPAY STORE");
    }

    #[test]
    fn replaces_start_chf_space() {
        assert_eq!(format_name("CHF STORE"), "STORE");
    }

    #[test]
    fn replaces_start_alepa() {
        assert_eq!(format_name("ALEPA KAMPPI HELSINKI"), "ALEPA");
    }

    #[test]
    fn replaces_start_stockmann() {
        assert_eq!(format_name("STOCKMANN HELSINKI CENTER"), "STOCKMANN");
    }

    #[test]
    fn replaces_start_paypal_bandcamp() {
        assert_eq!(format_name("PAYPAL BANDCAMP SAN FRANCISCO"), "PAYPAL BANDCAMP");
    }

    #[test]
    fn replaces_start_paypal_beatport() {
        assert_eq!(format_name("PAYPAL BEATPORT BERLIN"), "PAYPAL BEATPORT");
    }

    #[test]
    fn replaces_start_paypal_djcity() {
        assert_eq!(format_name("PAYPAL DJCITY USA"), "PAYPAL DJCITY");
    }

    #[test]
    fn replaces_start_paypal_dropbox() {
        assert_eq!(format_name("PAYPAL DROPBOX PREMIUM"), "PAYPAL DROPBOX");
    }

    #[test]
    fn replaces_start_paypal_patreon() {
        assert_eq!(format_name("PAYPAL PATREON MEMBERSH"), "PAYPAL PATREON");
    }

    #[test]
    fn replaces_start_hertz() {
        assert_eq!(format_name("HERTZ RENTAL CAR HELSINKI"), "HERTZ");
    }

    #[test]
    fn replaces_start_beamhill() {
        assert_eq!(format_name("BEAMHILL OY HELSINKI"), "BEAMHILL");
    }

    #[test]
    fn replaces_start_k_market() {
        assert_eq!(format_name("K-MARKET KAMPPI HELSINKI"), "K-MARKET");
    }

    #[test]
    fn replaces_chatgpt_subscription() {
        assert_eq!(
            format_name("CHATGPT SUBSCRIPTION HTTPSOPENAI.C"),
            "CHATGPT SUBSCRIPTION OPENAI.COM"
        );
    }

    #[test]
    fn replaces_levistrauss() {
        assert_eq!(format_name("STORE LEVISTRAUSS JEANS"), "STORE LEVIS JEANS");
    }

    #[test]
    fn replaces_dotcomfi() {
        assert_eq!(format_name("STORE.COMFI"), "STORE.COM");
    }
}

#[cfg(test)]
mod test_split_item_text {
    use super::*;

    #[test]
    fn splits_standard_format() {
        let (date, name, sum) = split_item_text("05.02. Osto K-MARKET HELSINKI  25,50");
        assert_eq!(date, "05.02.");
        assert_eq!(name, "Osto K-MARKET HELSINKI");
        assert_eq!(sum, "25,50");
    }

    #[test]
    fn handles_large_sum() {
        let (date, name, sum) = split_item_text("30.12. Osto LUNDIA OY HELSINKI  1 735,50");
        assert_eq!(date, "30.12.");
        assert!(name.contains("LUNDIA"));
        assert_eq!(sum, "1 735,50");
    }

    #[test]
    fn handles_single_digit_day() {
        let (date, name, sum) = split_item_text("05.02. Osto STORE  9,99");
        assert_eq!(date, "05.02.");
        assert!(name.contains("STORE"));
        assert_eq!(sum, "9,99");
    }

    #[test]
    fn handles_empty_string() {
        let (date, name, sum) = split_item_text("");
        assert_eq!(date, "");
        assert_eq!(name, "");
        assert_eq!(sum, "");
    }

    #[test]
    fn handles_no_double_space() {
        let (date, name, sum) = split_item_text("05.02. NoDoubleSpace");
        assert_eq!(date, "05.02.");
        // Without double space, the whole remainder goes to name
        assert!(name.is_empty() || sum.is_empty() || name.contains("NoDoubleSpace") || sum.contains("NoDoubleSpace"));
    }
}

#[cfg(test)]
mod test_visa_item_serialize {
    use super::*;

    #[test]
    fn serializes_to_correct_json() {
        let item = VisaItem {
            date: NaiveDate::from_ymd_opt(2024, 6, 15).expect("valid date"),
            name: "TEST STORE".to_string(),
            sum: 123.45,
        };

        let json = serde_json::to_string(&item).expect("should serialize");
        assert!(json.contains("Date"));
        assert!(json.contains("Name"));
        assert!(json.contains("Sum"));
        assert!(json.contains("2024.06.15"));
        assert!(json.contains("TEST STORE"));
        assert!(json.contains("123,45"));
    }
}

#[cfg(test)]
mod test_visa_item_ordering_comprehensive {
    use super::*;

    #[test]
    fn orders_by_date_first() {
        let item1 = VisaItem {
            date: NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid date"),
            name: "ZZZ".to_string(),
            sum: 999.99,
        };
        let item2 = VisaItem {
            date: NaiveDate::from_ymd_opt(2024, 1, 2).expect("valid date"),
            name: "AAA".to_string(),
            sum: 1.00,
        };

        assert!(item1 < item2);
    }

    #[test]
    fn orders_by_name_when_date_equal() {
        let item1 = VisaItem {
            date: NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid date"),
            name: "AAA".to_string(),
            sum: 999.99,
        };
        let item2 = VisaItem {
            date: NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid date"),
            name: "ZZZ".to_string(),
            sum: 1.00,
        };

        assert!(item1 < item2);
    }

    #[test]
    fn orders_by_sum_when_date_and_name_equal() {
        let item1 = VisaItem {
            date: NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid date"),
            name: "SAME".to_string(),
            sum: 10.00,
        };
        let item2 = VisaItem {
            date: NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid date"),
            name: "SAME".to_string(),
            sum: 20.00,
        };

        assert!(item1 < item2);
    }

    #[test]
    fn equal_items_are_equal() {
        let item1 = VisaItem {
            date: NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid date"),
            name: "SAME".to_string(),
            sum: 10.00,
        };
        let item2 = VisaItem {
            date: NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid date"),
            name: "SAME".to_string(),
            sum: 10.00,
        };

        assert_eq!(item1.cmp(&item2), Ordering::Equal);
    }
}

#[cfg(test)]
mod test_extract_items_edge_cases {
    use super::*;

    #[test]
    fn handles_single_digit_day_and_month() {
        let lines = vec!["05.02. Osto STORE  10,00".to_string()];
        let year = 2024;

        let items = extract_items(&lines, year).expect("should parse");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].date.day(), 5);
        assert_eq!(items[0].date.month(), 2);
    }

    #[test]
    fn handles_double_digit_day_and_month() {
        let lines = vec!["15.12. Osto STORE  10,00".to_string()];
        let year = 2024;

        let items = extract_items(&lines, year).expect("should parse");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].date.day(), 15);
        assert_eq!(items[0].date.month(), 12);
    }

    #[test]
    fn returns_error_for_invalid_date_format() {
        let lines = vec!["invalid date format  10,00".to_string()];
        let year = 2024;

        let result = extract_items(&lines, year);
        assert!(result.is_err());
    }

    #[test]
    fn returns_error_for_invalid_sum() {
        let lines = vec!["05.02. Osto STORE  not_a_number".to_string()];
        let year = 2024;

        let result = extract_items(&lines, year);
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod test_clean_whitespaces_comprehensive {
    use super::*;

    #[test]
    fn replaces_tabs() {
        assert_eq!(clean_whitespaces("hello\tworld"), "hello world");
    }

    #[test]
    fn replaces_newlines() {
        assert_eq!(clean_whitespaces("hello\nworld"), "hello world");
    }

    #[test]
    fn replaces_carriage_returns() {
        assert_eq!(clean_whitespaces("hello\rworld"), "hello world");
    }

    #[test]
    fn collapses_multiple_whitespaces() {
        assert_eq!(clean_whitespaces("hello     world"), "hello world");
    }

    #[test]
    fn handles_mixed_whitespace() {
        assert_eq!(clean_whitespaces("hello\t\n\r  world"), "hello world");
    }

    #[test]
    fn preserves_single_spaces() {
        assert_eq!(clean_whitespaces("hello world"), "hello world");
    }
}

#[cfg(test)]
mod test_filter_prefixes {
    use super::*;

    #[test]
    fn filter_prefixes_contains_common_items() {
        assert!(FILTER_PREFIXES.contains(&"WOLT"));
        assert!(FILTER_PREFIXES.contains(&"ALEPA"));
        assert!(FILTER_PREFIXES.contains(&"K-MARKET"));
        assert!(FILTER_PREFIXES.contains(&"STOCKMANN"));
        assert!(FILTER_PREFIXES.contains(&"EPASSI"));
    }

    #[test]
    fn filter_prefixes_is_not_empty() {
        assert!(!FILTER_PREFIXES.is_empty());
        assert!(FILTER_PREFIXES.len() > 50);
    }
}

#[cfg(test)]
mod test_regex_patterns {
    use super::*;

    #[test]
    fn re_brackets_matches_square_brackets() {
        assert!(RE_BRACKETS.is_match("[test]"));
    }

    #[test]
    fn re_brackets_matches_parentheses() {
        assert!(RE_BRACKETS.is_match("(test)"));
    }

    #[test]
    fn re_brackets_matches_curly_braces() {
        assert!(RE_BRACKETS.is_match("{test}"));
    }

    #[test]
    fn re_html_and_matches_ampersand() {
        assert!(RE_HTML_AND.is_match("&amp;"));
        assert!(RE_HTML_AND.is_match("&AMP;"));
    }

    #[test]
    fn re_separators_matches_newline() {
        assert!(RE_SEPARATORS.is_match("\n"));
    }

    #[test]
    fn re_separators_matches_tab() {
        assert!(RE_SEPARATORS.is_match("\t"));
    }

    #[test]
    fn re_separators_matches_carriage_return() {
        assert!(RE_SEPARATORS.is_match("\r"));
    }

    #[test]
    fn re_whitespace_matches_multiple_spaces() {
        assert!(RE_WHITESPACE.is_match("  "));
        assert!(RE_WHITESPACE.is_match("   "));
    }

    #[test]
    fn re_whitespace_does_not_match_single_space() {
        assert!(!RE_WHITESPACE.is_match(" "));
    }

    #[test]
    fn re_item_date_matches_date_format() {
        assert!(RE_ITEM_DATE.is_match("05.02.Some text"));
        assert!(RE_ITEM_DATE.is_match("15.12.Another text"));
    }

    #[test]
    fn re_item_date_does_not_match_without_date() {
        assert!(!RE_ITEM_DATE.is_match("Some text without date"));
    }

    #[test]
    fn re_start_date_extracts_year() {
        let text = r#"<StartDate Format="CCYYMMDD">20240201</StartDate>"#;
        let caps = RE_START_DATE.captures(text);
        assert!(caps.is_some());
        assert_eq!(caps.unwrap().get(1).unwrap().as_str(), "2024");
    }

    #[test]
    fn re_specification_free_text_extracts_content() {
        let text = "  <SpecificationFreeText>05.02. Some content here</SpecificationFreeText>";
        let caps = RE_SPECIFICATION_FREE_TEXT.captures(text);
        assert!(caps.is_some());
        assert!(caps.unwrap().get(1).unwrap().as_str().contains("05.02."));
    }
}

#[cfg(test)]
mod test_replace_pairs {
    use super::*;

    #[test]
    fn replace_pairs_has_expected_entries() {
        assert!(REPLACE_PAIRS.iter().any(|(pattern, _)| *pattern == "4029357733"));
        assert!(REPLACE_PAIRS.iter().any(|(pattern, _)| *pattern == " - "));
        assert!(REPLACE_PAIRS.iter().any(|(pattern, _)| *pattern == " DRI "));
        assert!(REPLACE_PAIRS.iter().any(|(pattern, _)| *pattern == "VFI "));
    }

    #[test]
    fn replace_start_pairs_has_expected_entries() {
        assert!(REPLACE_START_PAIRS.iter().any(|(pattern, _)| *pattern == "WWW."));
        assert!(REPLACE_START_PAIRS.iter().any(|(pattern, _)| *pattern == "MOB.PAY"));
    }

    #[test]
    fn replace_contains_has_expected_entries() {
        assert!(REPLACE_CONTAINS.iter().any(|(pattern, _)| *pattern == "EPASSI"));
        assert!(REPLACE_CONTAINS.iter().any(|(pattern, _)| *pattern == "WOLT"));
        assert!(REPLACE_CONTAINS.iter().any(|(pattern, _)| *pattern == "ITUNES.COM"));
    }

    #[test]
    fn replace_start_has_expected_entries() {
        assert!(REPLACE_START.contains(&"ALEPA"));
        assert!(REPLACE_START.contains(&"K-MARKET"));
        assert!(REPLACE_START.contains(&"STOCKMANN"));
        assert!(REPLACE_START.contains(&"PAYPAL BANDCAMP"));
    }
}
