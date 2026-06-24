use proust::fork_pool::PhpForkPool;
use proust::types::{BatchClass, BatchPlan, TestOutcome};
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::PathBuf;

/// Write a temp TestCase whose constructor captures and clears PROUST_DB_DSN
/// (before TestExecutor's dbHandle() tries to connect) and stores the value in an
/// instance property. The test then fails with the captured value so it is visible
/// in TestOutcome.message. Works on machines with no PDO drivers.
fn write_echo_test(dir: &std::path::Path) -> PathBuf {
    let file = dir.join("DsnEchoTest.php");
    // TestExecutor instantiates the class with `new $class($method)`, then calls
    // dbHandle() (line ~204). The constructor runs BEFORE dbHandle, so we capture
    // and clear the DSN there — dbHandle then sees an empty value and returns null.
    std::fs::write(
        &file,
        "<?php\n\
use PHPUnit\\Framework\\TestCase;\n\
class DsnEchoTest extends TestCase {\n\
    private string $capturedDsn = '';\n\
    public function __construct(?string $name = null, array $data = [], int|string $dataName = '') {\n\
        parent::__construct($name ?? 'testEchoDsn', $data, $dataName);\n\
        $raw = getenv('PROUST_DB_DSN');\n\
        $this->capturedDsn = $raw === false ? '' : $raw;\n\
        // Clear so TestExecutor's dbHandle() returns null (no PDO connection needed).\n\
        putenv('PROUST_DB_DSN=');\n\
        $_ENV['PROUST_DB_DSN'] = '';\n\
    }\n\
    public function testEchoDsn(): void {\n\
        $this->fail('DSN=' . var_export($this->capturedDsn, true));\n\
    }\n\
}\n",
    )
    .unwrap();
    file
}

fn plan_for(file: &std::path::Path) -> BatchPlan {
    BatchPlan {
        classes: vec![BatchClass {
            file: file.to_path_buf(),
            class: "DsnEchoTest".to_string(),
            methods: vec![],
            row_filter: None,
            required_files: vec![],
            is_isolated: false,
        }],
        fingerprint: HashSet::new(),
        force_exit_after: false,
    }
}

fn dsn_in(outcomes: &[TestOutcome]) -> String {
    let msg = outcomes
        .iter()
        .find(|o| o.class == "DsnEchoTest")
        .and_then(|o| o.message.clone())
        .expect("DsnEchoTest outcome with a message");
    let i = msg.find("DSN=").expect("DSN= marker in message");
    msg[i + "DSN=".len()..].trim().to_string()
}

/// DSN must be injected per-slot and DISTINCT across slots.
#[test]
fn per_slot_dsn_is_injected_and_distinct() {
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/sample_project");
    let autoload = project.join("vendor/autoload.php");
    let script = proust::php_worker::find_fork_script().expect("worker_fork.php");
    let dir = std::env::temp_dir().join(format!("proust_dsn_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = write_echo_test(&dir);

    // Use arbitrary distinct DSN-like strings. The constructor captures and
    // clears PROUST_DB_DSN before dbHandle() fires, so no PDO driver
    // is needed. Works on any machine.
    let dsns = vec![
        "pgsql:host=localhost;dbname=app_w0".to_string(),
        "pgsql:host=localhost;dbname=app_w1".to_string(),
    ];
    let mut pool = PhpForkPool::spawn(
        &script,
        &autoload,
        None,
        &[],
        &[],
        &[],
        &[],
        &[],
        2,
        &HashMap::new(),
        "512M",
        0,
        Some(&dsns),
        None,
    )
    .expect("spawn");

    pool.write_batch(0, &plan_for(&file)).unwrap();
    pool.write_batch(1, &plan_for(&file)).unwrap();
    pool.close_write_ends();

    let mut per_slot: Vec<String> = Vec::new();
    for mut reader in pool.into_readers() {
        let mut outs = Vec::new();
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            let t = line.trim();
            if !t.is_empty() {
                if let Ok(o) = serde_json::from_str::<TestOutcome>(t) {
                    outs.push(o);
                }
            }
            line.clear();
        }
        if !outs.is_empty() {
            per_slot.push(dsn_in(&outs));
        }
    }
    pool.wait();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(per_slot.len(), 2, "both slots produced an outcome");
    let distinct: HashSet<&String> = per_slot.iter().collect();
    assert_eq!(
        distinct.len(),
        2,
        "DSNs must be distinct per slot: {per_slot:?}"
    );
    // Each slot must see its own injected DSN value
    assert!(
        per_slot.iter().any(|d| d.contains("app_w0")),
        "slot 0 must see its DSN: {per_slot:?}"
    );
    assert!(
        per_slot.iter().any(|d| d.contains("app_w1")),
        "slot 1 must see its DSN: {per_slot:?}"
    );
}

/// When no DSNs are provided (empty), PROUST_DB_DSN must NOT be set —
/// behaviour byte-identical to pre-P3.
#[test]
fn per_slot_dsn_absent_when_none() {
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/sample_project");
    let autoload = project.join("vendor/autoload.php");
    let script = proust::php_worker::find_fork_script().expect("worker_fork.php");
    let dir = std::env::temp_dir().join(format!("proust_dsn_none_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = write_echo_test(&dir);

    let mut pool = PhpForkPool::spawn(
        &script,
        &autoload,
        None,
        &[],
        &[],
        &[],
        &[],
        &[],
        1,
        &HashMap::new(),
        "512M",
        0,
        None, // no DSNs
        None,
    )
    .expect("spawn");

    pool.write_batch(0, &plan_for(&file)).unwrap();
    pool.close_write_ends();

    let mut outs = Vec::new();
    for mut reader in pool.into_readers() {
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            let t = line.trim();
            if !t.is_empty() {
                if let Ok(o) = serde_json::from_str::<TestOutcome>(t) {
                    outs.push(o);
                }
            }
            line.clear();
        }
    }
    pool.wait();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(!outs.is_empty(), "slot 0 produced an outcome");
    let dsn_val = dsn_in(&outs);
    // When no DSN is injected, getenv() returns false; the constructor stores ''
    // in capturedDsn and var_export('', true) = "''". Either way it must not
    // contain any DSN-like content.
    assert!(
        dsn_val.is_empty() || dsn_val == "''" || dsn_val == "false",
        "PROUST_DB_DSN must NOT be set when per_slot_dsn is None; got: {dsn_val:?}"
    );
}
