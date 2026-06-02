//! Parity test: pcov-rs static coverage vs PHP pcov runtime coverage.
//!
//! Disabled by default; enable with: PARITY_TEST=1
//!
//! Requires Docker (runs PHP 8.4 + pcov extension). Set PARITY_PROJECT_ROOT to a
//! PHP project with phpunit.xml (default: /tmp/doctrine-orm).
//!
//! Semantics:
//!   - False positive (pcov-rs says covered, PHP pcov says not) → analyzer bug
//!   - False negative (PHP pcov covered, pcov-rs missed) → acceptable opacity
//!
//! Outputs a report to stderr; does not hard-fail (proof instrument, not CI gate).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// file (relative to project root) → line_num → execution count
type CovMap = HashMap<PathBuf, HashMap<u32, u32>>;

#[test]
fn parity_with_php_pcov() {
    if std::env::var("PARITY_TEST").is_err() {
        eprintln!("skipping: PARITY_TEST not set");
        return;
    }

    let project_root = std::env::var("PARITY_PROJECT_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/doctrine-orm"));
    assert!(
        project_root.exists(),
        "project root {:?} not found; set PARITY_PROJECT_ROOT",
        project_root
    );

    let dockerfile = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Dockerfile.pcov");
    assert!(
        dockerfile.exists(),
        "Dockerfile.pcov not found at {:?}",
        dockerfile
    );

    eprintln!("[parity] building Docker image pcov-rs-parity ...");
    docker_build(&dockerfile);

    let php_xml = project_root.join("_parity_php.xml");
    eprintln!("[parity] running PHPUnit via Docker ...");
    docker_phpunit(&project_root, &php_xml);

    let rs_xml = project_root.join("_parity_rs.xml");
    eprintln!("[parity] running pcov-rs analyze ...");
    pcovrs_analyze(&project_root, &rs_xml);

    // PHP Clover paths are absolute inside the container: /app/src/Foo.php
    // pcov-rs Clover paths are absolute on the host: /tmp/doctrine-orm/src/Foo.php
    let php_cov = parse_clover(&php_xml, "/app");
    let rs_root_str = project_root.to_string_lossy().into_owned();
    let rs_cov = parse_clover(&rs_xml, &rs_root_str);

    let _ = std::fs::remove_file(&php_xml);
    let _ = std::fs::remove_file(&rs_xml);

    print_parity_report(&php_cov, &rs_cov);
}

// ── Docker / binary helpers ────────────────────────────────────────────────────

fn docker_build(dockerfile: &Path) {
    let context = dockerfile.parent().unwrap();
    let status = Command::new("docker")
        .args(["build", "-f"])
        .arg(dockerfile)
        .arg("-t")
        .arg("pcov-rs-parity")
        .arg(context)
        .status()
        .expect("'docker' not found in PATH");
    assert!(status.success(), "docker build failed");
}

fn docker_phpunit(project_root: &Path, output_xml: &Path) {
    let _ = std::fs::remove_file(output_xml);
    let xml_name = output_xml.file_name().unwrap().to_str().unwrap();

    // PHPUnit may exit non-zero (failing tests, strict mode warnings) — we don't
    // care about test results, only about the coverage file being produced.
    let _status = Command::new("docker")
        .args(["run", "--rm", "-w", "/app"])
        .arg("-v")
        .arg(format!("{}:/app", project_root.display()))
        .arg("pcov-rs-parity")
        // -d memory_limit=-1: PHPUnit coverage data is memory-intensive
        .args([
            "php",
            "-d",
            "memory_limit=-1",
            "vendor/bin/phpunit",
            "--coverage-clover",
        ])
        .arg(format!("/app/{xml_name}"))
        .status()
        .expect("docker run failed to start");

    assert!(
        output_xml.exists(),
        "PHPUnit did not produce coverage XML at {:?}",
        output_xml
    );
}

fn pcovrs_analyze(project_root: &Path, output_xml: &Path) {
    let bin = assert_cmd::cargo::cargo_bin("pcov-rs");
    let status = Command::new(bin)
        .arg("analyze")
        .arg("--config")
        .arg(project_root.join("phpunit.xml"))
        .arg("--format")
        .arg("clover")
        .arg("--output")
        .arg(output_xml)
        .status()
        .expect("pcov-rs binary not found");
    assert!(status.success(), "pcov-rs analyze failed");
}

