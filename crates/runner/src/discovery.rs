//! Thin shim re-exporting the shared `discovery` crate.
//!
//! The runner originally owned a 750+ line tree-sitter discovery module;
//! it now lives in `crates/discovery` so the analyzer can consume the
//! same parsing surface. This module preserves the historical import
//! path (`proust::discovery::*`) so external callers and tests
//! don't have to switch their use-statements.

pub use discovery::{
    discover_cases_and_test_index, discover_class_file_index, discover_class_file_index_targeted,
    discover_in_dir, discover_in_dirs, discover_in_file, discover_nontest_class_index,
    discover_with_index, format_shared_fixture_report, group_by_class,
    shared_fixture_report_in_dir, shared_fixture_report_in_file, GroupedMethod,
    SharedFixtureReport, TestCase, TestClass,
};
