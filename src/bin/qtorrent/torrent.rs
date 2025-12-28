//! Torrent file parsing module.
//!
//! Provides structs and functions to parse `.torrent` files and extract metadata.

use std::borrow::Cow;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_bencode::ser;
use serde_bytes::ByteBuf;
use sha1::{Digest, Sha1};

const HEX_CHARS: &[u8] = b"0123456789abcdef";
const BYTES_PER_MB: u64 = 1024 * 1024;

/// Represents a parsed `.torrent` file.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Torrent {
    #[serde(default)]
    pub announce: Option<String>,
    #[serde(default)]
    #[serde(rename = "announce-list")]
    pub announce_list: Option<Vec<Vec<String>>>,
    #[serde(rename = "comment")]
    pub comment: Option<String>,
    #[serde(default)]
    #[serde(rename = "created by")]
    pub created_by: Option<String>,
    #[serde(default)]
    #[serde(rename = "creation date")]
    pub creation_date: Option<i64>,
    #[serde(default)]
    pub encoding: Option<String>,
    pub info: Info,
    #[serde(default)]
    nodes: Option<Vec<Node>>,
    #[serde(default)]
    pub httpseeds: Option<Vec<String>>,
}

/// Contains metadata about the torrent content.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Info {
    #[serde(default)]
    pub files: Option<Vec<File>>,
    #[serde(default)]
    pub length: Option<i64>,
    #[serde(default)]
    pub md5sum: Option<String>,
    pub name: Option<String>,
    #[serde(default)]
    pub path: Option<Vec<String>>,
    #[serde(rename = "piece length")]
    pub piece_length: i64,
    #[serde(default)]
    pub pieces: ByteBuf,
    #[serde(default)]
    pub private: Option<u8>,
    #[serde(default)]
    #[serde(rename = "root hash")]
    pub root_hash: Option<String>,
}

/// Represents a file within a multi-file torrent.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct File {
    pub length: i64,
    pub path: Vec<String>,
    #[serde(default)]
    pub md5sum: Option<String>,
}

/// Node information for DHT.
#[derive(Debug, Deserialize, Serialize)]
struct Node(String, i64);

/// Information about a single file in a torrent.
#[derive(Debug, Clone)]
pub struct FileInfo<'a> {
    /// File index in the torrent.
    pub index: usize,
    /// Full path within the torrent.
    pub path: Cow<'a, str>,
    /// File size in bytes.
    pub size: u64,
    /// Reason for exclusion (if any).
    pub exclusion_reason: Option<String>,
}

/// File filter configuration.
#[derive(Debug, Default)]
pub struct FileFilter<'a> {
    /// File extensions to skip (lowercase, without dot).
    pub skip_extensions: &'a [String],
    /// File or folder names to skip (lowercase for case-insensitive matching).
    pub skip_names: &'a [String],
    /// Minimum file size in bytes.
    pub min_size_bytes: Option<u64>,
    /// Minimum file size in MB (pre-calculated for display).
    pub min_size_mb: Option<u64>,
}

/// Result of filtering files in a multi-file torrent.
#[derive(Debug, Default)]
pub struct FilteredFiles<'a> {
    /// Files that will be downloaded.
    pub included: Vec<FileInfo<'a>>,
    /// Files that will be skipped.
    pub excluded: Vec<FileInfo<'a>>,
}

impl Torrent {
    /// Create `Torrent` from bytes.
    ///
    /// # Errors
    /// Returns an error if the bytes cannot be parsed as a torrent.
    pub fn from_buffer(buffer: &[u8]) -> Result<Self> {
        serde_bencode::from_bytes(buffer).context("Failed to parse torrent file")
    }

    /// Check if this is a multi-file torrent.
    #[must_use]
    pub const fn is_multi_file(&self) -> bool {
        self.info.files.is_some()
    }

