extern crate colored;

use anyhow::{Context, Result};
use chrono::{Datelike, Local, NaiveDate};
use clap::Parser;
use colored::Colorize;
use csv::Writer;
use lazy_static::lazy_static;
use regex::Regex;
use walkdir::{DirEntry, WalkDir};

use serde::ser::{Serialize, SerializeStruct, Serializer};
use std::ffi::OsStr;
use std::fmt::format;
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
#[command(author, about, version)]
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

#[derive(Debug, Clone)]
struct VisaItem {
    date: NaiveDate,
    name: String,
    sum: f64,
}

impl Serialize for VisaItem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Define the number of fields you are going to serialize.
        let mut state = serializer.serialize_struct("VisaItem", 3)?;

        let formatted_date = self.date.format("%Y.%m.%d").to_string();
        state.serialize_field("date", &formatted_date)?;

        let formatted_sum = format!("{:.2}€", self.sum);
        state.serialize_field("sum", &formatted_sum)?;

        state.serialize_field("name", &self.name)?;

        state.end()
    }
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

fn format_sum(value: &str) -> Result<f64> {
    value
        .trim()
        .replace(",", ".")
        .parse::<f64>()
        .context(format!("Failed to parse sum as float: {}", value))
}

fn format_name(text: &str) -> String {
    let mut name = text
        .replace("Osto ", "")
        .replace('*', " ")
        .replace('/', " ")
        .replace('_', " ");

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

    name = name.replace(
        "CHATGPT SUBSCRIPTION HTTPSOPENAI.C",
        "CHATGPT SUBSCRIPTION OPENAI.COM",
    );

    name = name.trim().to_string();
    name = RE_WHITESPACE.replace_all(&name, " ").to_string();
    name
}

fn clean_text(text: &str) -> String {
    RE_WHITESPACE
        .replace_all(RE_SEPARATORS.replace_all(text, " ").as_ref(), " ")
        .to_string()
}

fn is_hidden(entry: &DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|s| s.starts_with("."))
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

fn read_xml_file(file: &Path) -> Vec<String> {
    let mut texts: Vec<String> = Vec::new();
    let xml_file = match File::open(file) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "{}",
                format!("Failed to open file: {}\n{}", file.display(), e).red()
            );
            return texts;
        }
    };

    let reader = BufReader::new(xml_file);
    for line in reader.lines() {
        if let Ok(text) = line {
            if let Some(caps) = RE_SPECIFICATION_FREE_TEXT.captures(&text) {
                if let Some(matched) = caps.get(1) {
                    let text = matched.as_str();
                    if RE_ITEM_DATE.is_match(text) {
                        texts.push(text.to_string());
                    }
                }
            }
        }
    }
    texts
}

fn split_item_row(input: &str) -> (String, String, String) {
    // Split the string at the first whitespace
    let mut parts = input.splitn(2, ' ');
    let first_part = parts.next().unwrap_or("").trim().to_string();
    let remainder = parts.next().unwrap_or("");

    // Split the remainder at three whitespaces
    let mut parts = remainder.splitn(2, "   ");
    let second_part = parts.next().unwrap_or("").trim().to_string();
    let third_part = parts.next().unwrap_or("").trim().to_string();

    (first_part, second_part, third_part)
}

fn append_year_and_convert(rows: Vec<String>) -> Vec<VisaItem> {
    let current_year = Local::now().year();
    let previous_year = current_year - 1;

    let mut temp: Vec<(u32, u32, String, f64)> = Vec::new();
    for line in rows.iter() {
        let (date, name, sum) = split_item_row(line);
        let (day, month) = date.split_once('.').unwrap();
        let month: u32 = month.replace(".", "").parse().unwrap();
        let day: u32 = day.parse().unwrap();
        let name = format_name(&name);
        let sum = format_sum(&sum).unwrap();
        temp.push((day, month, name, sum));
    }

    // Determine if there's a transition from December to January.
    let mut year_transition_detected = false;
    let mut last_month: u32 = 0;
    for (_, month, _, _) in temp.iter() {
        if *month == 1u32 && last_month == 12u32 {
            year_transition_detected = true;
            break;
        }
        last_month = *month;
    }

    let mut result: Vec<VisaItem> = Vec::new();
    for (day, month, name, sum) in temp.into_iter() {
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

fn parse_files(files: Vec<PathBuf>) -> Result<Vec<VisaItem>> {
    let mut result: Vec<VisaItem> = Vec::new();
    for file in files.iter() {
        println!("{}", file.display());
        let raw_text = read_xml_file(file);
        let one_file = append_year_and_convert(raw_text);
        result.extend(one_file);
    }

    for item in &result {
        println!("{}", item);
    }

    Ok(result)
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
            .magenta()
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

fn visa_parse(input: PathBuf, output: PathBuf, verbose: bool) -> Result<()> {
    let files = if input.is_file() {
        println!("Parsing file: {}", input.display());
        if input.extension() == Some(OsStr::new("xml")) {
            vec![input.clone()]
        } else {
            vec![]
        }
    } else {
        println!("Parsing files from: {}", input.display());
        get_xml_files(&input)
    };

    if files.is_empty() {
        anyhow::bail!("No XML files to parse");
    }

    let rows = parse_files(files)?;
    if verbose {
        println!("Got {} items", rows.len());
    }

    write_to_csv(&rows, &output)?;

    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let input_path = args.path.trim();
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

    let output_path = {
        let path = args.output.unwrap_or_default().trim().to_string();
        if path.is_empty() {
            if absolute_input_path.is_file() {
                absolute_input_path
                    .parent()
                    .context("Failed to get parent directory")?
                    .to_path_buf()
            } else {
                absolute_input_path.clone()
            }
        } else {
            Path::new(&path).to_path_buf()
        }
    };
    visa_parse(absolute_input_path, output_path, args.verbose)
}
