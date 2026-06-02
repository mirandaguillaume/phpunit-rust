//! Data-provider row enumerator. Runs `enumerate_providers.php` once
//! before the fork pool spawns workers, asking PHP how many rows each
//! `#[DataProvider]` / `@dataProvider` method produces. The runner uses
//! the result for LPT cost weighting and per-row dispatch.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::types::TestCase;

/// `(class, providerMethod) -> Some(row_count)` if PHP enumerated the
/// provider successfully, `None` if it threw or returned a non-iterable.
/// Callers fall back to single-bucket dispatch when the value is `None`.
pub type RowCounts = HashMap<(String, String), Option<usize>>;

/// Wire shape returned by enumerate_providers.php. Each key is
/// `"ClassName::providerMethod"`; value is the row count or null.
#[derive(Debug, Deserialize)]
struct EnumerateOutput(HashMap<String, Option<usize>>);

/// Build the unique `(class, providerMethod)` set from a flat list of test
/// cases. Multiple tests can share the same provider; we only enumerate
/// each once.
pub fn collect_provider_pairs(cases: &[TestCase]) -> Vec<(String, String)> {
    let mut seen: HashMap<(String, String), ()> = HashMap::new();
    for c in cases {
        if let Some(p) = &c.data_provider {
            seen.entry((c.class.clone(), p.clone())).or_insert(());
        }
    }
    seen.into_keys().collect()
}

/// Run `php enumerate_providers.php` with the given pairs piped to stdin.
/// On failure (PHP exits non-zero, stdout isn't valid JSON, etc.) returns
/// an empty map — every provider falls back to "unknown" and the runner
/// uses single-bucket dispatch. The cost is at worst a missed scheduling
/// optimisation, never a correctness issue.
pub fn enumerate(
    script: &Path,
    autoload: &Path,
    bootstrap: Option<&Path>,
    defines: &[[String; 2]],
    pairs: &[(String, String)],
) -> Result<RowCounts> {
    if pairs.is_empty() {
        return Ok(HashMap::new());
    }

    let mut cmd = Command::new("php");
    cmd.arg(script)
        .arg("--autoload")
        .arg(autoload)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(bs) = bootstrap {
        cmd.arg("--bootstrap").arg(bs);
    }
    if !defines.is_empty() {
        cmd.arg("--defines")
            .arg(serde_json::to_string(defines).context("serializing defines")?);
    }

    let mut child = cmd.spawn().context("spawning enumerate_providers.php")?;
    let payload: Vec<[&String; 2]> = pairs.iter().map(|(c, m)| [c, m]).collect();
    let json = serde_json::to_string(&payload).context("serializing provider pairs")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(json.as_bytes())
            .context("writing pairs to PHP stdin")?;
        // stdin is dropped here, closing it so PHP can exit its read loop.
    }

    let output = child
        .wait_with_output()
        .context("waiting for enumerate_providers.php")?;
    if !output.status.success() {
        return Err(anyhow!(
            "enumerate_providers.php exited with status {}",
            output.status
        ));
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let EnumerateOutput(map) = serde_json::from_str(text.trim())
        .with_context(|| format!("parsing enumerator output: {:?}", text))?;

    // Re-key by (class, method) tuple for the runner's use.
    let mut out: RowCounts = HashMap::with_capacity(map.len());
    for (k, v) in map {
        if let Some((cls, mth)) = k.split_once("::") {
            out.insert((cls.to_string(), mth.to_string()), v);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn case(class: &str, method: &str, dp: Option<&str>) -> TestCase {
        TestCase {
            file: PathBuf::from("/f.php"),
            class: class.to_string(),
            method: method.to_string(),
            data_provider: dp.map(String::from),
            groups: vec![],
            external_providers: vec![],
            is_tautological: false,
            has_lifecycle_overrides: false,
            depends_on: vec![],
            is_dispatch_safe: true,
            fingerprint: std::collections::HashSet::new(),
            is_stateful: false,
            is_isolated: false,
        }
    }

    #[test]
    fn collect_pairs_dedups_shared_providers() {
        let cases = vec![
            case("A", "t1", Some("provideX")),
            case("A", "t2", Some("provideX")), // same provider as t1 — dedup
            case("A", "t3", None),
            case("B", "t1", Some("provideY")),
        ];
        let pairs = collect_provider_pairs(&cases);
        assert_eq!(pairs.len(), 2);
        assert!(pairs.contains(&("A".into(), "provideX".into())));
        assert!(pairs.contains(&("B".into(), "provideY".into())));
    }

    #[test]
    fn enumerate_returns_empty_for_no_pairs() {
        // No PHP invocation — we never even spawn the child.
        let counts = enumerate(
            Path::new("/does/not/exist"),
            Path::new("/does/not/exist"),
            None,
            &[],
            &[],
        )
        .unwrap();
        assert!(counts.is_empty());
    }
}
