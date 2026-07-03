//! Mutation orchestration (V1: file-swap + cold covering-test runs).
//!
//! Generation and planning live in the `analyzer` crate (where mago is wired);
//! this module drives the actual test runs that classify each mutant.
pub mod execute;
pub mod report;

pub use execute::{run_one, MutantOutcome, MutantStatus};

use anyhow::{anyhow, Context, Result};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

/// Per-mutant timeout: a mutant that hangs is counted caught (Timeout), like Infection.
const MUTANT_TIMEOUT: Duration = Duration::from_secs(60);

/// Drive a full `proust --mutate` run over `project` and print an MSI report.
///
/// Pipeline: baseline (vanilla phpunit `--coverage-php`, must be green + a pcov/xdebug
/// driver) → per-test coverage map → generate mutants over `<source>` → plan against
/// coverage → run each mutant's covering tests → MSI. Returns a non-zero `ExitCode`
/// when `min_msi` is set and not met.
pub fn run_mutation(
    project: &Path,
    config: Option<&Path>,
    min_msi: Option<f64>,
    escaped_json: Option<&Path>,
    workers: Option<usize>,
) -> Result<ExitCode> {
    let phpunit = project.join("vendor/bin/phpunit");
    if !phpunit.is_file() {
        return Err(anyhow!(
            "phpunit not found at {} — run `composer install`",
            phpunit.display()
        ));
    }
    let config = resolve_config(project, config)?;
    let xml = std::fs::read_to_string(&config)
        .with_context(|| format!("reading {}", config.display()))?;
    let cfg_dir = config.parent().unwrap_or(project);
    // The phpunit.xml bootstrap (framework init) — run ONCE in the fork-master.
    let bootstrap: Option<PathBuf> = crate::phpunit_xml::parse_bootstrap(&xml).map(|b| {
        let p = PathBuf::from(&b);
        if p.is_absolute() {
            p
        } else {
            cfg_dir.join(b)
        }
    });
    let source_dirs = source_include_dirs(&xml, cfg_dir);
    if source_dirs.is_empty() {
        return Err(anyhow!(
            "no <source><include><directory> in {} — cannot pick files to mutate",
            config.display()
        ));
    }

    // Baseline coverage: vanilla phpunit records per-test data natively (it brackets
    // each test), exactly like Infection. A red suite or a missing driver aborts.
    let tmp = tempfile::TempDir::new().context("creating mutation temp dir")?;
    let baseline_cov = tmp.path().join("baseline.cov");
    let status = Command::new("php")
        .arg(&phpunit)
        .arg("--configuration")
        .arg(&config)
        .arg("--coverage-php")
        .arg(&baseline_cov)
        .arg("--do-not-cache-result")
        .current_dir(project)
        .status()
        .context("running the baseline suite")?;
    if !status.success() {
        return Err(anyhow!(
            "baseline suite is not green — mutation testing needs a passing suite"
        ));
    }
    if !baseline_cov.is_file() {
        return Err(anyhow!(
            "no coverage was produced — install the pcov (or xdebug) extension"
        ));
    }

    // Per-test coverage map via php/pertest_coverage.php.
    let pertest_script = crate::php_worker::find_pertest_coverage_script()?;
    let request = format!(
        r#"{{"files":["{}"]}}"#,
        baseline_cov.to_string_lossy().replace('\\', "\\\\")
    );
    let project_autoload = project.join("vendor/autoload.php");
    let json = run_php_with_stdin(&pertest_script, &project_autoload, &request)?;
    let cov = analyzer::mutate::coverage::PerTestCoverage::from_json(json.as_bytes())
        .context("parsing per-test coverage")?;

    // Generate mutants over every source .php file.
    let mut mutants = Vec::new();
    for dir in &source_dirs {
        for entry in walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("php") {
                continue;
            }
            if let Ok(src) = std::fs::read(p) {
                let abs = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
                mutants.extend(analyzer::mutate::generate_file(&abs, &src));
            }
        }
    }

    let planned = analyzer::mutate::plan::plan(mutants, &cov);

    let nthreads = workers.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    });

    // Prefer the warm fork-master (V3): boot the project once, fork per mutant — the
    // PHP + composer + PHPUnit bootstrap is paid once instead of per mutant. Fall back
    // to the per-process overlay (V2) when pcntl is unavailable or the master can't run.
    let outcomes: Vec<MutantOutcome> = match run_via_fork_master(
        project,
        bootstrap.as_deref(),
        &planned,
        nthreads,
        MUTANT_TIMEOUT.as_secs(),
    ) {
        Some(o) => o,
        None => {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(nthreads)
                .build()
                .context("building mutation thread pool")?;
            pool.install(|| {
                planned
                    .par_iter()
                    .map(|pm| run_one(project, "php", &phpunit, &config, pm, MUTANT_TIMEOUT))
                    .collect()
            })
        }
    };

    let msi = report::summarize(&outcomes);
    let escaped: Vec<&MutantOutcome> = outcomes
        .iter()
        .filter(|o| o.status == MutantStatus::Escaped)
        .collect();
    print!("{}", report::text_report(&msi, &escaped));

    if let Some(path) = escaped_json {
        write_escaped_json(path, &escaped)?;
    }

    if let Some(min) = min_msi {
        if msi.msi() < min {
            eprintln!("MSI {:.2}% is below the required {:.2}%", msi.msi(), min);
            return Ok(ExitCode::from(1));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The configured or autodetected phpunit config path.
fn resolve_config(project: &Path, config: Option<&Path>) -> Result<PathBuf> {
    if let Some(c) = config {
        return Ok(if c.is_absolute() {
            c.to_path_buf()
        } else {
            project.join(c)
        });
    }
    for name in ["phpunit.xml", "phpunit.dist.xml", "phpunit.xml.dist"] {
        let p = project.join(name);
        if p.is_file() {
            return Ok(p);
        }
    }
    Err(anyhow!("no phpunit.xml found in {}", project.display()))
}

/// `<directory>` entries inside the `<source><include>` block, resolved against
/// `cfg_dir`. Excludes are ignored in V1 (documented). A light scan — we only need
/// the include dirs, not a full XML model.
fn source_include_dirs(xml: &str, cfg_dir: &Path) -> Vec<PathBuf> {
    let Some(start) = xml.find("<source") else {
        return Vec::new();
    };
    let end = xml[start..]
        .find("</source>")
        .map(|e| start + e)
        .unwrap_or(xml.len());
    let block = &xml[start..end];
    let mut out = Vec::new();
    let mut rest = block;
    while let Some(open) = rest.find("<directory") {
        let after = &rest[open..];
        let Some(gt) = after.find('>') else { break };
        let Some(close) = after.find("</directory>") else {
            break;
        };
        let text = after[gt + 1..close].trim();
        if !text.is_empty() {
            let dir = cfg_dir.join(text);
            if dir.is_dir() {
                out.push(dir);
            }
        }
        rest = &after[close + "</directory>".len()..];
    }
    out
}

/// Run `php <script> <autoload>` feeding `input` on stdin; return stdout as a string.
/// `autoload` is passed as argv[1] so the script loads the project's classes.
fn run_php_with_stdin(script: &Path, autoload: &Path, input: &str) -> Result<String> {
    use std::io::Write;
    let mut child = Command::new("php")
        .arg(script)
        .arg(autoload)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning php {}", script.display()))?;
    child
        .stdin
        .take()
        .context("child stdin")?
        .write_all(input.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(anyhow!(
            "{} failed: {}",
            script.display(),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Write `{"escaped":[{"mutator","file","line"}]}` — consumed by the oracle gate.
fn write_escaped_json(path: &Path, escaped: &[&MutantOutcome]) -> Result<()> {
    let mut s = String::from("{\"escaped\":[");
    for (i, o) in escaped.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            r#"{{"mutator":"{}","file":"{}","line":{}}}"#,
            o.mutant.mutator,
            o.mutant.file.to_string_lossy().replace('\\', "\\\\"),
            o.mutant.line
        ));
    }
    s.push_str("]}");
    std::fs::write(path, s).with_context(|| format!("writing {}", path.display()))
}

/// True when this `php` has the pcntl extension (needed to fork the warm master).
fn pcntl_available() -> bool {
    Command::new("php")
        .arg("-r")
        .arg("echo function_exists('pcntl_fork') ? '1' : '0';")
        .output()
        .map(|o| o.stdout == b"1")
        .unwrap_or(false)
}

/// Group covering test ids (`Class::method[#dataset]`) into `[{class, methods}]`,
/// deduped — the shape `mutation_run.php` runs via `TestExecutor::runClass`.
fn group_covering(tests: &[String]) -> Vec<serde_json::Value> {
    use std::collections::BTreeMap;
    let mut by_class: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for t in tests {
        let Some((class, rest)) = t.split_once("::") else {
            continue;
        };
        let method = rest.split('#').next().unwrap_or(rest);
        by_class.entry(class).or_default().push(method);
    }
    by_class
        .into_iter()
        .map(|(class, mut methods)| {
            methods.sort_unstable();
            methods.dedup();
            serde_json::json!({ "class": class, "methods": methods })
        })
        .collect()
}

/// Warm fork-master execution (V3). Writes one overlay temp file per covered mutant
/// plus a jobs file, invokes `mutation_run.php` (boots once, forks per mutant), then
/// reads back each mutant's verdict. Returns `None` — so the caller falls back to the
/// per-process overlay — when pcntl is missing or the master cannot be launched/run.
fn run_via_fork_master(
    project: &Path,
    bootstrap: Option<&Path>,
    planned: &[analyzer::mutate::plan::PlannedMutant],
    workers: usize,
    timeout_secs: u64,
) -> Option<Vec<MutantOutcome>> {
    if !pcntl_available() {
        return None;
    }
    let script = crate::php_worker::find_mutation_run_script().ok()?;
    let workdir = tempfile::TempDir::new().ok()?;
    let results_dir = workdir.path().join("results");
    std::fs::create_dir_all(&results_dir).ok()?;

    // One overlay temp file + one job per covered mutant; the job id is its index in
    // `planned` so we can map verdicts straight back. Bad spans / unreadable files are
    // simply skipped here and read back as "killed" (their result file is absent).
    let mut jobs: Vec<serde_json::Value> = Vec::new();
    for (i, pm) in planned.iter().enumerate() {
        if pm.covering_tests.is_empty() {
            continue;
        }
        let m = &pm.mutant;
        let Ok(orig) = std::fs::read(&m.file) else {
            continue;
        };
        if m.start > m.end || m.end > orig.len() {
            continue;
        }
        let mut mutated = orig;
        mutated.splice(m.start..m.end, m.replacement.iter().copied());
        let file = workdir.path().join(format!("mutant_{i}.php"));
        if std::fs::write(&file, &mutated).is_err() {
            continue;
        }
        jobs.push(serde_json::json!({
            "id": i.to_string(),
            "file": file.to_string_lossy(),
            "covering": group_covering(&pm.covering_tests),
        }));
    }

    let jobs_file = workdir.path().join("jobs.json");
    std::fs::write(&jobs_file, serde_json::to_vec(&jobs).ok()?).ok()?;

    let autoload = project.join("vendor/autoload.php");
    let mut cmd = Command::new("php");
    cmd.arg(&script)
        .arg(format!("--autoload={}", autoload.display()))
        .arg(format!("--jobs={}", jobs_file.display()))
        .arg(format!("--results={}", results_dir.display()))
        .arg(format!("--workers={workers}"))
        .arg(format!("--timeout={timeout_secs}"))
        .current_dir(project);
    if let Some(b) = bootstrap {
        cmd.arg(format!("--bootstrap={}", b.display()));
    }
    let status = cmd.status().ok()?;
    if !status.success() {
        return None;
    }

    // Map each mutant's verdict back; a missing result file means the child died on
    // the mutant (fatal / crash) — counted as caught, like Infection.
    let outcomes = planned
        .iter()
        .enumerate()
        .map(|(i, pm)| {
            let status = if pm.covering_tests.is_empty() {
                MutantStatus::NotCovered
            } else {
                let verdict =
                    std::fs::read_to_string(results_dir.join(i.to_string())).unwrap_or_default();
                match verdict.trim() {
                    "escaped" => MutantStatus::Escaped,
                    "timeout" => MutantStatus::Timeout,
                    _ => MutantStatus::Killed,
                }
            };
            MutantOutcome {
                mutant: pm.mutant.clone(),
                status,
            }
        })
        .collect();
    Some(outcomes)
}
