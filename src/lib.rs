use anyhow::Context;
use walkdir::DirEntry;

use std::path::{Path, PathBuf};
use std::{env, fs};

/// Check if entry is a hidden file or directory (starts with '.')
pub fn is_hidden(entry: &DirEntry) -> bool {
    entry.file_name().to_str().map(|s| s.starts_with('.')).unwrap_or(false)
}

pub fn resolve_input_path(path: Option<String>) -> anyhow::Result<PathBuf> {
    let input_path = path.unwrap_or_default().trim().to_string();
    let filepath = if input_path.is_empty() {
        env::current_dir().context("Failed to get current working directory")?
    } else {
        PathBuf::from(input_path)
    };
    if !filepath.exists() {
        anyhow::bail!(
            "Input path does not exist or is not accessible: '{}'",
            filepath.display()
        );
    }
    let absolute_input_path = fs::canonicalize(filepath)?;
    Ok(absolute_input_path)
}

pub fn resolve_output_path(path: Option<String>, absolute_input_path: &Path) -> anyhow::Result<PathBuf> {
    let output_path = {
        let path = path.unwrap_or_default().trim().to_string();
        if path.is_empty() {
            if absolute_input_path.is_file() {
                absolute_input_path
                    .parent()
                    .context("Failed to get parent directory")?
                    .to_path_buf()
            } else {
                absolute_input_path.to_path_buf()
            }
        } else {
            Path::new(&path).to_path_buf()
        }
    };
    Ok(output_path)
}

pub fn get_relative_path_or_filename(full_path: &Path, root: &Path) -> String {
    match full_path.strip_prefix(root) {
        Ok(relative_path) => relative_path.display().to_string(),
        Err(_) => match full_path.file_name() {
            None => full_path.display().to_string(),
            Some(name) => name.to_string_lossy().to_string(),
        },
    }
}
