//! Shared `dirmove` library code.
//!
//! Contains reusable grouping types, filename matching utilities, and reliable
//! file moving helpers used by the `dirmove` binary and benchmarks.

pub mod mover;
pub mod types;
pub mod utils;

pub use mover::*;
pub use types::*;
pub use utils::*;
