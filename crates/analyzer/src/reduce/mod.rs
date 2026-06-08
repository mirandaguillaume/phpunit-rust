//! Symbolic test reducer (increment 1).
//!
//! Reduces pure PHP tests to `Pass`/`Fail`/`Error` in Rust — evaluating the test
//! body and the application code it exercises over the test's concrete inputs —
//! instead of starting a PHP worker, while NEVER producing a result the real
//! runner wouldn't. Anything unmodelled becomes [`Outcome::Bailed`], never a guess
//! (fail-closed; see the design spec §5 and §12).
//!
//! # Module map
//! - [`value`] — the byte-backed [`Value`] + PHP conversions and PHP-8 comparisons.
//! - [`eval`] — the concrete PHP-semantics evaluator (operator results computed
//!   OURSELVES, never lifted from mago's folded result nodes) + the cross-check.
//! - [`gate`] — the typed reducibility gate: lifts literals off mago's per-node
//!   `AnalysisArtifacts`, fail-closed on anything not a single concrete literal.
//! - [`driver`] — `reduce_file`: codebase build + per-test reduction + provider rows.
//!
//! # The load-bearing safety rules (spec §12.2)
//! 1. Read literals only off LEAF/OPERAND nodes from the artifact; compute every
//!    operator RESULT ourselves in PHP semantics. Never lift a folded result.
//! 2. Cross-check: when a node's inputs are concrete and we computed its result,
//!    if mago also folded it to a literal, compare and BailOut on divergence
//!    (catches mago's non-PHP int saturation).
//! 3. Gate only on the value-returning getters; missing key / mixed / non-single
//!    / non-whitelisted atomic = BailOut.

pub mod driver;
pub mod eval;
pub mod gate;
pub mod value;
