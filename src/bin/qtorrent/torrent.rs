//! Torrent file parsing module.
//!
//! Provides structs and functions to parse `.torrent` files and extract metadata.

use std::borrow::Cow;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use sha1::{Digest, Sha1};

use crate::config::Config;
use crate::utils::TorrentInfo;

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
pub struct FileFilter {
    /// File extensions to skip (lowercase, without dot).
    pub skip_extensions: Vec<String>,
    /// Directory names to skip (lowercase for case-insensitive full name matching).
    pub skip_directories: Vec<String>,
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
    pub fn filter_files(&self, filter: &FileFilter) -> FilteredFiles<'_> {
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

impl FileFilter {
    /// Create a new file filter from the given configuration.
    #[must_use]
    pub fn new(skip_extensions: Vec<String>, skip_directories: Vec<String>, min_size_bytes: Option<u64>) -> Self {
        let min_size_mb = min_size_bytes.map(|bytes| bytes / BYTES_PER_MB);
        Self {
            skip_extensions,
            skip_directories,
            min_size_bytes,
            min_size_mb,
        }
    }

    /// Check if any filters are configured.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.skip_extensions.is_empty() && self.skip_directories.is_empty() && self.min_size_bytes.is_none()
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
                    if self.skip_directories.iter().any(|skip| skip == dir_name_str.as_ref()) {
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
            return Some(format!("size < {min_size_mb} MB"));
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
    #[allow(unused)]
    pub fn excluded_size(&self) -> u64 {
        self.excluded.iter().map(|file| file.size).sum()
    }
}

/// Parse a single torrent file.
///
/// Applies file filtering and determines whether to treat this as a multi-file torrent
/// based on how many files will actually be included after filtering.
pub fn parse_torrent(path: &Path, config: &Config) -> Result<TorrentInfo> {
    let filter = &config.file_filter;
    let bytes = fs::read(path).context("Failed to read torrent file")?;
    let torrent = Torrent::from_buffer(&bytes)?;

    // Calculate info hash from raw bytes (not re-serialized) for correct hash
    let info_hash = Torrent::info_hash_hex_from_bytes(&bytes)?;

    let original_is_multi_file = torrent.is_multi_file();
    let original_name = torrent.name().map(String::from);

    // Filter files and determine effective multi-file status based on included files
    let (effective_is_multi_file, excluded_indices, single_included_file, effective_original_name, included_size) =
        if original_is_multi_file && !filter.is_empty() {
            let filtered = torrent.filter_files(filter);
            let excluded: Vec<usize> = filtered.excluded.iter().map(|file| file.index).collect();
            let included_size = filtered.included_size();
            // Treat as multi-file only if more than one file will be included
            let effective_multi = filtered.included.len() > 1;
            // If only one file remains, store its name for extension extraction
            // and use its path as the original name for renaming (since NoSubfolder is used)
            let (single_file, eff_name) = if filtered.included.len() == 1 {
                let file_path = filtered.included[0].path.to_string();
                (Some(file_path.clone()), Some(file_path))
            } else {
                (None, original_name)
            };
            (effective_multi, excluded, single_file, eff_name, included_size)
        } else {
            // No filtering applied - use original multi-file status
            (
                original_is_multi_file,
                Vec::new(),
                None,
                original_name,
                torrent.total_size(),
            )
        };

    Ok(TorrentInfo {
        path: path.to_path_buf(),
        torrent,
        bytes,
        info_hash,
        original_is_multi_file,
        effective_is_multi_file,
        rename_to: None,
        included_size,
        excluded_indices,
        single_included_file,
        original_name: effective_original_name,
        tags: config.resolve_tags(path),
    })
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
        let filter = FileFilter::new(vec!["txt".to_string(), "nfo".to_string()], vec![], None);

        let file = make_file_info("movie/sample.txt", 1000);
        let reason = filter.should_exclude(&file);

        assert!(reason.is_some());
        assert!(reason.unwrap().contains("extension"));
    }

    #[test]
    fn excludes_by_directory_name() {
        let filter = FileFilter::new(vec![], vec!["sample".to_string()], None);

        let file = make_file_info("movie/sample/video.mp4", 1_000_000);
        let reason = filter.should_exclude(&file);

        assert!(reason.is_some());
        assert!(reason.unwrap().contains("directory"));
    }

