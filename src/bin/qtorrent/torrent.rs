//! Torrent file parsing module.
//!
//! Provides structs and functions to parse `.torrent` files and extract metadata.

use std::borrow::Cow;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
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
    /// Directory names to skip (lowercase for case-insensitive full name matching).
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

    /// Calculate SHA-1 info hash directly from raw torrent bytes.
    ///
    /// This extracts the original `info` dictionary bytes and hashes them,
    /// which is more reliable than re-serializing the parsed struct.
    ///
    /// # Errors
    /// Returns an error if the info dictionary cannot be found.
    pub fn info_hash_from_bytes(buffer: &[u8]) -> Result<Vec<u8>> {
        let info_bytes = extract_info_dict_bytes(buffer)?;
        let hash: Vec<u8> = Sha1::digest(info_bytes).to_vec();
        Ok(hash)
    }

    /// Get the info hash as a hex string from raw torrent bytes.
    ///
    /// # Errors
    /// Returns an error if the info hash cannot be calculated.
    pub fn info_hash_hex_from_bytes(buffer: &[u8]) -> Result<String> {
        let hash = Self::info_hash_from_bytes(buffer)?;
        Ok(to_hex(&hash))
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

        // Check directory names (full match only, not the filename itself)
        // Path format in torrents is "dir1/dir2/filename.ext"
        let path = Path::new(path_lower.as_str());
        if let Some(parent) = path.parent() {
            for component in parent.components() {
                if let std::path::Component::Normal(dir_name) = component {
                    let dir_name_str = dir_name.to_string_lossy();
                    if self.skip_names.iter().any(|skip| skip == dir_name_str.as_ref()) {
                        return Some(format!("directory: {dir_name_str}"));
                    }
                }
            }
        }

        // Check extension
        if let Some(extension) = Path::new(file.path.as_ref()).extension() {
            let ext_lower = extension.to_string_lossy().to_lowercase();
            if self.skip_extensions.contains(&ext_lower) {
                return Some(format!("extension: .{ext_lower}"));
            }
        }

        // Check minimum size
        if let Some(min_size) = self.min_size_bytes
            && let Some(min_size_mb) = self.min_size_mb
            && file.size < min_size
        {
            return Some(format!("size {} < {min_size_mb} MB", cli_tools::format_size(file.size)));
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

/// Extract the raw `info` dictionary bytes from a torrent file.
///
/// This finds the `info` key in the top-level dictionary and returns
/// the raw bytes of the value (the info dictionary), which can then
/// be hashed to get the correct info hash.
///
/// # Errors
/// Returns an error if the info dictionary cannot be found.
fn extract_info_dict_bytes(buffer: &[u8]) -> Result<&[u8]> {
    // Torrent files are bencoded. The top level is a dictionary.
    // We need to find "4:info" key and extract its value bytes.

    // Find "4:info" in the buffer
    let info_key = b"4:info";
    let info_pos = find_subsequence(buffer, info_key).context("Could not find 'info' key in torrent file")?;

    // The info dictionary starts right after "4:info"
    let info_start = info_pos + info_key.len();

    if info_start >= buffer.len() {
        bail!("Torrent file truncated after 'info' key");
    }

    // Parse the bencode value to find its end
    let info_len = bencode_value_length(&buffer[info_start..])?;
    let info_end = info_start + info_len;

    if info_end > buffer.len() {
        bail!("Info dictionary extends beyond end of file");
    }

    Ok(&buffer[info_start..info_end])
}

/// Find the position of a subsequence in a byte slice.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// Calculate the length of a bencoded value starting at the given position.
///
/// # Errors
/// Returns an error if the bencode is malformed.
fn bencode_value_length(data: &[u8]) -> Result<usize> {
    if data.is_empty() {
        bail!("Empty bencode value");
    }

    match data[0] {
        // Integer: i<number>e
        b'i' => {
            let end = find_subsequence(data, b"e").context("Malformed bencode integer")?;
            Ok(end + 1)
        }
        // List: l<items>e
        b'l' => {
            let mut pos = 1;
            while pos < data.len() && data[pos] != b'e' {
                let item_len = bencode_value_length(&data[pos..])?;
                pos += item_len;
            }
            if pos >= data.len() {
                bail!("Malformed bencode list");
            }
            Ok(pos + 1) // +1 for the 'e'
        }
        // Dictionary: d<key><value>...e
        b'd' => {
            let mut pos = 1;
            while pos < data.len() && data[pos] != b'e' {
                // Key (must be a string)
                let key_len = bencode_value_length(&data[pos..])?;
                pos += key_len;
                // Value
                let value_len = bencode_value_length(&data[pos..])?;
                pos += value_len;
            }
            if pos >= data.len() {
                bail!("Malformed bencode dictionary");
            }
            Ok(pos + 1) // +1 for the 'e'
        }
        // String: <length>:<content>
        b'0'..=b'9' => {
            let colon_pos = find_subsequence(data, b":").context("Malformed bencode string")?;
            let len_str = std::str::from_utf8(&data[..colon_pos]).context("Invalid length in bencode string")?;
            let str_len: usize = len_str.parse().context("Invalid length number in bencode string")?;
            Ok(colon_pos + 1 + str_len)
        }
        other => bail!("Unknown bencode type: {}", other as char),
    }
}

#[cfg(test)]
mod test_to_hex {
    use super::*;

    #[test]
    fn empty_bytes() {
        assert_eq!(to_hex(&[]), "");
    }

    #[test]
    fn single_byte_zero() {
        assert_eq!(to_hex(&[0x00]), "00");
    }

    #[test]
    fn single_byte_max() {
        assert_eq!(to_hex(&[0xff]), "ff");
    }

    #[test]
    fn multiple_bytes() {
        assert_eq!(
            to_hex(&[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]),
            "0123456789abcdef"
        );
    }

    #[test]
    fn sha1_hash_length() {
        // SHA-1 produces 20 bytes = 40 hex characters
        let hash = vec![
            0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55, 0xbf, 0xef, 0x95, 0x60, 0x18, 0x90, 0xaf, 0xd8,
            0x07, 0x09,
        ];
        let hex = to_hex(&hash);
        assert_eq!(hex.len(), 40);
        assert_eq!(hex, "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }
}

#[cfg(test)]
mod test_find_subsequence {
    use super::*;

    #[test]
    fn finds_at_start() {
        let haystack = b"hello world";
        let needle = b"hello";
        assert_eq!(find_subsequence(haystack, needle), Some(0));
    }

    #[test]
    fn finds_at_end() {
        let haystack = b"hello world";
        let needle = b"world";
        assert_eq!(find_subsequence(haystack, needle), Some(6));
    }

    #[test]
    fn finds_in_middle() {
        let haystack = b"hello world";
        let needle = b"lo wo";
        assert_eq!(find_subsequence(haystack, needle), Some(3));
    }

    #[test]
    fn not_found() {
        let haystack = b"hello world";
        let needle = b"xyz";
        assert_eq!(find_subsequence(haystack, needle), None);
    }

    #[test]
    fn empty_haystack() {
        let haystack = b"";
        let needle = b"hello";
        assert_eq!(find_subsequence(haystack, needle), None);
    }

    #[test]
    fn finds_bencode_info_key() {
        let data = b"d8:announce35:http://tracker.example.com/announce4:infod4:name4:test12:piece lengthi16384eee";
        assert_eq!(find_subsequence(data, b"4:info"), Some(49));
    }
}

#[cfg(test)]
mod test_bencode_value_length {
    use super::*;

    #[test]
    fn integer_positive() {
        let data = b"i42e";
        assert_eq!(bencode_value_length(data).expect("should parse"), 4);
    }

    #[test]
    fn integer_negative() {
        let data = b"i-123e";
        assert_eq!(bencode_value_length(data).expect("should parse"), 6);
    }

    #[test]
    fn integer_zero() {
        let data = b"i0e";
        assert_eq!(bencode_value_length(data).expect("should parse"), 3);
    }

    #[test]
    fn string_simple() {
        let data = b"4:test";
        assert_eq!(bencode_value_length(data).expect("should parse"), 6);
    }

    #[test]
    fn string_empty() {
        let data = b"0:";
        assert_eq!(bencode_value_length(data).expect("should parse"), 2);
    }

    #[test]
    fn string_longer() {
        let data = b"11:hello world";
        assert_eq!(bencode_value_length(data).expect("should parse"), 14);
    }

    #[test]
    fn list_empty() {
        let data = b"le";
        assert_eq!(bencode_value_length(data).expect("should parse"), 2);
    }

    #[test]
    fn list_with_items() {
        let data = b"l4:spam4:eggse";
        assert_eq!(bencode_value_length(data).expect("should parse"), 14);
    }

    #[test]
    fn list_nested() {
        let data = b"ll4:testee";
        assert_eq!(bencode_value_length(data).expect("should parse"), 10);
    }

    #[test]
    fn dict_empty() {
        let data = b"de";
        assert_eq!(bencode_value_length(data).expect("should parse"), 2);
    }

    #[test]
    fn dict_with_items() {
        let data = b"d3:cow3:moo4:spam4:eggse";
        assert_eq!(bencode_value_length(data).expect("should parse"), 24);
    }

    #[test]
    fn dict_nested() {
        let data = b"d4:infod4:name4:testee";
        assert_eq!(bencode_value_length(data).expect("should parse"), 22);
    }

    #[test]
    fn error_on_empty() {
        let data = b"";
        assert!(bencode_value_length(data).is_err());
    }

    #[test]
    fn error_on_unknown_type() {
        let data = b"x";
        assert!(bencode_value_length(data).is_err());
    }
}

#[cfg(test)]
mod test_file_filter {
    use super::*;

    fn make_file_info(path: &str, size: u64) -> FileInfo<'static> {
        FileInfo {
            index: 0,
            path: Cow::Owned(path.to_string()),
            size,
            exclusion_reason: None,
        }
    }

    #[test]
    fn excludes_by_extension() {
        let skip_extensions = vec!["txt".to_string(), "nfo".to_string()];
        let filter = FileFilter::new(&skip_extensions, &[], None);

        let file = make_file_info("movie/sample.txt", 1000);
        let reason = filter.should_exclude(&file);

        assert!(reason.is_some());
        assert!(reason.unwrap().contains("extension"));
    }

    #[test]
    fn excludes_by_directory_name() {
        let skip_names = vec!["sample".to_string()];
        let filter = FileFilter::new(&[], &skip_names, None);

        let file = make_file_info("movie/sample/video.mp4", 1_000_000);
        let reason = filter.should_exclude(&file);

        assert!(reason.is_some());
        assert!(reason.unwrap().contains("directory"));
    }

    #[test]
    fn excludes_by_size() {
        let min_size = 10 * 1024 * 1024; // 10 MB
        let filter = FileFilter::new(&[], &[], Some(min_size));

        let file = make_file_info("movie/small.mp4", 1_000_000); // 1 MB
        let reason = filter.should_exclude(&file);

        assert!(reason.is_some());
        assert!(reason.unwrap().contains("size"));
    }

    #[test]
    fn includes_when_no_filter_matches() {
        let skip_extensions = vec!["txt".to_string()];
        let skip_names = vec!["sample".to_string()];
        let min_size = 1024;
        let filter = FileFilter::new(&skip_extensions, &skip_names, Some(min_size));

        let file = make_file_info("movie/video.mp4", 1_000_000);
        let reason = filter.should_exclude(&file);

        assert!(reason.is_none());
    }

    #[test]
    fn is_empty_returns_true_for_no_filters() {
        let filter = FileFilter::new(&[], &[], None);
        assert!(filter.is_empty());
    }

    #[test]
    fn is_empty_returns_false_with_extension_filter() {
        let skip_extensions = vec!["txt".to_string()];
        let filter = FileFilter::new(&skip_extensions, &[], None);
        assert!(!filter.is_empty());
    }

    #[test]
    fn is_empty_returns_false_with_size_filter() {
        let filter = FileFilter::new(&[], &[], Some(1024));
        assert!(!filter.is_empty());
    }
}

#[cfg(test)]
mod test_filtered_files {
    use super::*;

    fn make_file_info(path: &str, size: u64) -> FileInfo<'static> {
        FileInfo {
            index: 0,
            path: Cow::Owned(path.to_string()),
            size,
            exclusion_reason: None,
        }
    }

    #[test]
    fn calculates_included_size() {
        let filtered = FilteredFiles {
            included: vec![make_file_info("a.mp4", 1000), make_file_info("b.mp4", 2000)],
            excluded: vec![make_file_info("c.txt", 500)],
        };

        assert_eq!(filtered.included_size(), 3000);
    }

    #[test]
    fn calculates_excluded_size() {
        let filtered = FilteredFiles {
            included: vec![make_file_info("a.mp4", 1000)],
            excluded: vec![make_file_info("b.txt", 500), make_file_info("c.nfo", 300)],
        };

        assert_eq!(filtered.excluded_size(), 800);
    }

    #[test]
    fn handles_empty_lists() {
        let filtered = FilteredFiles::default();

        assert_eq!(filtered.included_size(), 0);
        assert_eq!(filtered.excluded_size(), 0);
    }
}
