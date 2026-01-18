//! visaparse - Parse Finvoice XML credit card statement files.
//!
//! This CLI tool parses Finvoice XML credit card statement files and generates
//! CSV and Excel reports with purchase data and statistics.

mod config;
mod parse;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

pub use crate::config::Config;
use crate::parse::visa_parse;

/// Command line arguments for visaparse.
#[derive(Parser, Debug)]
#[command(
    author,
    version,
    name = env!("CARGO_BIN_NAME"),
    about = "Parse Finvoice XML credit card statement files"
)]
pub struct VisaParseArgs {
    /// Optional input directory or XML file path
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    pub path: Option<PathBuf>,

    /// Optional output path (default is the input directory)
    #[arg(short, long, name = "OUTPUT_PATH")]
    pub output: Option<String>,

    /// Only print information without writing to file
    #[arg(short, long)]
    pub print: bool,

    /// How many total sums to print with verbose output
    #[arg(short, long)]
    pub number: Option<usize>,

    /// Print verbose output
    #[arg(short, long)]
    pub verbose: bool,
}

fn main() -> Result<()> {
    let args = VisaParseArgs::parse();
    let config = Config::from_args(&args)?;
    visa_parse(&config)
}