// ── Clover XML parsing ─────────────────────────────────────────────────────────

/// Parse a Clover XML into `{relative_path → {line → count}}`.
///
/// `abs_prefix` is stripped from every file path to produce relative keys
/// (e.g. `/app` strips `/app/src/Foo.php` → `src/Foo.php`).
fn parse_clover(path: &Path, abs_prefix: &str) -> CovMap {
    let xml =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {:?}: {}", path, e));
    let doc = roxmltree::Document::parse(&xml)
        .unwrap_or_else(|e| panic!("invalid XML at {:?}: {}", path, e));

    let prefix = abs_prefix.trim_end_matches('/');
    let mut map: CovMap = HashMap::new();

    for file_node in doc.descendants().filter(|n| n.has_tag_name("file")) {
        let Some(name) = file_node.attribute("name") else {
            continue;
        };

        let rel = match name.strip_prefix(prefix) {
            Some(rest) => PathBuf::from(rest.trim_start_matches('/')),
            None => PathBuf::from(name),
        };

        let lines = map.entry(rel).or_default();
        for ln in file_node.children().filter(|n| n.has_tag_name("line")) {
            let Some(num) = ln.attribute("num").and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let count = ln
                .attribute("count")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);
            // Keep max if duplicate (shouldn't happen, but defensive).
            lines
                .entry(num)
                .and_modify(|c| *c = (*c).max(count))
                .or_insert(count);
        }
    }

    map
}

// ── Parity report ──────────────────────────────────────────────────────────────

fn print_parity_report(php_cov: &CovMap, rs_cov: &CovMap) {
    let mut tp: u64 = 0; // both covered
    let mut tn: u64 = 0; // both not covered
    let mut fp: u64 = 0; // rs covered, PHP not  ← over-reporting bug
    let mut fnn: u64 = 0; // PHP covered, rs not  ← acceptable gap

    let mut fp_by_file: Vec<(PathBuf, Vec<u32>)> = Vec::new();

    // PHP pcov is ground truth: iterate every line it knows is executable.
    for (file, php_lines) in php_cov {
        let rs_lines = rs_cov.get(file);
        let mut fp_here: Vec<u32> = Vec::new();

        for (&line_num, &php_count) in php_lines {
            let rs_count = rs_lines
                .and_then(|m| m.get(&line_num))
                .copied()
                .unwrap_or(0);
            match (php_count > 0, rs_count > 0) {
                (true, true) => tp += 1,
                (true, false) => fnn += 1,
                (false, true) => {
                    fp += 1;
                    fp_here.push(line_num);
                }
                (false, false) => tn += 1,
            }
        }

        if !fp_here.is_empty() {
            fp_here.sort_unstable();
            fp_by_file.push((file.clone(), fp_here));
        }
    }

    let total = tp + tn + fp + fnn;
    let pct = |n: u64| {
        if total > 0 {
            n as f64 / total as f64 * 100.0
        } else {
            0.0
        }
    };

    eprintln!("\n=== pcov-rs / PHP pcov parity report ===");
    eprintln!("PHP-executable lines:                   {total}");
    eprintln!(
        "True  positives  (both covered):         {tp}  ({:.1}%)",
        pct(tp)
    );
    eprintln!(
        "True  negatives  (both not covered):     {tn}  ({:.1}%)",
        pct(tn)
    );
    eprintln!(
        "False negatives  (rs under-reports):     {fnn}  ({:.1}%)  ← acceptable",
        pct(fnn)
    );
    eprintln!(
        "False positives  (rs over-reports):      {fp}  ({:.1}%)  ← bugs",
        pct(fp)
    );

    if !fp_by_file.is_empty() {
        fp_by_file.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
        eprintln!("\nTop false-positive files ({} total):", fp_by_file.len());
        for (file, lines) in fp_by_file.iter().take(10) {
            let preview: Vec<_> = lines.iter().take(5).collect();
            let ellipsis = if lines.len() > 5 { "…" } else { "" };
            eprintln!(
                "  {:?}  {} line(s): {:?}{}",
                file,
                lines.len(),
                preview,
                ellipsis
            );
        }
    } else {
        eprintln!("\nNo false positives detected.");
    }

    eprintln!("=========================================\n");
}
