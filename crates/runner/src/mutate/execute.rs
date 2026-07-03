//! File-swap executor: classify one mutant by running only its covering tests
//! against a patched, isolated copy of the project.
//!
//! V1 favours the *simplest correct* isolation — a full copy of the project so
//! concurrent mutants never race on the same source path, and a fresh PHP process
//! per mutant so opcache never serves the original's stale compile. The warm,
//! no-copy overlay is V2; it swaps only this stage, not the mutant set.
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

/// Run one mutant. Uncovered mutants short-circuit to `NotCovered` (no process);
/// otherwise the project is copied, the target file patched, and the covering
/// tests run via `php <phpunit> --configuration phpunit.xml --filter <regex>`.
pub fn run_one(
    project: &Path,
    php: &str,
    phpunit: &Path,
    planned: &PlannedMutant,
    timeout: Duration,
) -> MutantOutcome {
    let m = planned.mutant.clone();
    if planned.covering_tests.is_empty() {
        return outcome(m, MutantStatus::NotCovered);
    }

    let workdir = match copy_tree(project) {
        Ok(d) => d,
        // An infra failure (copy) is treated as caught rather than a false survivor.
        Err(_) => return outcome(m, MutantStatus::Killed),
    };
    let rel = m.file.strip_prefix(project).unwrap_or(&m.file);
    let target = workdir.path().join(rel);
    if apply_patch(&target, m.start, m.end, &m.replacement).is_err() {
        return outcome(m, MutantStatus::Killed);
    }

    // Prefer the COPY's own phpunit binary so its composer autoloader is the ONLY one
    // loaded. Running the original project's phpunit against the copy would bootstrap
    // the original vendor AND the copy's `bootstrap="vendor/autoload.php"`, redeclaring
    // composer's init class (a fatal that falsely "kills" every covered mutant). Fall
    // back to the caller's phpunit for projects whose copy has no vendor/bin/phpunit.
    let local_phpunit = workdir.path().join("vendor/bin/phpunit");
    let phpunit = if local_phpunit.is_file() {
        local_phpunit.as_path()
    } else {
        phpunit
    };
    let filter = build_filter(&planned.covering_tests);
    let status = run_phpunit(php, phpunit, workdir.path(), &filter, timeout);
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

/// Splice `file[start..end] = replacement` in place.
fn apply_patch(file: &Path, start: usize, end: usize, replacement: &[u8]) -> std::io::Result<()> {
    let mut bytes = std::fs::read(file)?;
    if start > end || end > bytes.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "patch span out of bounds",
        ));
    }
    bytes.splice(start..end, replacement.iter().copied());
    std::fs::write(file, bytes)
}

/// Recursively copy `project` into a fresh temp dir, skipping `.git`.
fn copy_tree(project: &Path) -> std::io::Result<tempfile::TempDir> {
    let dst = tempfile::TempDir::new()?;
    for entry in walkdir::WalkDir::new(project)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        let rel = match path.strip_prefix(project) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if rel.components().any(|c| c.as_os_str() == ".git") {
            continue;
        }
        let target = dst.path().join(rel);
        let ft = entry.file_type();
        if ft.is_symlink() {
            // Recreate symlinks (e.g. vendor/bin/phpunit -> ../phpunit/…) rather than
            // dereferencing them, so the copy has a working vendor/bin. Best-effort:
            // a single bad link must not abort the whole copy.
            if let Ok(link_target) = std::fs::read_link(path) {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                #[cfg(unix)]
                let _ = std::os::unix::fs::symlink(&link_target, &target);
            }
        } else if ft.is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if ft.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(path, &target)?;
        }
    }
    Ok(dst)
}

/// Run phpunit in `workdir`; exit 0 (all covering tests passed the mutant) => the
/// mutant Escaped; non-zero => Killed; over `timeout` => Timeout. Child output goes
/// to a readable file in the (temporary) workdir, never to /dev/null.
fn run_phpunit(
    php: &str,
    phpunit: &Path,
    workdir: &Path,
    filter: &str,
    timeout: Duration,
) -> MutantStatus {
    let log = match File::create(workdir.join("phpunit_output.txt")) {
        Ok(f) => f,
        Err(_) => return MutantStatus::Killed,
    };
    let err_log = match log.try_clone() {
        Ok(f) => f,
        Err(_) => return MutantStatus::Killed,
    };
    let mut child = match Command::new(php)
        .arg(phpunit)
        .arg("--configuration")
        .arg("phpunit.xml")
        .arg("--filter")
        .arg(filter)
        .arg("--do-not-cache-result")
        .current_dir(workdir)
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
        let out = run_one(&project, "php", &phpunit, &planned, Duration::from_secs(60));
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
        let out = run_one(&project, "php", &phpunit, &planned, Duration::from_secs(60));
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
            &planned,
            Duration::from_secs(60),
        );
        assert_eq!(out.status, MutantStatus::NotCovered);
    }
}
