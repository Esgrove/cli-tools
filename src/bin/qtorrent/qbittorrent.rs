//! qBittorrent `WebUI` API client module.
//!
//! Provides functions to interact with the qBittorrent `WebUI` API
//! for authentication and adding torrents.

use std::path::Path;

use anyhow::{Context, Result, bail};
use reqwest::multipart::{Form, Part};
use reqwest::{Client, StatusCode};

/// qBittorrent `WebUI` API client.
#[derive(Debug)]
pub struct QBittorrentClient {
    client: Client,
    base_url: String,
    authenticated: bool,
}

/// Parameters for adding a torrent to qBittorrent.
#[derive(Debug, Default)]
pub struct AddTorrentParams {
    /// Torrent file path.
    pub torrent_path: String,
    /// Torrent file bytes.
    pub torrent_bytes: Vec<u8>,
    /// Download folder path.
    pub save_path: Option<String>,
    /// Category for the torrent.
    pub category: Option<String>,
    /// Tags for the torrent (comma-separated).
    pub tags: Option<String>,
    /// Rename the torrent (sets the output filename for single-file torrents).
    pub rename: Option<String>,
    /// Skip hash checking.
    pub skip_checking: bool,
    /// Add torrent in paused state.
    pub paused: bool,
    /// Create root folder (false to avoid subfolder for single-file torrents).
    pub root_folder: bool,
}

impl QBittorrentClient {
    /// Create a new qBittorrent client.
    ///
    /// # Arguments
    /// * `host` - The host address (e.g., "localhost" or "192.168.1.100")
    /// * `port` - The `WebUI` port (default is usually 8080)
    #[must_use]
    pub fn new(host: &str, port: u16) -> Self {
        let base_url = format!("http://{host}:{port}");
        let client = Client::builder()
            .cookie_store(true)
            .build()
            .expect("Failed to build HTTP client");

        Self {
            client,
            base_url,
            authenticated: false,
        }
    }

    /// Authenticate with the qBittorrent `WebUI`.
    ///
    /// # Errors
    /// Returns an error if authentication fails.
    pub async fn login(&mut self, username: &str, password: &str) -> Result<()> {
        let url = format!("{}/api/v2/auth/login", self.base_url);

        let response = self
            .client
            .post(&url)
            .form(&[("username", username), ("password", password)])
            .send()
            .await
            .context("Failed to send login request")?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if status == StatusCode::OK && body == "Ok." {
            self.authenticated = true;
            Ok(())
        } else if status == StatusCode::FORBIDDEN || body == "Fails." {
            bail!("Authentication failed: Invalid username or password")
        } else {
            bail!("Authentication failed: HTTP {status} - {body}")
        }
    }

    /// Log out from the qBittorrent `WebUI`.
    ///
    /// # Errors
    /// Returns an error if the logout request fails.
    pub async fn logout(&mut self) -> Result<()> {
        if !self.authenticated {
            return Ok(());
        }

        let url = format!("{}/api/v2/auth/logout", self.base_url);

        self.client
            .post(&url)
            .send()
            .await
            .context("Failed to send logout request")?;

        self.authenticated = false;
        Ok(())
    }

