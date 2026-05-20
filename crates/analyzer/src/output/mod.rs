//! Output formats for coverage results.

pub mod pcov;
pub mod pcov_extended;
pub mod clover;
pub mod json;

use crate::analyzer::Coverage;

/// Output format for `pcov-rs analyze`.
#[derive(Debug, Clone, Copy)]
pub enum Format {
    /// Strict PCov: aggregate `{file: {line: 1|-1}}`. No per-test attribution.
    Pcov,
    /// Extended PCov: `{file: {line: [TestId, ...]}}`. Preserves per-test data.
    PcovExtended,
    /// PHPUnit Clover XML.
    Clover,
    /// Raw internal JSON shape (serde of Coverage).
    Json,
}

impl std::str::FromStr for Format {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pcov" => Ok(Format::Pcov),
            "pcov-extended" => Ok(Format::PcovExtended),
            "clover" => Ok(Format::Clover),
            "json" => Ok(Format::Json),
            other => Err(format!("unknown format: {other}")),
        }
    }
}

/// Render `coverage` in the requested format and return as a string.
pub fn render(format: Format, coverage: &Coverage) -> String {
    match format {
        Format::Pcov => pcov::render(coverage),
        Format::PcovExtended => pcov_extended::render(coverage),
        Format::Clover => clover::render(coverage),
        Format::Json => json::render(coverage),
    }
}
