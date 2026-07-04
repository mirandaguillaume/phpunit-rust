//! Mutation Score Indicator (MSI) math + text report. Formulas match Infection's
//! definitions exactly so the oracle gate can compare numbers directly.
use super::{MutantOutcome, MutantStatus};

/// Tallied mutant outcomes.
#[derive(Debug, Default, Clone, Copy)]
pub struct Msi {
    pub killed: usize,
    pub escaped: usize,
    pub timeout: usize,
    pub not_covered: usize,
}

impl Msi {
    fn total(&self) -> usize {
        self.killed + self.escaped + self.timeout + self.not_covered
    }

    /// Infection MSI: detected / total. Uncovered mutants count against the score.
    pub fn msi(&self) -> f64 {
        let t = self.total();
        if t == 0 {
            return 100.0;
        }
        (self.killed + self.timeout) as f64 / t as f64 * 100.0
    }

    /// Infection "covered MSI": detected / covered (uncovered mutants excluded).
    pub fn covered_msi(&self) -> f64 {
        let covered = self.killed + self.timeout + self.escaped;
        if covered == 0 {
            return 100.0;
        }
        (self.killed + self.timeout) as f64 / covered as f64 * 100.0
    }
}

/// Tally a slice of outcomes into an `Msi`.
pub fn summarize(outcomes: &[MutantOutcome]) -> Msi {
    let mut m = Msi::default();
    for o in outcomes {
        match o.status {
            MutantStatus::Killed => m.killed += 1,
            MutantStatus::Escaped => m.escaped += 1,
            MutantStatus::Timeout => m.timeout += 1,
            MutantStatus::NotCovered => m.not_covered += 1,
        }
    }
    m
}

/// Human-readable report: the tally, both MSI figures, and the escaped mutants.
pub fn text_report(msi: &Msi, escaped: &[&MutantOutcome]) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(s, "Mutants: {}", msi.total());
    let _ = writeln!(
        s,
        "  killed: {}  escaped: {}  timeout: {}  not covered: {}",
        msi.killed, msi.escaped, msi.timeout, msi.not_covered
    );
    let _ = writeln!(
        s,
        "MSI: {:.2}%   Covered MSI: {:.2}%",
        msi.msi(),
        msi.covered_msi()
    );
    if !escaped.is_empty() {
        let _ = writeln!(s, "\nEscaped mutants:");
        for o in escaped {
            let _ = writeln!(
                s,
                "  {} {}:{}",
                o.mutant.mutator,
                o.mutant.file.display(),
                o.mutant.report_line
            );
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infection_msi_formulas() {
        // 6 killed, 2 escaped, 1 timeout, 1 not-covered -> total 10.
        let m = Msi {
            killed: 6,
            escaped: 2,
            timeout: 1,
            not_covered: 1,
        };
        // MSI = (killed+timeout)/total = 7/10 = 70.0
        assert!((m.msi() - 70.0).abs() < 1e-9);
        // Covered MSI = (killed+timeout)/(killed+timeout+escaped) = 7/9
        assert!((m.covered_msi() - (7.0 / 9.0 * 100.0)).abs() < 1e-9);
    }

    #[test]
    fn empty_run_is_100_percent() {
        let m = Msi::default();
        assert_eq!(m.msi(), 100.0);
        assert_eq!(m.covered_msi(), 100.0);
    }
}