    #[test]
    fn excludes_by_size() {
        let min_size = 10 * 1024 * 1024; // 10 MB
        let filter = FileFilter::new(vec![], vec![], Some(min_size));

        let file = make_file_info("movie/small.mp4", 1_000_000); // 1 MB
        let reason = filter.should_exclude(&file);

        assert!(reason.is_some());
        assert!(reason.unwrap().contains("size"));
    }

    #[test]
    fn includes_when_no_filter_matches() {
        let filter = FileFilter::new(vec!["nfo".to_string()], vec!["sample".to_string()], Some(100));

        let file = make_file_info("movie/video.mp4", 1_000_000);
        let reason = filter.should_exclude(&file);

        assert!(reason.is_none());
    }

    #[test]
    fn is_empty_returns_true_for_no_filters() {
        let filter = FileFilter::default();
        assert!(filter.is_empty());
    }

    #[test]
    fn is_empty_returns_false_with_extension_filter() {
        let filter = FileFilter::new(vec!["txt".to_string()], vec![], None);
        assert!(!filter.is_empty());
    }

    #[test]
    fn is_empty_returns_false_with_size_filter() {
        let filter = FileFilter::new(vec![], vec![], Some(100));
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

#[cfg(test)]
mod test_torrent_parsing_from_file {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn read_dummy_torrent() -> Vec<u8> {
        fs::read(Path::new("tests/fixtures/dummy.torrent")).expect("Failed to read dummy.torrent")
    }

    #[test]
    fn parses_dummy_torrent_file() {
        let buffer = read_dummy_torrent();
        let torrent = Torrent::from_buffer(&buffer).expect("should parse torrent");

        assert!(torrent.info.name.is_some());
        assert_eq!(torrent.info.name.as_deref(), Some("dummy.txt"));
    }

    #[test]
    fn extracts_announce_url() {
        let buffer = read_dummy_torrent();
        let torrent = Torrent::from_buffer(&buffer).expect("should parse torrent");

        assert!(torrent.announce.is_some());
        assert!(torrent.announce.as_ref().unwrap().contains("tracker.opentrackr.org"));
    }

    #[test]
    fn extracts_announce_list() {
        let buffer = read_dummy_torrent();
        let torrent = Torrent::from_buffer(&buffer).expect("should parse torrent");

        assert!(torrent.announce_list.is_some());
        let announce_list = torrent.announce_list.as_ref().unwrap();
        assert!(!announce_list.is_empty());
    }

    #[test]
    fn extracts_comment() {
        let buffer = read_dummy_torrent();
        let torrent = Torrent::from_buffer(&buffer).expect("should parse torrent");

        assert!(torrent.comment.is_some());
        assert!(torrent.comment.as_ref().unwrap().contains("example.com"));
    }

    #[test]
    fn extracts_created_by() {
        let buffer = read_dummy_torrent();
        let torrent = Torrent::from_buffer(&buffer).expect("should parse torrent");

        assert!(torrent.created_by.is_some());
        assert!(torrent.created_by.as_ref().unwrap().contains("torrent-creator"));
    }

    #[test]
    fn extracts_creation_date() {
        let buffer = read_dummy_torrent();
        let torrent = Torrent::from_buffer(&buffer).expect("should parse torrent");

        assert!(torrent.creation_date.is_some());
        assert!(torrent.creation_date.unwrap() > 0);
    }

    #[test]
    fn extracts_file_length() {
        let buffer = read_dummy_torrent();
        let torrent = Torrent::from_buffer(&buffer).expect("should parse torrent");

        // Single file torrent has length in info
        assert!(torrent.info.length.is_some());
        assert_eq!(torrent.info.length, Some(7));
    }

    #[test]
    fn extracts_piece_length() {
        let buffer = read_dummy_torrent();
        let torrent = Torrent::from_buffer(&buffer).expect("should parse torrent");

        assert_eq!(torrent.info.piece_length, 16384);
    }

    #[test]
    fn extracts_private_flag() {
        let buffer = read_dummy_torrent();
        let torrent = Torrent::from_buffer(&buffer).expect("should parse torrent");

        assert!(torrent.info.private.is_some());
        assert_eq!(torrent.info.private, Some(1));
    }

    #[test]
    fn is_single_file_torrent() {
        let buffer = read_dummy_torrent();
        let torrent = Torrent::from_buffer(&buffer).expect("should parse torrent");

        assert!(!torrent.is_multi_file());
    }

    #[test]
    fn calculates_total_size() {
        let buffer = read_dummy_torrent();
        let torrent = Torrent::from_buffer(&buffer).expect("should parse torrent");

        assert_eq!(torrent.total_size(), 7);
    }

    #[test]
    fn gets_torrent_name() {
        let buffer = read_dummy_torrent();
        let torrent = Torrent::from_buffer(&buffer).expect("should parse torrent");

        assert_eq!(torrent.name(), Some("dummy.txt"));
    }

    #[test]
    fn gets_files_list_for_single_file() {
        let buffer = read_dummy_torrent();
        let torrent = Torrent::from_buffer(&buffer).expect("should parse torrent");

        let files = torrent.files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path.as_ref(), "dummy.txt");
        assert_eq!(files[0].size, 7);
        assert_eq!(files[0].index, 0);
    }

    #[test]
    fn calculates_info_hash() {
        let buffer = read_dummy_torrent();
        let hash = Torrent::info_hash_from_bytes(&buffer).expect("should calculate hash");

        // SHA-1 hash is 20 bytes
        assert_eq!(hash.len(), 20);
    }

    #[test]
    fn calculates_info_hash_hex() {
        let buffer = read_dummy_torrent();
        let hex = Torrent::info_hash_hex_from_bytes(&buffer).expect("should calculate hash");

        // SHA-1 hex is 40 characters
        assert_eq!(hex.len(), 40);
        // Should only contain hex characters
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn filter_files_with_no_filters() {
        let buffer = read_dummy_torrent();
        let torrent = Torrent::from_buffer(&buffer).expect("should parse torrent");

        let filter = FileFilter::default();
        let result = torrent.filter_files(&filter);

        assert_eq!(result.included.len(), 1);
        assert_eq!(result.excluded.len(), 0);
    }

    #[test]
    fn filter_files_excludes_by_extension() {
        let buffer = read_dummy_torrent();
        let torrent = Torrent::from_buffer(&buffer).expect("should parse torrent");

        let filter = FileFilter::new(vec!["txt".to_string()], vec![], None);
        let result = torrent.filter_files(&filter);

        assert_eq!(result.included.len(), 0);
        assert_eq!(result.excluded.len(), 1);
        assert!(
            result.excluded[0]
                .exclusion_reason
                .as_ref()
                .unwrap()
                .contains("extension")
        );
    }

    #[test]
    fn filter_files_excludes_by_size() {
        let buffer = read_dummy_torrent();
        let torrent = Torrent::from_buffer(&buffer).expect("should parse torrent");

        // File is 7 bytes, set minimum to 1MB
        let min_size = 1024 * 1024;
        let filter = FileFilter::new(vec![], vec![], Some(min_size));
        let result = torrent.filter_files(&filter);

        assert_eq!(result.included.len(), 0);
        assert_eq!(result.excluded.len(), 1);
        assert!(result.excluded[0].exclusion_reason.as_ref().unwrap().contains("size"));
    }
}

#[cfg(test)]
mod test_torrent_struct_methods {
    use super::*;

    #[test]
    fn from_buffer_fails_on_invalid_data() {
        let invalid_data = b"not a valid torrent";
        let result = Torrent::from_buffer(invalid_data);
        assert!(result.is_err());
    }

    #[test]
    fn info_hash_from_bytes_fails_without_info_dict() {
        let invalid_data = b"d8:announce5:test1e";
        let result = Torrent::info_hash_from_bytes(invalid_data);
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod test_extract_info_dict_bytes {
    use super::*;

    #[test]
    fn extracts_info_dict_from_valid_torrent() {
        let buffer = std::fs::read("tests/fixtures/dummy.torrent").expect("should read file");
        let result = extract_info_dict_bytes(&buffer);
        assert!(result.is_ok());
        let info_bytes = result.unwrap();
        // Info dict should start with 'd' (dictionary)
        assert_eq!(info_bytes[0], b'd');
    }

    #[test]
    fn fails_on_missing_info_key() {
        let data = b"d8:announce5:test1e";
        let result = extract_info_dict_bytes(data);
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod test_parse_torrent {
    use super::*;
    use clap::Parser;
    use std::path::Path;

    use crate::config::Config;

    fn dummy_torrent_path() -> &'static Path {
        Path::new("tests/fixtures/dummy.torrent")
    }

    fn default_config() -> Config {
        let args = crate::QtorrentArgs::try_parse_from(["test"]).expect("should parse default args");
        Config::from_args(args).expect("should create default config")
    }

    #[test]
    fn parses_valid_torrent_file() {
        let config = default_config();
        let result = parse_torrent(dummy_torrent_path(), &config);
        assert!(result.is_ok());
    }

    #[test]
    fn sets_path_correctly() {
        let config = default_config();
        let info = parse_torrent(dummy_torrent_path(), &config).expect("should parse");
        assert_eq!(info.path, dummy_torrent_path());
    }

    #[test]
    fn calculates_info_hash() {
        let config = default_config();
        let info = parse_torrent(dummy_torrent_path(), &config).expect("should parse");
        assert_eq!(info.info_hash.len(), 40);
        assert!(info.info_hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn stores_raw_bytes() {
        let config = default_config();
        let info = parse_torrent(dummy_torrent_path(), &config).expect("should parse");
        assert!(!info.bytes.is_empty());
    }

    #[test]
    fn single_file_torrent_not_multi_file() {
        let config = default_config();
        let info = parse_torrent(dummy_torrent_path(), &config).expect("should parse");
        assert!(!info.original_is_multi_file);
        assert!(!info.effective_is_multi_file);
    }

    #[test]
    fn calculates_included_size() {
        let config = default_config();
        let info = parse_torrent(dummy_torrent_path(), &config).expect("should parse");
        assert_eq!(info.included_size, 7);
    }

    #[test]
    fn no_excluded_indices_without_filter() {
        let config = default_config();
        let info = parse_torrent(dummy_torrent_path(), &config).expect("should parse");
        assert!(info.excluded_indices.is_empty());
    }

    #[test]
    fn sets_original_name() {
        let config = default_config();
        let info = parse_torrent(dummy_torrent_path(), &config).expect("should parse");
        assert_eq!(info.original_name, Some("dummy.txt".to_string()));
    }

    #[test]
    fn rename_to_is_none_initially() {
        let config = default_config();
        let info = parse_torrent(dummy_torrent_path(), &config).expect("should parse");
        assert!(info.rename_to.is_none());
    }

    #[test]
    fn single_included_file_is_none_for_single_file_torrent() {
        let config = default_config();
        let info = parse_torrent(dummy_torrent_path(), &config).expect("should parse");
        assert!(info.single_included_file.is_none());
    }

    #[test]
    fn fails_on_nonexistent_file() {
        let config = default_config();
        let result = parse_torrent(Path::new("nonexistent.torrent"), &config);
        assert!(result.is_err());
    }

    #[test]
    fn fails_on_invalid_torrent_data() {
        // Create a temp file with invalid data
        let temp_dir = std::env::temp_dir();
        let temp_path = temp_dir.join("invalid_test.torrent");
        std::fs::write(&temp_path, b"not a valid torrent").expect("should write temp file");

        let config = default_config();
        let result = parse_torrent(&temp_path, &config);

        std::fs::remove_file(&temp_path).ok();
        assert!(result.is_err());
    }

    #[test]
    fn filter_excludes_by_extension() {
        let mut config = default_config();
        config.file_filter.skip_extensions = vec!["txt".to_string()];
        let info = parse_torrent(dummy_torrent_path(), &config).expect("should parse");

        // Single file torrent with txt extension should have the file excluded
        // but since it's single-file, excluded_indices is only populated for multi-file
        // The included_size should still be the full size for single-file torrents
        assert_eq!(info.included_size, 7);
    }

    #[test]
    fn filter_by_size_on_single_file() {
        // File is 7 bytes, set minimum to 100 bytes
        let mut config = default_config();
        config.file_filter.min_size_bytes = Some(100);
        let info = parse_torrent(dummy_torrent_path(), &config).expect("should parse");

        // Single file torrent - size filter doesn't affect single-file torrents at parse time
        assert_eq!(info.included_size, 7);
    }
}

#[cfg(test)]
mod test_parse_torrent_all_files_excluded {
    use super::*;
    use clap::Parser;
    use serde_bytes::ByteBuf;

    use crate::config::Config;

    fn default_config() -> Config {
        let args = crate::QtorrentArgs::try_parse_from(["test"]).expect("should parse default args");
        Config::from_args(args).expect("should create default config")
    }

    /// Create a valid multi-file torrent file on disk and return its path.
    /// The torrent contains two `.txt` files, each 500 bytes.
    fn create_multi_file_torrent(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let torrent = Torrent {
            announce: Some("http://tracker.example.com/announce".to_string()),
            info: Info {
                name: Some("test_folder".to_string()),
                piece_length: 262_144,
                pieces: ByteBuf::from(vec![0u8; 20]),
                files: Some(vec![
                    File {
                        length: 500,
                        path: vec!["file1.txt".to_string()],
                        md5sum: None,
                    },
                    File {
                        length: 500,
                        path: vec!["file2.txt".to_string()],
                        md5sum: None,
                    },
                ]),
                ..Info::default()
            },
            ..Torrent::default()
        };

        let bytes = serde_bencode::to_bytes(&torrent).expect("should serialize torrent");
        let path = dir.join(name);
        std::fs::write(&path, &bytes).expect("should write torrent file");
        path
    }

    #[test]
    fn all_files_excluded_by_extension() {
        let temp_dir = tempfile::tempdir().expect("should create temp dir");
        let torrent_path = create_multi_file_torrent(temp_dir.path(), "test.torrent");

        let mut config = default_config();
        config.file_filter = FileFilter::new(vec!["txt".to_string()], vec![], None);

        let info = parse_torrent(&torrent_path, &config).expect("should parse");
        assert!(info.all_files_excluded());
        assert_eq!(info.excluded_indices.len(), 2);
        assert_eq!(info.included_size, 0);
    }

    #[test]
    fn all_files_excluded_by_size() {
        let temp_dir = tempfile::tempdir().expect("should create temp dir");
        let torrent_path = create_multi_file_torrent(temp_dir.path(), "test.torrent");

        let mut config = default_config();
        // Both files are 500 bytes; require at least 1 MB
        config.file_filter = FileFilter::new(vec![], vec![], Some(1024 * 1024));

        let info = parse_torrent(&torrent_path, &config).expect("should parse");
        assert!(info.all_files_excluded());
        assert_eq!(info.excluded_indices.len(), 2);
        assert_eq!(info.included_size, 0);
    }

    #[test]
    fn not_all_excluded_when_some_files_remain() {
        let temp_dir = tempfile::tempdir().expect("should create temp dir");

        // Create a torrent with one .txt and one .mp4 file
        let torrent = Torrent {
            announce: Some("http://tracker.example.com/announce".to_string()),
            info: Info {
                name: Some("mixed_folder".to_string()),
                piece_length: 262_144,
                pieces: ByteBuf::from(vec![0u8; 20]),
                files: Some(vec![
                    File {
                        length: 500,
                        path: vec!["readme.txt".to_string()],
                        md5sum: None,
                    },
                    File {
                        length: 1_000_000,
                        path: vec!["video.mp4".to_string()],
                        md5sum: None,
                    },
                ]),
                ..Info::default()
            },
            ..Torrent::default()
        };

        let bytes = serde_bencode::to_bytes(&torrent).expect("should serialize torrent");
        let torrent_path = temp_dir.path().join("mixed.torrent");
        std::fs::write(&torrent_path, &bytes).expect("should write torrent file");

        let mut config = default_config();
        config.file_filter = FileFilter::new(vec!["txt".to_string()], vec![], None);

        let info = parse_torrent(&torrent_path, &config).expect("should parse");
        assert!(!info.all_files_excluded());
        assert_eq!(info.excluded_indices.len(), 1);
        assert_eq!(info.included_size, 1_000_000);
    }

    #[test]
    fn not_all_excluded_without_filters() {
        let temp_dir = tempfile::tempdir().expect("should create temp dir");
        let torrent_path = create_multi_file_torrent(temp_dir.path(), "test.torrent");

        let mut config = default_config();
        config.file_filter = FileFilter::default();

        let info = parse_torrent(&torrent_path, &config).expect("should parse");
        assert!(!info.all_files_excluded());
        assert!(info.excluded_indices.is_empty());
        assert_eq!(info.included_size, 1000);
    }
}
