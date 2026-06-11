//! Symbolic test reducer (increment 1).
//!
//! Reduces pure PHP tests to `Pass`/`Fail`/`Error` in Rust — evaluating the test
//! body and the application code it exercises over the test's concrete inputs —
//! instead of starting a PHP worker, while NEVER producing a result the real
//! runner wouldn't. Anything unmodelled becomes [`Outcome::Bailed`], never a guess
//! (fail-closed; see the design spec §5 and §12).
//!
//! # The principle
//!
//! A test, given its complete Givens (data-provider row + fixtures), is a TRIVIAL
//! deterministic computation. "Reduce" = perform that computation NATIVELY in Rust
//! on the first run, skipping the PHP VM (no startup / bootstrap / IPC). There is
//! no cache and no memoization — the win is first-run speed. The native evaluator
//! ([`eval`]) is the center; it computes the values. mago is an accelerator for
//! the reducibility decision and for resolving user-function calls, NOT the source
//! of computed values.
//!
//! # Module map
//! - [`value`] — the byte-backed [`Value`] + PHP conversions and PHP-8 comparisons.
//! - [`eval`] — the native evaluator: runs the trivial ops a test touches
//!   (arithmetic with PHP overflow→float, concat, comparisons, control flow, assert
//!   intrinsics → Pass/Fail) over the concrete Givens. Each op gold-tested vs
//!   `php -r`. Bails (fail-closed) on any op outside the modelled set.
//! - [`gate`] — the reducibility decision: are the Givens complete (pure, every
//!   operand concrete)? Uses mago's per-node types to decide; fail-closed on
//!   `mixed` / widened / unmodelled.
//! - [`driver`] — `reduce_file`: codebase build + per-test reduction + provider rows.
//!
//! # Fail-closed (spec §5)
//!
//! Reducible IFF the Givens are complete (the test is pure: no hidden time / DB /
//! random / network / global-state inputs) AND every op and value is modelled.
//! Anything else → `Outcome::Bailed(reason)` (defined in [`eval`]). The standing
//! differential (reduce vs the real runner) is the soundness backstop; it is
//! driven separately.

pub mod bridge_term;
pub mod driver;
pub mod egraph;
pub mod eval;
pub mod gate;
pub mod subst;
pub mod term;
pub mod value;
