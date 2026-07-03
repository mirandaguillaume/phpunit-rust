//! Native mutation generation over the mago AST (V1 mutator set).
//!
//! `mutators` maps individual AST operator/literal nodes to their byte-range
//! replacement; higher tasks walk a file's AST and join the result with per-test
//! coverage. The byte offsets come straight from mago spans, so a mutation is a
//! splice of `source[start..end]` — no pretty-printer, no re-emission.
use std::path::PathBuf;

pub mod mutators;

/// One mutation: replace `file[start..end]` with `replacement`.
///
/// `start`/`end` are byte offsets into the file's source; `line` is 1-based (the
/// line the mutated token starts on); `mutator` is the Infection-compatible name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mutant {
    pub file: PathBuf,
    pub start: usize,
    pub end: usize,
    pub replacement: Vec<u8>,
    pub mutator: &'static str,
    pub line: u32,
}