    /// Add a torrent to qBittorrent.
    ///
    /// # Errors
    /// Returns an error if the torrent cannot be added.
    pub async fn add_torrent(&self, params: AddTorrentParams) -> Result<()> {
        if !self.authenticated {
            bail!("Not authenticated. Call login() first.");
        }

        let url = format!("{}/api/v2/torrents/add", self.base_url);

        // Extract filename from path
        let filename = Path::new(&params.torrent_path).file_name().map_or_else(
            || "torrent.torrent".to_string(),
            |name| name.to_string_lossy().to_string(),
        );

        // Build multipart form
        let torrent_part = Part::bytes(params.torrent_bytes)
            .file_name(filename)
            .mime_str("application/x-bittorrent")
            .context("Failed to set MIME type")?;

        let mut form = Form::new().part("torrents", torrent_part);

        // Add optional parameters
        if let Some(save_path) = params.save_path {
            form = form.text("savepath", save_path);
        }

        if let Some(category) = params.category {
            form = form.text("category", category);
        }

        if let Some(tags) = params.tags {
            form = form.text("tags", tags);
        }

        if let Some(rename) = params.rename {
            form = form.text("rename", rename);
        }

        if params.skip_checking {
            form = form.text("skip_checking", "true");
        }

        if params.paused {
            form = form.text("paused", "true");
        }

        if params.root_folder {
            form = form.text("root_folder", "true");
        } else {
            form = form.text("root_folder", "false");
        }

        let response = self
            .client
            .post(&url)
            .multipart(form)
            .send()
            .await
            .context("Failed to send add torrent request")?;

        let status = response.status();

        match status {
            StatusCode::OK => Ok(()),
            StatusCode::UNSUPPORTED_MEDIA_TYPE => {
                bail!("Torrent file is not valid")
            }
            StatusCode::FORBIDDEN => {
                bail!("Authentication required or session expired")
            }
            _ => {
                let body = response.text().await.unwrap_or_default();
                bail!("Failed to add torrent: HTTP {status} - {body}")
            }
        }
    }

    /// Check if the client is authenticated.
    #[allow(dead_code)]
    #[must_use]
    pub const fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    /// Get the API version from qBittorrent.
    ///
    /// # Errors
    /// Returns an error if the request fails.
    pub async fn get_api_version(&self) -> Result<String> {
        let url = format!("{}/api/v2/app/webapiVersion", self.base_url);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to get API version")?;

        let version = response.text().await.context("Failed to read API version")?;
        Ok(version)
    }

    /// Get the qBittorrent application version.
    ///
    /// # Errors
    /// Returns an error if the request fails.
    pub async fn get_app_version(&self) -> Result<String> {
        let url = format!("{}/api/v2/app/version", self.base_url);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to get app version")?;

        let version = response.text().await.context("Failed to read app version")?;
        Ok(version)
    }

    /// Get the default save path from qBittorrent preferences.
    ///
    /// # Errors
    /// Returns an error if the request fails or if not authenticated.
    #[allow(dead_code)]
    pub async fn get_default_save_path(&self) -> Result<String> {
        if !self.authenticated {
            bail!("Not authenticated. Call login() first.");
        }

        let url = format!("{}/api/v2/app/defaultSavePath", self.base_url);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to get default save path")?;

        let path = response.text().await.context("Failed to read default save path")?;
        Ok(path)
    }

    /// Set file priorities for a torrent.
    ///
    /// Priority values:
    /// - 0: Do not download
    /// - 1: Normal priority
    /// - 6: High priority
    /// - 7: Maximum priority
    ///
    /// # Errors
    /// Returns an error if the request fails or if not authenticated.
    pub async fn set_file_priorities(&self, info_hash: &str, file_indices: &[usize], priority: u8) -> Result<()> {
        if !self.authenticated {
            bail!("Not authenticated. Call login() first.");
        }

        if file_indices.is_empty() {
            return Ok(());
        }

        let url = format!("{}/api/v2/torrents/filePrio", self.base_url);

        // Format file indices as pipe-separated list
        let indices_str = file_indices
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("|");

        let response = self
            .client
            .post(&url)
            .form(&[
                ("hash", info_hash),
                ("id", &indices_str),
                ("priority", &priority.to_string()),
            ])
            .send()
            .await
            .context("Failed to send set file priorities request")?;

        let status = response.status();

        match status {
            StatusCode::OK => Ok(()),
            StatusCode::BAD_REQUEST => {
                bail!("Invalid priority value")
            }
            StatusCode::CONFLICT => {
                bail!("Torrent metadata has not yet been downloaded")
            }
            StatusCode::FORBIDDEN => {
                bail!("Authentication required or session expired")
            }
            StatusCode::NOT_FOUND => {
                bail!("Torrent hash not found")
            }
            _ => {
                let body = response.text().await.unwrap_or_default();
                bail!("Failed to set file priorities: HTTP {status} - {body}")
            }
        }
    }
}
