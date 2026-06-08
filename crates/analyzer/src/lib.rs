//! pcov-rs: PHP test coverage via static analysis.

pub mod analyzer;
pub mod boundary;
pub mod cache;
pub mod cli;
pub mod complexity;
pub mod concrete;
pub mod config;
pub mod mago_bridge;
pub mod opacity;
pub mod reduce;
pub mod output;
pub mod report;
pub mod test_discovery;
pub mod types;

// Convenience re-exports for external callers (e.g. phpunit-rust).
pub use analyzer::Coverage;
pub use cli::analyze::analyze_filtered;
pub use config::{parse as parse_config, ProjectConfig};
pub use output::{render, Format};
