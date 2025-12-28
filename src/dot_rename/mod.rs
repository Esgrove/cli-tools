//! Dot rename module for formatting filenames with dot separators.
//!
//! This module provides functionality to rename files using dot-separated formatting,
//! with support for various transformations like date reordering, prefix/suffix addition,
//! and pattern-based replacements.

mod config;
mod rename;

pub use config::{DotRenameConfig, DotsConfig};
pub use rename::DotRename;
