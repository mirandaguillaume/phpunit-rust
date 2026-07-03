//! proust library surface.

// This crate orchestrates PHP workers via fork(2)/pipe(2)/signals and raw file
// descriptors — it is Unix-only by construction. Fail loudly with a clear
// message on non-Unix targets instead of emitting a wall of `std::os::unix`
// import errors. On Windows, build and run under WSL.
#[cfg(not(unix))]
compile_error!(
    "proust requires a Unix-like platform: it relies on fork/pipe/signals \
     which are unavailable on this target. On Windows, build and run under WSL."
);

pub mod components;
pub mod coverage;
pub mod coverage_runtime;
pub mod discovery;
pub mod fork_pool;
pub mod mock_bake;
#[cfg(feature = "coverage")]
pub mod mutate;
pub mod php_worker;
pub mod phpunit_xml;
pub mod profiler;
pub mod provider_enum;
pub mod reporter;
pub mod reports;
pub mod resource_lease;
pub mod runner;
pub mod types;
