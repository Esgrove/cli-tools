//! Shared user configuration loading for the `cli-tools` binaries.
//!
//! Reads a single named section from `~/.config/cli-tools.toml` (or the path set in `CLI_TOOLS_CONFIG`),
//! returning `T::default()` when no config file or section exists and reporting clear read or parse errors otherwise.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;

/// Load and deserialize `[section]` from the active user config file.
///
/// Returns `T::default()` if the user config path cannot be resolved, the file does not exist,
/// or the section is absent from the file.
///
/// # Errors
/// Returns an error if the config file cannot be read or contains invalid TOML or unknown keys
/// (when `T` uses `#[serde(deny_unknown_fields)]`).
pub fn load_user_config_section<T: DeserializeOwned + Default>(section: &str) -> Result<T> {
    let Some(path) = crate::config_path() else {
        return Ok(T::default());
    };
    load_user_config_section_at(&path, section)
}

/// Load and deserialize `[section]` from the TOML config file at `path`.
///
/// # Errors
/// Returns an error if reading `path` fails for a reason other than `NotFound`,
/// or if parsing the TOML content or `[section]` fails.
pub fn load_user_config_section_at<T: DeserializeOwned + Default>(path: &Path, section: &str) -> Result<T> {
    match fs::read_to_string(path) {
        Ok(content) => parse_user_config_section(&content, section)
            .with_context(|| format!("Failed to parse config file {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("Failed to read config file {}", path.display())),
    }
}

/// Parse `[section]` from a TOML string.
///
/// Returns `T::default()` when `section` is not present in the document.
///
/// # Errors
/// Returns an error if `content` is not valid TOML or if `[section]` fails to deserialize into `T`.
pub fn parse_user_config_section<T: DeserializeOwned + Default>(content: &str, section: &str) -> Result<T> {
    let mut table: toml::Table = toml::from_str(content).context("Invalid TOML syntax")?;
    let Some(value) = table.remove(section) else {
        return Ok(T::default());
    };
    value
        .try_into()
        .with_context(|| format!("Invalid [{section}] configuration"))
}

#[cfg(test)]
mod test_config_loading {
    use super::*;
    use serde::Deserialize;
    use tempfile::TempDir;

    #[derive(Debug, Default, PartialEq, Eq, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SampleSection {
        #[serde(default)]
        enabled: bool,
        #[serde(default)]
        limit: usize,
    }

    #[test]
    fn missing_section_returns_default() {
        let parsed: SampleSection = parse_user_config_section("[other]\nvalue = 1\n", "sample").unwrap();
        assert_eq!(parsed, SampleSection::default());
    }

    #[test]
    fn parses_matching_section_values() {
        let parsed: SampleSection =
            parse_user_config_section("[sample]\nenabled = true\nlimit = 42\n", "sample").unwrap();
        assert_eq!(
            parsed,
            SampleSection {
                enabled: true,
                limit: 42
            }
        );
    }

    #[test]
    fn rejects_unknown_field_when_struct_denies_unknown_fields() {
        let err = parse_user_config_section::<SampleSection>("[sample]\nunknown_key = true\n", "sample")
            .expect_err("unknown key should fail");
        assert!(format!("{err:#}").contains("unknown_key"), "{err:#}");
    }

    #[test]
    fn missing_file_returns_default() {
        let dir = TempDir::new().unwrap();
        let parsed: SampleSection = load_user_config_section_at(&dir.path().join("missing.toml"), "sample").unwrap();
        assert_eq!(parsed, SampleSection::default());
    }

    #[test]
    fn invalid_toml_file_includes_path_in_error() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("broken.toml");
        fs::write(&file, "[sample\nbad = ").unwrap();

        let err = load_user_config_section_at::<SampleSection>(&file, "sample").expect_err("broken TOML should fail");
        assert!(format!("{err:#}").contains("broken.toml"), "{err:#}");
    }
}
