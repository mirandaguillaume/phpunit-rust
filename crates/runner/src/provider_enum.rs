//! Data-provider row counts.
//!
//! Historically the runner ran `enumerate_providers.php` before forking to
//! learn each provider's row count and stride-split the heavy ones across
//! workers. That enumerator was removed (see `main.rs`): in production it
//! produced no usable counts (it could not load `\PhpunitRust\TestExecutor`, so
//! every provider degraded to `null`), and once made to work, stride-splitting
//! measured net-slower on every OSS suite. The `RowCounts` map remains as the
//! (now always empty) input to `build_queue`, which dispatches every method as
//! a single bucket.

use std::collections::HashMap;

/// `(class, providerMethod) -> Some(row_count)` if a provider was enumerated,
/// `None` otherwise. Now always empty — kept so `build_queue`'s signature and
/// its single-bucket fallback path stay unchanged.
pub type RowCounts = HashMap<(String, String), Option<usize>>;
