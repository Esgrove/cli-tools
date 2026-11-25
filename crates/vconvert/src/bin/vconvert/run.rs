//! Running the selected vconvert mode.
//!
//! Opens the database and the run log, then runs a scan, processes the pending files from the database,
//! or runs one of the database commands.

use std::sync::atomic::AtomicBool;

use anyhow::Result;
use cli_tools_vconvert::{Database, VideoConvert};
use colored::Colorize;

use crate::cli::{DatabaseMode, clean_scan_cache, clear_database, list_extensions, show_database_contents};
use crate::config::Config;
use crate::logger::FileLogger;
use crate::reporter::Reporter;

/// Run the mode selected by the config.
///
/// Setting the abort flag stops a conversion run after the current file.
pub fn run(config: &Config, abort_flag: &AtomicBool) -> Result<()> {
    let logger = FileLogger::new()?;
    logger.log_init(config);

    match config.database_mode {
        Some(DatabaseMode::Clear) => Database::open_default().and_then(|database| clear_database(&database)),
        Some(DatabaseMode::Show) => {
            Database::open_default().and_then(|database| show_database_contents(&database, config))
        }
        Some(DatabaseMode::ListExtensions) => {
            Database::open_default().and_then(|database| list_extensions(&database, config.verbose))
        }
        Some(DatabaseMode::CleanScanCache) => {
            Database::open_default().and_then(|mut database| clean_scan_cache(&mut database, config.verbose))
        }
        Some(DatabaseMode::Process) => run_from_database(config, logger, abort_flag),
        None => run_scan(config, logger, abort_flag),
    }
}

/// Scan the input paths and process the files found.
fn run_scan(config: &Config, logger: FileLogger, abort_flag: &AtomicBool) -> Result<()> {
    let mut database = Database::open_default()?;
    if config.verbose {
        println!("Database: {}", Database::path().display());
    }

    let reporter = Reporter::new(config, logger);
    let converter = VideoConvert::new(config.options.clone(), &reporter);
    let Some(summary) = converter.run(&mut database, abort_flag)? else {
        println!("No video files found");
        return Ok(());
    };

    reporter.finish(&summary);
    Ok(())
}

/// Process the pending files stored in the database, then show what remains there.
fn run_from_database(config: &Config, logger: FileLogger, abort_flag: &AtomicBool) -> Result<()> {
    let mut database = Database::open_default()?;

    let reporter = Reporter::new(config, logger);
    let converter = VideoConvert::new(config.options.clone(), &reporter);
    let Some(summary) = converter.run_from_database(&mut database, abort_flag)? else {
        println!("No pending files in database matching filters");
        return Ok(());
    };

    reporter.finish(&summary);

    let database_stats = database.get_stats()?;
    if database_stats.total_files > 0 {
        println!("\n{}", "Remaining in database:".bold());
        println!("{database_stats}");
    }

    Ok(())
}
