extern crate colored;

use anyhow::{Context, Result};
use chrono::{Datelike, NaiveDate};
use clap::Parser;
use colored::Colorize;
use csv::Writer;
use regex::Regex;
use std::ffi::OsStr;

use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use lazy_static::lazy_static;

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
}

#[derive(Parser, Debug)]
#[command(author, about, version)]
struct Args {
    /// Input directory or XML file path
    #[clap(value_parser)]
    path: String,

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

fn format_sum(value: &str) -> Result<f64> {
    value
        .trim()
        .replace(",", ".")
        .parse::<f64>()
        .context("Failed to parse sum as float")
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

fn get_xml_files<P: AsRef<Path>>(root: P) -> Vec<PathBuf> {
    WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .map(|e| e.path().to_owned())
        .filter(|path| path.is_file() && path.extension() == Some(OsStr::new("xml")))
        .collect()
}

fn parse_files(files: Vec<PathBuf>) -> Result<Vec<VisaItem>> {
    let result: Vec<VisaItem> = vec![];
    for file in files.iter() {
        println!("{}", file.display());
    }
    Ok(result)
}

fn visa_parse(path: PathBuf, verbose: bool) -> Result<()> {
    let (files, root) = if path.is_file() {
        println!("Parsing file: {}", path.display());
        let root = path.parent().context("Failed to get file root directory")?;
        if path.extension() == Some(OsStr::new("xml")) {
            (vec![path], root.to_path_buf())
        } else {
            (vec![], root.to_path_buf())
        }
    } else {
        println!("Parsing files from: {}", path.display());
        let files = get_xml_files(&path);
        (files, path)
    };

    if files.is_empty() {
        anyhow::bail!("No XML files to parse");
    }

    let rows = parse_files(files)?;

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
    visa_parse(absolute_input_path, args.verbose)
}