    /// Get the torrent name (internal name from the info dictionary).
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.info.name.as_deref()
    }

    /// Get total size of all files in the torrent.
    #[must_use]
    pub fn total_size(&self) -> u64 {
        self.info.files.as_ref().map_or_else(
            || self.info.length.unwrap_or(0) as u64,
            |files| files.iter().map(|file| file.length as u64).sum(),
        )
    }

    /// Get the list of files in a multi-file torrent.
    #[must_use]
    pub fn files(&self) -> Vec<FileInfo<'_>> {
        self.info.files.as_ref().map_or_else(
            || {
                // Single-file torrent
                vec![FileInfo {
                    index: 0,
                    path: Cow::Borrowed(self.info.name.as_deref().unwrap_or_default()),
                    size: self.info.length.unwrap_or(0) as u64,
                    exclusion_reason: None,
                }]
            },
            |files| {
                files
                    .iter()
                    .enumerate()
                    .map(|(index, file)| FileInfo {
                        index,
                        path: Cow::Owned(file.path.join("/")),
                        size: file.length as u64,
                        exclusion_reason: None,
                    })
                    .collect()
            },
        )
    }

    /// Filter files according to the given filter configuration.
    #[must_use]
    pub fn filter_files(&self, filter: &FileFilter<'_>) -> FilteredFiles<'_> {
        let mut result = FilteredFiles::default();

        for mut file_info in self.files() {
            if let Some(reason) = filter.should_exclude(&file_info) {
                file_info.exclusion_reason = Some(reason);
                result.excluded.push(file_info);
            } else {
                result.included.push(file_info);
            }
        }

        result
    }

    /// Calculate SHA-1 info hash.
    ///
    /// # Errors
    /// Returns an error if serialization fails.
    pub fn info_hash(&self) -> Result<Vec<u8>> {
        let info = ser::to_bytes(&self.info).context("Failed to serialize info dictionary")?;
        let info_hash: Vec<u8> = Sha1::digest(&info).to_vec();
        Ok(info_hash)
    }

    /// Get the info hash as a hex string.
    ///
    /// # Errors
    /// Returns an error if the info hash cannot be calculated.
    pub fn info_hash_hex(&self) -> Result<String> {
        let hash = self.info_hash()?;
        Ok(to_hex(&hash))
    }
}

impl<'a> FileFilter<'a> {
    /// Create a new file filter from the given configuration.
    #[must_use]
    pub fn new(skip_extensions: &'a [String], skip_names: &'a [String], min_size_bytes: Option<u64>) -> Self {
        let min_size_mb = min_size_bytes.map(|bytes| bytes / BYTES_PER_MB);
        Self {
            skip_extensions,
            skip_names,
            min_size_bytes,
            min_size_mb,
        }
    }

    /// Check if any filters are configured.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.skip_extensions.is_empty() && self.skip_names.is_empty() && self.min_size_bytes.is_none()
    }

    /// Check if a file should be excluded. Returns the reason if excluded.
    #[must_use]
    pub fn should_exclude(&self, file: &FileInfo<'_>) -> Option<String> {
        let path_lower = file.path.to_lowercase();

        // Check minimum size
        if let Some(min_size) = self.min_size_bytes
            && let Some(min_size_mb) = self.min_size_mb
            && file.size < min_size
        {
            return Some(format!("size {} < {min_size_mb} MB", cli_tools::format_size(file.size)));
        }

        // Check extension
        if let Some(extension) = Path::new(file.path.as_ref()).extension() {
            let ext_lower = extension.to_string_lossy().to_lowercase();
            if self.skip_extensions.contains(&ext_lower) {
                return Some(format!("extension: .{ext_lower}"));
            }
        }

        // Check file/folder names
        for skip_name in self.skip_names {
            if path_lower.contains(skip_name.as_str()) {
                return Some(format!("name matches: {skip_name}"));
            }
        }

        None
    }
}

impl FilteredFiles<'_> {
    /// Get the total size of included files.
    #[must_use]
    pub fn included_size(&self) -> u64 {
        self.included.iter().map(|file| file.size).sum()
    }

    /// Get the total size of excluded files.
    #[must_use]
    pub fn excluded_size(&self) -> u64 {
        self.excluded.iter().map(|file| file.size).sum()
    }
}

/// Convert bytes to a hex string.
#[must_use]
pub fn to_hex(bytes: &[u8]) -> String {
    let mut hex = Vec::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push(HEX_CHARS[(byte >> 4) as usize]);
        hex.push(HEX_CHARS[(byte & 0x0f) as usize]);
    }
    String::from_utf8(hex).expect("hex chars are valid UTF-8")
}
