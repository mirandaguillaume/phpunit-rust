//! Risk report: combine per-method coverage with cyclomatic complexity.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::analyzer::Coverage;
use crate::complexity::MethodComplexity;

/// Per-method risk entry.
#[derive(Debug, Clone)]
pub struct MethodRisk {
    pub class: String,
    pub method: String,
    pub file: PathBuf,
    pub covered_lines: u32,
    pub total_lines: u32,
    pub coverage_pct: f64,
    pub cyclomatic: u32,
    /// risk = (1 − coverage_fraction) × cyclomatic
    pub risk: f64,
}

/// Build a risk report from coverage data and complexity measurements.
///
/// Only includes methods with at least one coverable line (total_lines > 0).
pub fn build(coverage: &Coverage, methods: &[MethodComplexity]) -> Vec<MethodRisk> {
    // Pre-index covered lines by file for O(1) lookup.
    let covered_by_file: HashMap<&PathBuf, &HashMap<u32, _>> =
        coverage.iter().map(|(f, lm)| (f, lm)).collect();

    let mut report: Vec<MethodRisk> = methods
        .iter()
        .filter_map(|m| {
            let total_lines = m.end_line.saturating_sub(m.start_line).saturating_add(1);
            if total_lines == 0 {
                return None;
            }

            let covered_lines = covered_by_file
                .get(&m.file)
                .map(|lm| {
                    (m.start_line..=m.end_line)
                        .filter(|l| lm.contains_key(l))
                        .count() as u32
                })
                .unwrap_or(0);

            let coverage_pct = (covered_lines as f64 / total_lines as f64) * 100.0;
            let risk = (1.0 - covered_lines as f64 / total_lines as f64) * m.cyclomatic as f64;

            Some(MethodRisk {
                class: m.class.clone(),
                method: m.method.clone(),
                file: m.file.clone(),
                covered_lines,
                total_lines,
                coverage_pct,
                cyclomatic: m.cyclomatic,
                risk,
            })
        })
        .collect();

    // Sort by risk descending, then by FQCN for stability.
    report.sort_by(|a, b| {
        b.risk
            .partial_cmp(&a.risk)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.class.cmp(&b.class))
            .then_with(|| a.method.cmp(&b.method))
    });

    report
}

/// Render as an aligned text table.
pub fn render_table(methods: &[MethodRisk], threshold: f64) -> String {
    let rows: Vec<&MethodRisk> = methods.iter().filter(|m| m.risk >= threshold).collect();
    if rows.is_empty() {
        return "No methods exceed the risk threshold.\n".to_string();
    }

    // Compute column widths.
    let method_col = rows
        .iter()
        .map(|r| format!("{}::{}", r.class, r.method).len())
        .max()
        .unwrap_or(6)
        .max(6);

    let file_col = rows
        .iter()
        .map(|r| r.file.display().to_string().len())
        .max()
        .unwrap_or(4)
        .max(4);

    let header = format!(
        "{:<6}  {:<5}  {:<2}  {:<method_col$}  {:<file_col$}",
        "RISK",
        "COV%",
        "CC",
        "METHOD",
        "FILE",
        method_col = method_col,
        file_col = file_col,
    );
    let sep = "-".repeat(header.len());

    let mut out = format!("{header}\n{sep}\n");
    for r in &rows {
        let fqcn = format!("{}::{}", r.class, r.method);
        out.push_str(&format!(
            "{:<6.1}  {:<5.1}  {:<2}  {:<method_col$}  {}\n",
            r.risk,
            r.coverage_pct,
            r.cyclomatic,
            fqcn,
            r.file.display(),
            method_col = method_col,
        ));
    }
    out
}

/// Render as JSON.
pub fn render_json(methods: &[MethodRisk], threshold: f64) -> String {
    let rows: Vec<serde_json::Value> = methods
        .iter()
        .filter(|m| m.risk >= threshold)
        .map(|r| {
            serde_json::json!({
                "class": r.class,
                "method": r.method,
                "file": r.file.display().to_string(),
                "covered_lines": r.covered_lines,
                "total_lines": r.total_lines,
                "coverage_pct": (r.coverage_pct * 10.0).round() / 10.0,
                "cyclomatic": r.cyclomatic,
                "risk": (r.risk * 10.0).round() / 10.0,
            })
        })
        .collect();
    serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".to_string())
}
