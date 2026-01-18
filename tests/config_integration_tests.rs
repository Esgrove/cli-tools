//! Integration tests for config loading from fixture files.
//!
//! These tests verify that all config modules can parse the sample config file correctly.

use std::fs;
use std::path::Path;

/// Read the sample config file content.
fn read_sample_config() -> String {
    let config_path = Path::new("tests/fixtures/sample_config.toml");
    fs::read_to_string(config_path).expect("Failed to read sample config file")
}

#[test]
fn sample_config_file_exists() {
    let config_path = Path::new("tests/fixtures/sample_config.toml");
    assert!(config_path.exists(), "Sample config file should exist");
}

#[test]
fn sample_config_is_valid_toml() {
    let config_content = read_sample_config();
    let result: Result<toml::Value, _> = toml::from_str(&config_content);
    assert!(result.is_ok(), "Sample config should be valid TOML: {:?}", result.err());
}

#[test]
fn sample_config_has_all_sections() {
    let config_content = read_sample_config();
    let value: toml::Value = toml::from_str(&config_content).expect("should parse");

    let table = value.as_table().expect("should be a table");

    // Check all expected sections exist
    let expected_sections = [
        "dots",
        "flip_date",
        "dirmove",
        "dupefind",
        "qtorrent",
        "video_convert",
        "resolution",
        "thumbnail",
    ];

    for section in expected_sections {
        assert!(table.contains_key(section), "Config should have [{section}] section");
    }
}

#[test]
fn dots_section_has_expected_structure() {
    let config_content = read_sample_config();
    let value: toml::Value = toml::from_str(&config_content).expect("should parse");

    let dots = value.get("dots").expect("should have dots section");

    assert!(dots.get("date_starts_with_year").is_some());
    assert!(dots.get("move_to_start").is_some());
    assert!(dots.get("move_to_end").is_some());
    assert!(dots.get("include").is_some());
    assert!(dots.get("exclude").is_some());
    assert!(dots.get("replace").is_some());
    assert!(dots.get("regex_replace").is_some());
}

#[test]
fn qtorrent_section_has_expected_structure() {
    let config_content = read_sample_config();
    let value: toml::Value = toml::from_str(&config_content).expect("should parse");

    let qtorrent = value.get("qtorrent").expect("should have qtorrent section");

    assert!(qtorrent.get("host").is_some());
    assert!(qtorrent.get("port").is_some());
    assert!(qtorrent.get("username").is_some());
    assert!(qtorrent.get("password").is_some());
    assert!(qtorrent.get("save_path").is_some());
    assert!(qtorrent.get("skip_extensions").is_some());
    assert!(qtorrent.get("skip_names").is_some());
    assert!(qtorrent.get("min_file_size_mb").is_some());
}

#[test]
fn video_convert_section_has_expected_structure() {
    let config_content = read_sample_config();
    let value: toml::Value = toml::from_str(&config_content).expect("should parse");

    let video_convert = value.get("video_convert").expect("should have video_convert section");

    assert!(video_convert.get("bitrate").is_some());
    assert!(video_convert.get("max_bitrate").is_some());
    assert!(video_convert.get("min_duration").is_some());
    assert!(video_convert.get("max_duration").is_some());
    assert!(video_convert.get("sort").is_some());
}

#[test]
fn thumbnail_section_has_expected_structure() {
    let config_content = read_sample_config();
    let value: toml::Value = toml::from_str(&config_content).expect("should parse");

    let thumbnail = value.get("thumbnail").expect("should have thumbnail section");

    assert!(thumbnail.get("cols_landscape").is_some());
    assert!(thumbnail.get("rows_landscape").is_some());
    assert!(thumbnail.get("cols_portrait").is_some());
    assert!(thumbnail.get("rows_portrait").is_some());
    assert!(thumbnail.get("padding_landscape").is_some());
    assert!(thumbnail.get("padding_portrait").is_some());
    assert!(thumbnail.get("font_size").is_some());
    assert!(thumbnail.get("quality").is_some());
    assert!(thumbnail.get("scale_width").is_some());
}

#[test]
fn dirmove_section_has_expected_structure() {
    let config_content = read_sample_config();
    let value: toml::Value = toml::from_str(&config_content).expect("should parse");

    let dirmove = value.get("dirmove").expect("should have dirmove section");

    assert!(dirmove.get("auto").is_some());
    assert!(dirmove.get("create").is_some());
    assert!(dirmove.get("min_group_size").is_some());
    assert!(dirmove.get("prefix_ignores").is_some());
    assert!(dirmove.get("prefix_overrides").is_some());
    assert!(dirmove.get("unpack_directories").is_some());
}

#[test]
fn dupefind_section_has_expected_structure() {
    let config_content = read_sample_config();
    let value: toml::Value = toml::from_str(&config_content).expect("should parse");

    let dupefind = value.get("dupefind").expect("should have dupefind section");

    assert!(dupefind.get("extensions").is_some());
    assert!(dupefind.get("patterns").is_some());
    assert!(dupefind.get("paths").is_some());
    assert!(dupefind.get("default_paths").is_some());
}

#[test]
fn resolution_section_has_expected_structure() {
    let config_content = read_sample_config();
    let value: toml::Value = toml::from_str(&config_content).expect("should parse");

    let resolution = value.get("resolution").expect("should have resolution section");

    assert!(resolution.get("debug").is_some());
    assert!(resolution.get("delete_limit").is_some());
    assert!(resolution.get("dryrun").is_some());
    assert!(resolution.get("overwrite").is_some());
    assert!(resolution.get("recurse").is_some());
    assert!(resolution.get("verbose").is_some());
}

#[test]
fn flip_date_section_has_expected_structure() {
    let config_content = read_sample_config();
    let value: toml::Value = toml::from_str(&config_content).expect("should parse");

    let flip_date = value.get("flip_date").expect("should have flip_date section");

    assert!(flip_date.get("directory").is_some());
    assert!(flip_date.get("file_extensions").is_some());
    assert!(flip_date.get("swap_year").is_some());
    assert!(flip_date.get("year_first").is_some());
}

#[test]
fn config_values_have_correct_types() {
    let config_content = read_sample_config();
    let value: toml::Value = toml::from_str(&config_content).expect("should parse");

    // Check boolean types
    let dirmove = value.get("dirmove").expect("should have dirmove section");
    assert!(dirmove.get("auto").unwrap().is_bool());
    assert!(dirmove.get("verbose").unwrap().is_bool());

    // Check integer types
    let thumbnail = value.get("thumbnail").expect("should have thumbnail section");
    assert!(thumbnail.get("cols_landscape").unwrap().is_integer());
    assert!(thumbnail.get("quality").unwrap().is_integer());

    // Check float types
    let qtorrent = value.get("qtorrent").expect("should have qtorrent section");
    assert!(qtorrent.get("min_file_size_mb").unwrap().is_float());

    // Check string types
    assert!(qtorrent.get("host").unwrap().is_str());
    assert!(qtorrent.get("category").unwrap().is_str());

    // Check array types
    let dupefind = value.get("dupefind").expect("should have dupefind section");
    assert!(dupefind.get("extensions").unwrap().is_array());
    assert!(dupefind.get("patterns").unwrap().is_array());
}
