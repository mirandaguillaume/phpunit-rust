use crate::discovery::group_by_class;
use crate::fork_pool::PhpForkPool;
use crate::types::{BatchClass, BatchPlan, TestCase, TestOutcome, TestStatus};
use anyhow::Result;
use rayon::prelude::*;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub autoload:  PathBuf,
    pub bootstrap: Option<PathBuf>,
    pub filter:    Option<String>,
    pub defines:   Vec<[String; 2]>,
}

#[derive(Debug)]
pub struct Report {
    pub outcomes:          Vec<TestOutcome>,
    pub total_duration_ms: f64,
}

impl Report {
    pub fn count(&self, status: TestStatus) -> usize {
        self.outcomes.iter().filter(|o| o.status == status).count()
    }
    pub fn passed(&self)     -> usize { self.count(TestStatus::Pass) }
    pub fn failed(&self)     -> usize { self.count(TestStatus::Fail) }
    pub fn errored(&self)    -> usize { self.count(TestStatus::Error) }
    pub fn skipped(&self)    -> usize { self.count(TestStatus::Skipped) }
    pub fn incomplete(&self) -> usize { self.count(TestStatus::Incomplete) }
    pub fn risky(&self)      -> usize { self.count(TestStatus::Risky) }

    pub fn is_success(&self) -> bool {
        self.failed() == 0 && self.errored() == 0
    }
}

/// Run all cases through the fork pool.
///
/// Cases are distributed across pool slots without splitting any class across
/// multiple slots. @depends ordering is handled inside PHP by MethodPlanner.
/// Results are drained in parallel via rayon; `on_progress` is called for each
/// outcome as it arrives from any slot.
pub fn run(
    pool: &mut PhpForkPool,
    cases: Vec<TestCase>,
    cfg: &RunConfig,
    on_progress: impl Fn(&TestOutcome) + Sync,
) -> Result<Report> {
    let filtered: Vec<TestCase> = cases.into_iter()
        .filter(|c| match &cfg.filter {
            Some(f) => format!("{}::{}", c.class, c.method).contains(f.as_str()),
            None    => true,
        })
        .collect();

    let n = pool.len();
    let chunks = chunk_by_class(filtered, n);

    for (slot, chunk) in chunks.iter().enumerate() {
        let groups = group_by_class(chunk.clone());
        let classes: Vec<BatchClass> = groups.into_iter().map(|g| BatchClass {
            file:    g.file,
            class:   g.class,
            methods: g.methods,
        }).collect();
        pool.write_batch(slot, &BatchPlan {
            autoload:  cfg.autoload.clone(),
            bootstrap: cfg.bootstrap.clone(),
            defines:   cfg.defines.clone(),
            classes,
        })?;
    }

    let readers = pool.into_readers();
    let slot_outcomes: Vec<Vec<TestOutcome>> = readers
        .into_par_iter()
        .map(|mut reader| {
            let mut outcomes = Vec::new();
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            if let Ok(outcome) = serde_json::from_str::<TestOutcome>(trimmed) {
                                on_progress(&outcome);
                                outcomes.push(outcome);
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            outcomes
        })
        .collect();

    let mut outcomes: Vec<TestOutcome> = Vec::new();
    let mut total = 0.0f64;
    for mut batch in slot_outcomes {
        for o in &batch { total += o.duration_ms; }
        outcomes.append(&mut batch);
    }

    Ok(Report { outcomes, total_duration_ms: total })
}

/// Distribute `cases` across `n` slots without splitting any class.
/// Round-robin by class discovery order.
pub fn chunk_by_class(cases: Vec<TestCase>, n: usize) -> Vec<Vec<TestCase>> {
    let n = n.max(1);
    let mut class_order: Vec<String> = Vec::new();
    let mut by_class: HashMap<String, Vec<TestCase>> = HashMap::new();
    for c in cases {
        by_class.entry(c.class.clone())
            .or_insert_with(|| { class_order.push(c.class.clone()); Vec::new() })
            .push(c);
    }
    let mut slots: Vec<Vec<TestCase>> = vec![Vec::new(); n];
    for (i, class) in class_order.into_iter().enumerate() {
        if let Some(class_cases) = by_class.remove(&class) {
            slots[i % n].extend(class_cases);
        }
    }
    slots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_case(class: &str, method: &str) -> TestCase {
        TestCase {
            file:   PathBuf::from("/f.php"),
            class:  class.to_string(),
            method: method.to_string(),
        }
    }

    #[test]
    fn chunk_preserves_all_cases() {
        let cases = vec![
            make_case("A", "t1"), make_case("A", "t2"),
            make_case("B", "t1"),
            make_case("C", "t1"),
        ];
        let chunks = chunk_by_class(cases, 2);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks.iter().map(|c| c.len()).sum::<usize>(), 4);
    }

    #[test]
    fn chunk_never_splits_a_class() {
        let cases = vec![
            make_case("Alpha", "t1"), make_case("Alpha", "t2"), make_case("Alpha", "t3"),
            make_case("Beta", "t1"),
        ];
        let chunks = chunk_by_class(cases, 3);
        for chunk in &chunks {
            let alpha = chunk.iter().filter(|c| c.class == "Alpha").count();
            assert!(alpha == 0 || alpha == 3,
                "Alpha was split: found {alpha} in one slot");
        }
    }

    #[test]
    fn chunk_handles_more_slots_than_classes() {
        let cases = vec![make_case("Only", "t1")];
        let chunks = chunk_by_class(cases, 8);
        assert_eq!(chunks.len(), 8);
        assert_eq!(chunks.iter().filter(|c| !c.is_empty()).count(), 1);
    }
}
