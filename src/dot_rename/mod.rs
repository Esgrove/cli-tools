//! Dot rename module for formatting filenames with dot separators.
//!
//! This module provides functionality to rename files using dot-separated formatting,
//! with support for various transformations like date reordering, prefix/suffix addition,
//! and pattern-based replacements.

mod config;
mod format;
mod rename;

pub use config::{DotRenameConfig, DotsConfig};
pub use format::DotFormat;
pub use format::{collapse_consecutive_dots, collapse_consecutive_dots_in_place, remove_extra_dots};
pub use rename::DotRename;
