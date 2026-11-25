//! Connecting to and disconnecting from qBittorrent for the qtorrent commands.
//!
//! Explains how to configure missing credentials, prints the connection progress in verbose mode,
//! and reports a failed logout as a warning.

use anyhow::{Result, bail};
use colored::Colorize;

use cli_tools_qtorrent::{ConnectionSettings, QBittorrentClient};

/// Log in to qBittorrent with the configured settings.
///
/// # Errors
/// Returns an error if credentials are not configured or the connection fails.
pub async fn connect(settings: &ConnectionSettings, verbose: bool) -> Result<QBittorrentClient> {
    if !settings.has_credentials() {
        bail!(
            "qBittorrent credentials not configured.\n\
             Set username and password via command line arguments or in config file:\n\
             ~/.config/cli-tools.toml under [qtorrent] section"
        );
    }

    if verbose {
        cli_tools_common::print_cyan("Connecting to qBittorrent...");
    }

    let connection = settings.connect().await?;

    if verbose {
        println!(
            "{} (App {}, API v{})\n",
            "Connected successfully".green(),
            connection.app_version,
            connection.api_version
        );
    }

    Ok(connection.client)
}

/// Log out of qBittorrent, printing a warning if it fails.
pub async fn logout(client: &mut QBittorrentClient) {
    if let Err(error) = client.logout().await {
        cli_tools_common::print_yellow!("Failed to logout: {error}");
    }
}
