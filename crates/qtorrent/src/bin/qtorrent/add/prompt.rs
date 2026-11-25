//! Interactive questions of the add workflow.
//!
//! Asks for a new output name before adding, offers to rename torrents that already exist in qBittorrent,
//! and confirms adding a torrent whose file is already in the downloaded directory.

use std::io::{self, Write};
use std::path::Path;

use anyhow::{Context, Result};
use colored::Colorize;

use cli_tools_qtorrent::{NameSuggester, TorrentAdder, TorrentInfo};

use super::QTorrent;
use super::printer::EventPrinter;

impl QTorrent {
    /// Warn about an existing torrent file in the downloaded directory and ask whether to continue.
    pub(super) fn confirm_downloaded_collision(&self, existing: &Path, prompt: &str) -> Result<bool> {
        cli_tools_common::print_yellow!(
            "A torrent file with the same name already exists in the downloaded directory:\n  {}",
            existing.display()
        );

        if self.config.yes {
            return Ok(true);
        }

        cli_tools_common::get_user_confirmation(prompt, false)
            .map_err(|error| anyhow::anyhow!("Failed to get confirmation: {error}"))
    }

    /// Prompt user to rename an existing torrent in qBittorrent.
    ///
    /// Returns `true` if the torrent was renamed, `false` if skipped.
    pub(super) async fn prompt_rename_existing(
        &self,
        info: &TorrentInfo,
        existing_name: &str,
        adder: &TorrentAdder<'_>,
        printer: &mut EventPrinter,
    ) -> Result<bool> {
        if self.config.yes || self.config.skip_existing {
            // With --yes or --skip-existing flag, skip rename prompt for existing torrents
            return Ok(false);
        }

        let suggested = self.names.clean_suggested_name(info);
        let internal_formatted = self.names.clean_internal_name(info);

        // Skip if existing name already matches suggestion
        let matches_suggested = existing_name == suggested;
        let matches_internal = internal_formatted.as_ref().is_some_and(|name| existing_name == name);
        if matches_suggested || matches_internal {
            println!("  {}", "Name already matches suggestion.".dimmed());
            return Ok(false);
        }

        println!(
            "  {} [{}]",
            "Rename existing?".cyan(),
            "press Enter to skip, or type new name".dimmed()
        );
        println!("  {} {}", "1:".dimmed(), suggested.green());
        if let Some(ref internal) = internal_formatted
            && internal != &suggested
        {
            println!("  {} {}", "2:".dimmed(), internal.green());
        }
        let input = read_choice()?;
        if input.is_empty() {
            println!("  {}", "Skipped.".dimmed());
            return Ok(false);
        }

        // Check if user entered a number to select an option
        let new_name = match input.as_str() {
            "1" => suggested,
            "2" if internal_formatted.is_some() => internal_formatted.expect("internal_formatted checked above"),
            _ => self.names.format_custom_name(&input, info.effective_is_multi_file),
        };

        adder.rename_existing(info, existing_name, &new_name, printer).await?;

        Ok(true)
    }

    /// Prompt user to rename the output name for a torrent.
    ///
    /// Shows both the suggested name (from torrent filename) and the formatted internal name,
    /// allowing the user to choose or enter a custom name.
    /// Returns `Some(new_name)` if the user wants to rename, `None` to keep original.
    pub(super) fn prompt_rename(&self, info: &TorrentInfo) -> Result<Option<String>> {
        if self.config.yes {
            // With --yes flag, skip rename prompt
            return Ok(None);
        }

        let label = if info.effective_is_multi_file {
            "Rename folder?"
        } else {
            "Rename file?"
        };

        let suggested = self.names.clean_suggested_name(info);
        let internal_formatted = self.names.clean_internal_name(info);

        // Normalize the two options to check if they're effectively the same
        let (normalized_suggested, normalized_internal) =
            NameSuggester::normalize_rename_options(&suggested, internal_formatted.as_deref());

        let show_internal = normalized_internal
            .as_ref()
            .is_some_and(|internal| internal != &normalized_suggested);

        println!(
            "{} [{}]",
            label.cyan(),
            "press Enter to skip, or type new name".dimmed()
        );
        println!("  {} {}", "1:".dimmed(), normalized_suggested.green());
        if let Some(ref internal) = normalized_internal
            && show_internal
        {
            println!("  {} {}", "2:".dimmed(), internal.green());
        }
        let input = read_choice()?;
        if input.is_empty() {
            return Ok(None);
        }

        // Check if user entered a number to select an option
        let selected_name = match input.as_str() {
            "1" => normalized_suggested,
            "2" if show_internal && normalized_internal.is_some() => {
                normalized_internal.expect("normalized_internal checked above")
            }
            _ => self.names.custom_output_name(&input, info, &normalized_suggested),
        };

        let new_name_label = if info.effective_is_multi_file {
            "New folder name:"
        } else {
            "New file name:"
        };
        println!("  {} {}", new_name_label.dimmed(), selected_name.green());
        Ok(Some(selected_name))
    }
}

/// Ask for a numbered choice or a new name and return the trimmed input.
fn read_choice() -> Result<String> {
    print!("  {} ", "Choice or name:".dimmed());
    io::stdout().flush().context("Failed to flush stdout")?;

    let mut input = String::new();
    io::stdin().read_line(&mut input).context("Failed to read input")?;
    Ok(input.trim().to_string())
}
