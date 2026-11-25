//! Shared `dirmove` library code.
//!
//! Contains the resolved options, root handling, file and directory scanning,
//! name matching, prefix grouping, unpacking, directory merging,
//! and reliable file moving used by the `dirmove` binary and benchmarks.

#![cfg_attr(test, allow(clippy::panic_in_result_fn))]

pub mod config;
pub mod grouping;
pub mod matcher;
pub mod merge;
pub mod mover;
pub mod name_filter;
pub mod prefix_index;
pub mod roots;
pub mod scanner;
pub mod types;
pub mod unpack;
pub mod utils;

#[cfg(test)]
mod test_helpers;

pub use config::{Config, CustomMapping};
pub use grouping::PrefixGrouper;
pub use matcher::{DirectoryMatcher, FileMatch, ListedFile, NormalizedFilename};
pub use merge::{DirectoryMerger, MergeEvent};
pub use mover::*;
pub use name_filter::NameFilter;
pub use prefix_index::*;
pub use roots::Roots;
pub use scanner::{DirectoryScan, Scanner};
pub use types::*;
pub use unpack::{EntryMove, UnpackCandidates, Unpacker};
pub use utils::*;
