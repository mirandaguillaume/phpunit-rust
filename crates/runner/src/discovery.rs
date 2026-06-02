//! Thin shim re-exporting the shared `discovery` crate.
//!
//! The runner originally owned a 750+ line tree-sitter discovery module;
//! it now lives in `crates/discovery` so the analyzer can consume the
//! same parsing surface. This module preserves the historical import
//! path (`phpunit_rust::discovery::*`) so external callers and tests
//! don't have to switch their use-statements.

pub use discovery::{
    discover_class_file_index, discover_class_file_index_targeted, discover_in_dir,
    discover_in_dirs, discover_in_file, discover_with_index, group_by_class, GroupedMethod,
    TestCase, TestClass,
};
