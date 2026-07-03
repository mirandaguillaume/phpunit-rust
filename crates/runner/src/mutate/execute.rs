//! Overlay executor: classify one mutant by running only its covering tests with
//! the mutated file overlaid onto the ORIGINAL project — no per-mutant project copy.
//!
//! The mutated bytes go to a temp file that a `-d auto_prepend_file` shim `require`s
//! (after registering composer's autoloader) so the mutated class is declared BEFORE
//! the autoloader would load the original — composer then skips the identically-named
//! original. The project itself is only read, so mutants parallelize without racing on
//! a shared path, and the multi-thousand-file vendor copy the earlier file-swap paid
//! per mutant is gone. Same mutants and same killed/escaped classification (the
//! Infection oracle gate guards it) — only faster.
use analyzer::mutate::plan::PlannedMutant;
use analyzer::mutate::Mutant;
use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How a mutant fared against its covering tests.
#[derive(Debug, PartialEq, Eq)]
pub enum MutantStatus {
    /// A covering test failed/errored on the mutated code — the mutant was caught.
    Killed,
    /// Every covering test still passed — the mutant survived (a test gap).
    Escaped,
    /// The run exceeded the per-mutant timeout (counted as caught, like Infection).
    Timeout,
    /// No test covers the mutated line — nothing to run.
    NotCovered,
}

pub struct MutantOutcome {
    pub mutant: Mutant,
    pub status: MutantStatus,
}

/// Run one mutant via an OVERLAY (no project copy). The mutated file's bytes are
/// written to a temp file and pre-declared through PHP's `auto_prepend_file` before
/// the autoloader runs, so composer skips the (identically-named) original class.
/// The covering tests run against the ORIGINAL project read-only, so mutants
/// parallelize without racing on a shared source path — and no multi-thousand-file
/// vendor copy is paid per mutant (the V1 cost this replaces).
pub fn run_one(
    project: &Path,
    php: &str,
    phpunit: &Path,
    config: &Path,
    planned: &PlannedMutant,
    timeout: Duration,
) -> MutantOutcome {
    let m = planned.mutant.clone();
    if planned.covering_tests.is_empty() {
        return outcome(m, MutantStatus::NotCovered);
    }

    // Build the mutated bytes in memory — the real file is never touched.
    let orig = match std::fs::read(&m.file) {
        Ok(b) => b,
        Err(_) => return outcome(m, MutantStatus::Killed),
    };
    if m.start > m.end || m.end > orig.len() {
        return outcome(m, MutantStatus::Killed);
    }
    let mut mutated = orig;
    mutated.splice(m.start..m.end, m.replacement.iter().copied());

    let tmp = match tempfile::TempDir::new() {
        Ok(d) => d,
        Err(_) => return outcome(m, MutantStatus::Killed),
    };
    let mutant_file = tmp.path().join("mutant.php");
    if std::fs::write(&mutant_file, &mutated).is_err() {
        return outcome(m, MutantStatus::Killed);
    }
    // Prepend shim: register composer's autoloader (if any), then declare the mutated
    // class FIRST so the autoloader never loads the original definition.
    let autoload = project.join("vendor/autoload.php");
    let shim = tmp.path().join("prepend.php");
    let shim_src = format!(
        "<?php\nif (is_file({a})) {{ require {a}; }}\nrequire {mf};\n",
        a = php_string(&autoload),
        mf = php_string(&mutant_file),
    );
    if std::fs::write(&shim, shim_src).is_err() {
        return outcome(m, MutantStatus::Killed);
    }

    let filter = build_filter(&planned.covering_tests);
    let status = run_phpunit(
        php,
        phpunit,
        project,
        config,
        &shim,
        &filter,
        timeout,
        tmp.path(),
    );
    outcome(m, status)
}

fn outcome(mutant: Mutant, status: MutantStatus) -> MutantOutcome {
    MutantOutcome { mutant, status }
}

/// A PHPUnit `--filter` regex selecting exactly the covering tests by full name.
/// PHPUnit matches the regex against `Class::method[#dataset]`, so we alternate the
/// full ids (namespace `\` escaped for PCRE) rather than bare method names — a bare
/// `^method$` matches nothing, and an unqualified method risks hitting a same-named
/// test in another class.
fn build_filter(tests: &[String]) -> String {
    let alts: Vec<String> = tests.iter().map(|t| t.replace('\\', "\\\\")).collect();
    format!("({})", alts.join("|"))
}

/// A PHP double-quoted string literal for a filesystem path (escape `\` and `"`).
fn php_string(p: &Path) -> String {
    let s = p
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("\"{s}\"")
}

