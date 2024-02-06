use std::path::Path;
use walkdir::DirEntry;

/// Check if entry is a hidden file or directory (starts with '.')
pub fn is_hidden(entry: &DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|s| s.starts_with('.'))
        .unwrap_or(false)
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
