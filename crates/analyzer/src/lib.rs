//! pcov-rs: PHP test coverage via static analysis.

pub mod cli;
pub mod config;
pub mod boundary;
pub mod mago_bridge;
pub mod test_discovery;
pub mod cache;
pub mod opacity;
pub mod concrete;
pub mod analyzer;
pub mod output;
pub mod types;
pub mod complexity;
pub mod report;

// Convenience re-exports for external callers (e.g. phpunit-rust).
pub use config::{parse as parse_config, ProjectConfig};
pub use output::{render, Format};
pub use analyzer::Coverage;
pub use cli::analyze::analyze_filtered;
