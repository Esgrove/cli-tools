//! Shared `dirmove` library code.
//!
//! Contains root handling, file and directory scanning, name filtering and matching,
//! prefix grouping, unpacking, directory merging, and reliable file moving.
//! Nothing here reads command line arguments or prints,
//! so other projects can use the components directly.
//! Long running operations report their steps and progress through observer traits.

#![cfg_attr(test, allow(clippy::panic_in_result_fn))]

pub mod candidates;
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

pub use candidates::*;
pub use grouping::{GroupingOptions, GroupingProgress, GroupingStage, PrefixGrouper};
pub use matcher::{CustomMapping, DirectoryMatcher, FileMatch, ListedFile, NormalizedFilename};
pub use merge::{DirectoryMerger, MergeEvent, MergeObserver};
pub use mover::*;
pub use name_filter::NameFilter;
pub use prefix_index::*;
pub use roots::Roots;
pub use scanner::{DirectoryScan, ScanOptions, Scanner};
pub use types::*;
pub use unpack::{EntryMove, PlannedMove, UnpackCandidates, UnpackEvent, UnpackObserver, UnpackPlan, Unpacker};
pub use utils::*;
