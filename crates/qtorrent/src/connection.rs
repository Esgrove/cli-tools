//! Connection settings for the qBittorrent `WebUI`.
//!
//! Holds the host, port, and credentials,
//! and opens an authenticated client session that reports the server versions.

use anyhow::{Result, bail};

use crate::qbittorrent::QBittorrentClient;

/// Default qBittorrent `WebUI` host.
pub const DEFAULT_HOST: &str = "localhost";

/// Default qBittorrent `WebUI` port.
pub const DEFAULT_PORT: u16 = 8080;

/// Where and how to connect to the qBittorrent `WebUI`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionSettings {
    /// qBittorrent `WebUI` host.
    pub host: String,
    /// qBittorrent `WebUI` port.
    pub port: u16,
    /// qBittorrent `WebUI` username.
    pub username: String,
    /// qBittorrent `WebUI` password.
    pub password: String,
}

/// An authenticated client session and the versions the server reported.
#[derive(Debug)]
pub struct Connection {
    /// Logged in API client.
    pub client: QBittorrentClient,
    /// qBittorrent application version.
    pub app_version: String,
    /// `WebUI` API version.
    pub api_version: String,
}

impl ConnectionSettings {
    /// Create connection settings for the given host, port, and credentials.
    #[must_use]
    pub const fn new(host: String, port: u16, username: String, password: String) -> Self {
        Self {
            host,
            port,
            username,
            password,
        }
    }

    /// Check if both the username and the password are set.
    #[must_use]
    pub const fn has_credentials(&self) -> bool {
        !self.username.is_empty() && !self.password.is_empty()
    }

    /// Log in to qBittorrent and check the connection by reading the app and API versions.
    ///
    /// # Errors
    /// Returns an error if the credentials are missing, the login fails, or the versions cannot be read.
    pub async fn connect(&self) -> Result<Connection> {
        if !self.has_credentials() {
            bail!("qBittorrent credentials not configured");
        }

        let mut client = QBittorrentClient::new(&self.host, self.port);
        client.login(&self.username, &self.password).await?;

        let application_version = client.get_app_version().await?;
        let api_version = client.get_api_version().await?;

        Ok(Connection {
            client,
            app_version: application_version,
            api_version,
        })
    }
}

impl Default for ConnectionSettings {
    fn default() -> Self {
        Self::new(DEFAULT_HOST.to_string(), DEFAULT_PORT, String::new(), String::new())
    }
}

#[cfg(test)]
mod test_connection_settings {
    use crate::test_server::{route, serve};

    use super::*;

    #[test]
    fn credentials_need_both_username_and_password() {
        let mut settings = ConnectionSettings::default();
        assert!(!settings.has_credentials());
        settings.username = "user".to_string();
        assert!(!settings.has_credentials());
        settings.password = "password".to_string();
        assert!(settings.has_credentials());
    }

    #[tokio::test]
    async fn connecting_without_credentials_fails_before_the_network() {
        let error = ConnectionSettings::default()
            .connect()
            .await
            .expect_err("should fail without credentials");
        assert!(error.to_string().contains("credentials not configured"));
    }

    #[tokio::test]
    async fn connecting_reads_the_server_versions() -> Result<()> {
        let port = serve(vec![
            route("/api/v2/auth/login", 200, "Ok."),
            route("/api/v2/app/version", 200, "v5.0.0"),
            route("/api/v2/app/webapiVersion", 200, "2.11.2"),
        ])
        .await;
        let settings = ConnectionSettings::new("127.0.0.1".to_string(), port, "user".to_string(), "pw".to_string());

        let connection = settings.connect().await?;

        assert_eq!(connection.app_version, "v5.0.0");
        assert_eq!(connection.api_version, "2.11.2");
        Ok(())
    }
}
