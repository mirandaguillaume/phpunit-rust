//! PHPUnit Clover XML output with per-test attribution.
//!
//! Standard Clover XML format used by PHPUnit and many CI tools. Each line
//! covered by tests gets a `<line>` element with hit count, plus child
//! `<testref>` elements naming the tests that covered it.

use crate::analyzer::Coverage;
use std::fmt::Write;

pub fn render(coverage: &Coverage) -> String {
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<coverage>\n  <project>\n");
    for (file, lines) in coverage {
        let _ = writeln!(s, "    <file name=\"{}\">", escape_xml(&file.display().to_string()));
        for (line, tests) in lines {
            let _ = writeln!(
                s,
                "      <line num=\"{}\" type=\"stmt\" count=\"{}\"/>",
                line, tests.len()
            );
            for t in tests {
                let _ = writeln!(s, "        <testref name=\"{}\"/>", escape_xml(&t.display()));
            }
        }
        s.push_str("    </file>\n");
    }
    s.push_str("  </project>\n</coverage>\n");
    s
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::TestId;
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[test]
    fn renders_clover_xml() {
        let mut cov: Coverage = HashMap::new();
        let mut lines = HashMap::new();
        lines.insert(47, vec![TestId { class: "T".into(), method: "testA".into(), data_set: None }]);
        cov.insert(PathBuf::from("src/U.php"), lines);
        let s = render(&cov);
        assert!(s.contains("<?xml"));
        assert!(s.contains("src/U.php"));
        assert!(s.contains("<line num=\"47\" type=\"stmt\" count=\"1\"/>"));
        assert!(s.contains("<testref name=\"T::testA\"/>"));
    }

    #[test]
    fn empty_coverage_yields_valid_xml() {
        let cov: Coverage = HashMap::new();
        let s = render(&cov);
        assert!(s.starts_with("<?xml version=\"1.0\""));
        assert!(s.contains("<coverage>"));
        assert!(s.contains("</coverage>"));
        // No <file> elements when empty.
        assert!(!s.contains("<file"));
    }

    #[test]
    fn escapes_xml_special_chars_in_filenames() {
        let mut cov: Coverage = HashMap::new();
        let lines = HashMap::new();
        cov.insert(PathBuf::from("src/Foo & Bar.php"), lines);
        let s = render(&cov);
        assert!(s.contains("Foo &amp; Bar.php"));
    }
}
