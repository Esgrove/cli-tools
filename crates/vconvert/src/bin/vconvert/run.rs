//! Running the selected vconvert mode.
//!
//! Opens the database and the run log, then runs a scan, processes the pending files from the database,
//! or runs one of the database commands.

use std::sync::atomic::AtomicBool;

use anyhow::{Result, bail, ensure};
use cli_tools_vconvert::{Database, ProcessingOutcome, RunSummary, VideoConvert};
use colored::Colorize;

use crate::cli::{DatabaseMode, clean_scan_cache, clear_database, list_extensions, show_database_contents};
use crate::config::Config;
use crate::logger::FileLogger;
use crate::reporter::Reporter;

/// Run the mode selected by the config.
///
/// Setting the abort flag stops a conversion run after the current file.
pub fn run(config: &Config, abort_flag: &AtomicBool) -> Result<()> {
    match config.database_mode {
        Some(DatabaseMode::Clear) => Database::open_default().and_then(|database| clear_database(&database)),
        Some(DatabaseMode::Show) => {
            let database = Database::open_read_only()?;
            show_database_contents(database.as_ref(), config)
        }
        Some(DatabaseMode::ListExtensions) => {
            let database = Database::open_read_only()?;
            list_extensions(database.as_ref(), config.verbose)
        }
        Some(DatabaseMode::CleanScanCache) => {
            Database::open_default().and_then(|mut database| clean_scan_cache(&mut database, config.verbose))
        }
        Some(DatabaseMode::Process) => {
            let logger = create_logger(config)?;
            run_from_database(config, logger, abort_flag)
        }
        None => {
            let logger = create_logger(config)?;
            run_scan(config, logger, abort_flag)
        }
    }
}

/// Create a file logger for real runs, or a no-op logger for dry-run previews.
fn create_logger(config: &Config) -> Result<FileLogger> {
    if config.options.dry_run {
        return Ok(FileLogger::noop());
    }
    let logger = FileLogger::new()?;
    logger.log_init(config);
    Ok(logger)
}

/// Scan the input paths and process the files found.
fn run_scan(config: &Config, logger: FileLogger, abort_flag: &AtomicBool) -> Result<()> {
    let mut database = if config.options.dry_run {
        match Database::open_read_only()? {
            Some(existing) => existing,
            None => Database::open_in_memory()?,
        }
    } else {
        Database::open_default()?
    };
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
    processing_status(&summary)
}

/// Process the pending files stored in the database, then show what remains there.
fn run_from_database(config: &Config, logger: FileLogger, abort_flag: &AtomicBool) -> Result<()> {
    let mut database = if config.options.dry_run {
        let Some(existing) = Database::open_read_only()? else {
            println!("No pending files in database matching filters");
            return Ok(());
        };
        existing
    } else {
        Database::open_default()?
    };

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

    processing_status(&summary)
}

/// Return the command status after the conversion and database summaries have been printed.
fn processing_status(summary: &RunSummary) -> Result<()> {
    if summary.stats.has_successes() {
        return Ok(());
    }
    if summary.outcome == ProcessingOutcome::OutOfDiskSpace {
        bail!("{}", summary.outcome);
    }
    ensure!(
        summary.stats.files_failed == 0,
        "No requested video processing succeeded"
    );
    Ok(())
}
