//! Bumping Rust projects to the local Rust toolchain version.
//!
//! Finds Rust projects, sets `rust-version` in their manifests, pins `rust` Docker images to the same version,
//! and runs the optional cargo and git steps, recording each step in a per-project report.
//! Nothing here reads command line arguments or prints,
//! so other projects can use the components directly and decide how to show the reports.

#![cfg_attr(test, allow(clippy::panic_in_result_fn))]

pub mod discovery;
pub mod dockerfile;
pub mod manifest;
pub mod project;
pub mod registry;

pub use discovery::find_projects;
pub use manifest::{LocalToolchain, RustVersion};
pub use project::{
    BumpObserver, Options, Outcome, Report, ReportEntry, bump_projects, check_tools, ensure_none_failed,
};
