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
    /// Digest per tag, `None` for tags that do not exist.
    cache: Mutex<HashMap<String, Option<String>>>,
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
        })
    }

    /// Digest for the given `rust` image tag, or `None` if the tag does not exist.
    ///
    /// The lock is held during the request, so a tag is only fetched once.
    ///
    /// # Errors
    /// Returns an error if the registry cannot be reached or answers with an unexpected status.
    pub fn resolve(&self, tag: &str) -> Result<Option<String>> {
        let mut cache = self.cache.lock().map_err(|_| anyhow!("Digest cache lock poisoned"))?;
        if let Some(digest) = cache.get(tag) {
            return Ok(digest.clone());
        }
        let digest = self.fetch_digest(tag)?;
        cache.insert(tag.to_string(), digest.clone());
        drop(cache);
        Ok(digest)
    }

    /// Ask the registry for the digest of one tag.
    fn fetch_digest(&self, tag: &str) -> Result<Option<String>> {
        let token_body = self
            .client
            .get(TOKEN_URL)
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .context("Failed to get Docker Hub token")?
            .text()
            .context("Failed to read Docker Hub token")?;
        let token: TokenResponse = serde_json::from_str(&token_body).context("Failed to parse Docker Hub token")?;

        let response = self
            .client
            .head(format!("{MANIFEST_URL}/{tag}"))
            .bearer_auth(token.token)
            .header(reqwest::header::ACCEPT, MANIFEST_ACCEPT)
            .send()
            .with_context(|| format!("Failed to query rust:{tag}"))?;

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
