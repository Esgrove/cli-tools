//! Docker Hub digest lookup for the official `rust` image.
//!
//! Asks the registry for the manifest digest of a tag with an anonymous pull token,
//! and caches the answer per tag so parallel projects share one request.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use serde::Deserialize;

/// Token endpoint for anonymous pulls of the `rust` repository.
const TOKEN_URL: &str = "https://auth.docker.io/token?service=registry.docker.io&scope=repository:library/rust:pull";

/// Manifest endpoint prefix for the `rust` repository.
const MANIFEST_URL: &str = "https://registry-1.docker.io/v2/library/rust/manifests";

/// Manifest types to accept, multi-platform indexes first,
/// so the digest matches what `docker pull` resolves.
const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
    application/vnd.docker.distribution.manifest.list.v2+json, \
    application/vnd.oci.image.manifest.v1+json, \
    application/vnd.docker.distribution.manifest.v2+json";

/// Resolves and caches `rust` image digests.
pub struct DigestResolver {
    /// HTTP client shared by all requests.
    client: Client,
    /// Cached lookup outcome per tag (`Ok(None)` for tags that do not exist, `Err(msg)` for cached failures).
    cache: Mutex<HashMap<String, Result<Option<String>, String>>>,
    /// Cached anonymous bearer token for the `library/rust` repository.
    token: Mutex<Option<String>>,
}

/// Token response from the Docker Hub auth service.
#[derive(Deserialize)]
struct TokenResponse {
    /// Bearer token for the registry.
    token: String,
}

impl DigestResolver {
    /// Create a resolver with an empty cache.
    ///
    /// # Errors
    /// Returns an error if the HTTP client cannot be built.
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("rsbump/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("Failed to build HTTP client")?;
        Ok(Self {
            client,
            cache: Mutex::new(HashMap::new()),
            token: Mutex::new(None),
        })
    }

    /// Digest for the given `rust` image tag, or `None` if the tag does not exist.
    ///
    /// The cache mutex is released across network I/O so independent tags resolve concurrently,
    /// while cached results and errors avoid hammering the registry repeatedly.
    ///
    /// # Errors
    /// Returns an error if the registry cannot be reached or answers with an unexpected status.
    pub fn resolve(&self, tag: &str) -> Result<Option<String>> {
        {
            let cache = self.cache.lock().map_err(|_| anyhow!("Digest cache lock poisoned"))?;
            if let Some(cached) = cache.get(tag) {
                return cached.clone().map_err(|message| anyhow!("{message}"));
            }
        }

        let fetched = self.fetch_digest(tag);
        let cache_value = match &fetched {
            Ok(digest) => Ok(digest.clone()),
            Err(error) => Err(format!("{error:#}")),
        };
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(tag.to_string(), cache_value);
        }
        fetched
    }

    /// Get a cached bearer token or fetch a fresh one from the Docker Hub auth service.
    fn bearer_token(&self, force_refresh: bool) -> Result<String> {
        if !force_refresh
            && let Ok(guard) = self.token.lock()
            && let Some(token) = guard.as_ref()
        {
            return Ok(token.clone());
        }

        let token_body = self
            .client
            .get(TOKEN_URL)
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .context("Failed to get Docker Hub token")?
            .text()
            .context("Failed to read Docker Hub token")?;
        let token: TokenResponse = serde_json::from_str(&token_body).context("Failed to parse Docker Hub token")?;
        if let Ok(mut guard) = self.token.lock() {
            *guard = Some(token.token.clone());
        }
        Ok(token.token)
    }

    /// Ask the registry for the digest of one tag, retrying once with a refreshed token on 401.
    fn fetch_digest(&self, tag: &str) -> Result<Option<String>> {
        let mut token = self.bearer_token(false)?;
        let mut response = self
            .client
            .head(format!("{MANIFEST_URL}/{tag}"))
            .bearer_auth(&token)
            .header(reqwest::header::ACCEPT, MANIFEST_ACCEPT)
            .send()
            .with_context(|| format!("Failed to query rust:{tag}"))?;

        if response.status() == StatusCode::UNAUTHORIZED {
            token = self.bearer_token(true)?;
            response = self
                .client
                .head(format!("{MANIFEST_URL}/{tag}"))
                .bearer_auth(&token)
                .header(reqwest::header::ACCEPT, MANIFEST_ACCEPT)
                .send()
                .with_context(|| format!("Failed to query rust:{tag}"))?;
        }

        match response.status() {
            StatusCode::NOT_FOUND => Ok(None),
            status if status.is_success() => response
                .headers()
                .get("docker-content-digest")
                .and_then(|value| value.to_str().ok())
                .map(|digest| Some(digest.to_string()))
                .ok_or_else(|| anyhow!("Registry response for rust:{tag} has no digest")),
            status => Err(anyhow!("Registry answered {status} for rust:{tag}")),
        }
    }
}
