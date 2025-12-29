//! qBittorrent `WebUI` API client module.
//!
//! Provides functions to interact with the qBittorrent `WebUI` API
//! for authentication and adding torrents.
//!
//! Documentation:
//! <https://github.com/qbittorrent/qBittorrent/wiki/WebUI-API-(qBittorrent-5.0)>

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use reqwest::multipart::{Form, Part};
use reqwest::{Client, StatusCode};
use serde::Deserialize;

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

/// Torrent info from the qBittorrent API `/torrents/info` endpoint.
#[derive(Debug, Deserialize)]
struct TorrentListItem {
    /// Torrent hash.
    hash: String,
    /// Torrent name.
    name: String,
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

    /// Check if the client is authenticated.
    #[allow(dead_code)]
    #[must_use]
    pub const fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    /// Authenticate with the qBittorrent `WebUI`.
    ///
    /// # Errors
    /// Returns an error if authentication fails.
    pub async fn login(&mut self, username: &str, password: &str) -> Result<()> {
        let url = self.build_url("auth/login");

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

        let url = self.build_url("auth/logout");

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

        let url = self.build_url("torrents/add");

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
            // Use both "paused" (legacy) and "stopped" (qBittorrent 5.0+) for compatibility
            form = form.text("paused", "true");
            form = form.text("stopped", "true");
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

    /// Get the API version from qBittorrent.
    ///
    /// # Errors
    /// Returns an error if the request fails.
    pub async fn get_api_version(&self) -> Result<String> {
        let url = self.build_url("app/webapiVersion");

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
        let url = self.build_url("app/version");

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

        let url = self.build_url("app/defaultSavePath");

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

        let url = self.build_url("torrents/filePrio");

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

    /// Get the list of all torrents in qBittorrent.
    ///
    /// Returns a map from torrent hash (lowercase) to torrent name.
    /// The list is sorted by name.
    ///
    /// # Errors
    /// Returns an error if the request fails or if not authenticated.
    pub async fn get_torrent_list(&self) -> Result<HashMap<String, String>> {
        if !self.authenticated {
            bail!("Not authenticated. Call login() first.");
        }

        let url = self.build_url("torrents/info?sort=name");

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to get torrent list")?;

        let status = response.status();

        if status == StatusCode::FORBIDDEN {
            bail!("Authentication required or session expired");
        }

        let body = response.text().await.context("Failed to read torrent list response")?;

        let torrents: Vec<TorrentListItem> =
            serde_json::from_str(&body).context("Failed to parse torrent list JSON")?;

        let map = torrents
            .into_iter()
            .map(|item| {
                let hash = item.hash.to_lowercase();
                (hash, item.name)
            })
            .collect();

        Ok(map)
    }

    /// Rename a file within a torrent.
    ///
    /// This renames the actual file on disk, not just the display name.
    /// For single-file torrents, `old_path` is the original filename and `new_path` is the new filename.
    /// For multi-file torrents, paths are relative to the torrent's root folder.
    ///
    /// # Errors
    /// Returns an error if the request fails or if not authenticated.
    pub async fn rename_file(&self, info_hash: &str, old_path: &str, new_path: &str) -> Result<()> {
        if !self.authenticated {
            bail!("Not authenticated. Call login() first.");
        }

        let url = self.build_url("torrents/renameFile");

        let response = self
            .client
            .post(&url)
            .form(&[("hash", info_hash), ("oldPath", old_path), ("newPath", new_path)])
            .send()
            .await
            .context("Failed to send rename file request")?;

        let status = response.status();

        match status {
            StatusCode::OK => Ok(()),
            StatusCode::BAD_REQUEST => {
                bail!("Missing or invalid parameters for file rename")
            }
            StatusCode::CONFLICT => {
                bail!("Invalid new path or file name already in use")
            }
            StatusCode::FORBIDDEN => {
                bail!("Authentication required or session expired")
            }
            StatusCode::NOT_FOUND => {
                bail!("Torrent hash not found")
            }
            _ => {
                let body = response.text().await.unwrap_or_default();
                bail!("Failed to rename file: HTTP {status} - {body}")
            }
        }
    }

    /// Rename a folder within a torrent.
    ///
    /// This renames the actual folder on disk.
    /// For multi-file torrents, paths are relative to the save location.
    ///
    /// # Errors
    /// Returns an error if the request fails or if not authenticated.
    pub async fn rename_folder(&self, info_hash: &str, old_path: &str, new_path: &str) -> Result<()> {
        if !self.authenticated {
            bail!("Not authenticated. Call login() first.");
        }

        let url = self.build_url("torrents/renameFolder");

        let response = self
            .client
            .post(&url)
            .form(&[("hash", info_hash), ("oldPath", old_path), ("newPath", new_path)])
            .send()
            .await
            .context("Failed to send rename folder request")?;

        let status = response.status();

        match status {
            StatusCode::OK => Ok(()),
            StatusCode::BAD_REQUEST => {
                bail!("Missing or invalid parameters for folder rename")
            }
            StatusCode::CONFLICT => {
                bail!("Invalid new path or folder name already in use")
            }
            StatusCode::FORBIDDEN => {
                bail!("Authentication required or session expired")
            }
            StatusCode::NOT_FOUND => {
                bail!("Torrent hash not found")
            }
            _ => {
                let body = response.text().await.unwrap_or_default();
                bail!("Failed to rename folder: HTTP {status} - {body}")
            }
        }
    }

    /// Set the name of a torrent (display name in qBittorrent).
    ///
    /// This changes the torrent's name as shown in the UI, not the actual file/folder name on disk.
    /// Use `rename_file` or `rename_folder` to change the actual content name.
    ///
    /// # Errors
    /// Returns an error if the request fails or if not authenticated.
    pub async fn set_torrent_name(&self, info_hash: &str, name: &str) -> Result<()> {
        if !self.authenticated {
            bail!("Not authenticated. Call login() first.");
        }

        let url = self.build_url("torrents/rename");

        let response = self
            .client
            .post(&url)
            .form(&[("hash", info_hash), ("name", name)])
            .send()
            .await
            .context("Failed to send rename torrent request")?;

        let status = response.status();

        match status {
            StatusCode::OK => Ok(()),
            StatusCode::NOT_FOUND => {
                bail!("Torrent hash not found")
            }
            StatusCode::CONFLICT => {
                bail!("Torrent name is empty")
            }
            StatusCode::FORBIDDEN => {
                bail!("Authentication required or session expired")
            }
            _ => {
                let body = response.text().await.unwrap_or_default();
                bail!("Failed to rename torrent: HTTP {status} - {body}")
            }
        }
    }

    /// Build full API url from the base url and given endpoint.
    fn build_url(&self, url: &str) -> String {
        format!("{}/api/v2/{url}", self.base_url)
    }
}
