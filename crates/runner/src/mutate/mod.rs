//! Mutation orchestration (V1: file-swap + cold covering-test runs).
//!
//! Generation and planning live in the `analyzer` crate (where mago is wired);
//! this module drives the actual test runs that classify each mutant.
pub mod execute;

pub use execute::{run_one, MutantOutcome, MutantStatus};