/// Run the covering tests against the ORIGINAL project with the mutant overlaid via
/// `-d auto_prepend_file=<shim>`. Exit 0 (all covering tests passed) => Escaped;
/// non-zero => Killed; over `timeout` => Timeout. Child output goes to a readable
/// file in `log_dir` (a temp dir), never to /dev/null.
#[allow(clippy::too_many_arguments)]
fn run_phpunit(
    php: &str,
    phpunit: &Path,
    project: &Path,
    config: &Path,
    shim: &Path,
    filter: &str,
    timeout: Duration,
    log_dir: &Path,
) -> MutantStatus {
    let log = match File::create(log_dir.join("phpunit_output.txt")) {
        Ok(f) => f,
        Err(_) => return MutantStatus::Killed,
    };
    let err_log = match log.try_clone() {
        Ok(f) => f,
        Err(_) => return MutantStatus::Killed,
    };
    let mut child = match Command::new(php)
        .arg("-d")
        .arg(format!("auto_prepend_file={}", shim.display()))
        .arg(phpunit)
        .arg("--configuration")
        .arg(config)
        .arg("--filter")
        .arg(filter)
        .arg("--do-not-cache-result")
        .current_dir(project)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err_log))
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return MutantStatus::Killed,
    };

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                return if st.success() {
                    MutantStatus::Escaped
                } else {
                    MutantStatus::Killed
                };
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return MutantStatus::Timeout;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return MutantStatus::Killed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use analyzer::mutate::plan::PlannedMutant;
    use analyzer::mutate::Mutant;
    use std::path::PathBuf;
    use std::time::Duration;

    fn repo_phpunit() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../php/vendor/bin/phpunit")
    }
    fn fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/mutation_sample")
    }

    // `$a + $b` -> `$a - $b`: testAdd expects 3, gets -1 -> the covering test must fail -> Killed.
    #[test]
    fn arithmetic_mutant_is_killed() {
        let phpunit = repo_phpunit();
        if !phpunit.exists() {
            eprintln!("SKIP: repo php/vendor phpunit not installed");
            return;
        }
        let project = fixture();
        let calc = project.join("src/Calc.php");
        let src = std::fs::read(&calc).unwrap();
        let plus = src.iter().position(|&c| c == b'+').unwrap();
        let planned = PlannedMutant {
            mutant: Mutant {
                file: calc.clone(),
                start: plus,
                end: plus + 1,
                replacement: b"-".to_vec(),
                mutator: "Plus",
                line: 11,
            },
            covering_tests: vec!["Sample\\Tests\\CalcTest::testAdd".to_string()],
        };
        let out = run_one(
            &project,
            "php",
            &phpunit,
            &project.join("phpunit.xml"),
            &planned,
            Duration::from_secs(60),
        );
        assert_eq!(out.status, MutantStatus::Killed);
    }

    // A covering test that still passes the (no-op) mutation means the mutant Escaped.
    #[test]
    fn noop_mutation_escapes() {
        let phpunit = repo_phpunit();
        if !phpunit.exists() {
            eprintln!("SKIP: repo php/vendor phpunit not installed");
            return;
        }
        let project = fixture();
        let calc = project.join("src/Calc.php");
        let src = std::fs::read(&calc).unwrap();
        // Replace a space with a space: semantically identical -> testAdd still passes.
        let sp = src.iter().position(|&c| c == b' ').unwrap();
        let planned = PlannedMutant {
            mutant: Mutant {
                file: calc.clone(),
                start: sp,
                end: sp + 1,
                replacement: b" ".to_vec(),
                mutator: "Plus",
                line: 1,
            },
            covering_tests: vec!["Sample\\Tests\\CalcTest::testAdd".to_string()],
        };
        let out = run_one(
            &project,
            "php",
            &phpunit,
            &project.join("phpunit.xml"),
            &planned,
            Duration::from_secs(60),
        );
        assert_eq!(out.status, MutantStatus::Escaped);
    }

    #[test]
    fn uncovered_mutant_is_not_run() {
        let project = fixture();
        let planned = PlannedMutant {
            mutant: Mutant {
                file: project.join("src/Calc.php"),
                start: 0,
                end: 1,
                replacement: b" ".to_vec(),
                mutator: "Plus",
                line: 1,
            },
            covering_tests: vec![],
        };
        let out = run_one(
            &project,
            "php",
            Path::new("php"),
            &project.join("phpunit.xml"),
            &planned,
            Duration::from_secs(60),
        );
        assert_eq!(out.status, MutantStatus::NotCovered);
    }
}
