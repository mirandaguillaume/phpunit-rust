use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectConfig {
    pub root: PathBuf,
    pub test_suites: Vec<PathBuf>,
    pub source_includes: Vec<PathBuf>,
    pub source_excludes: Vec<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config file not found: {0}")]
    NotFound(PathBuf),
    #[error("config file unreadable: {0}")]
    Unreadable(#[from] std::io::Error),
    #[error("malformed phpunit.xml at {pos}: {msg}")]
    Malformed { pos: String, msg: String },
}

pub fn parse(path: &Path) -> Result<ProjectConfig, ConfigError> {
    if !path.exists() {
        return Err(ConfigError::NotFound(path.to_path_buf()));
    }
    let bytes = std::fs::read(path)?;
    let text = std::str::from_utf8(&bytes).map_err(|e| ConfigError::Malformed {
        pos: "0".into(),
        msg: format!("non-UTF8: {e}"),
    })?;

    let doc = roxmltree::Document::parse(text).map_err(|e| ConfigError::Malformed {
        pos: format!("{:?}", e.pos()),
        msg: e.to_string(),
    })?;

    // Canonicalize first so that relative config paths like "phpunit.xml" get
    // an absolute root. We fall back to the raw parent if canonicalize fails
    // (e.g., path doesn't exist yet — caught by the exists() check above).
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = abs.parent().unwrap_or(Path::new(".")).to_path_buf();
    let phpunit_node = doc.root_element();

    let mut test_suites = Vec::new();
    for suite in phpunit_node
        .descendants()
        .filter(|n| n.has_tag_name("testsuite"))
    {
        for dir in suite.descendants().filter(|n| n.has_tag_name("directory")) {
            if let Some(text) = dir.text() {
                test_suites.push(root.join(text.trim()));
            }
        }
    }

    let mut source_includes = Vec::new();
    let mut source_excludes = Vec::new();
    for source in phpunit_node
        .descendants()
        .filter(|n| n.has_tag_name("source"))
    {
        for include in source.descendants().filter(|n| n.has_tag_name("include")) {
            for dir in include
                .descendants()
                .filter(|n| n.has_tag_name("directory"))
            {
                if let Some(text) = dir.text() {
                    source_includes.push(root.join(text.trim()));
                }
            }
        }
        for exclude in source.descendants().filter(|n| n.has_tag_name("exclude")) {
            for dir in exclude
                .descendants()
                .filter(|n| n.has_tag_name("directory"))
            {
                if let Some(text) = dir.text() {
                    source_excludes.push(root.join(text.trim()));
                }
            }
        }
    }

    Ok(ProjectConfig {
        root,
        test_suites,
        source_includes,
        source_excludes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phpunit.xml");
        std::fs::write(
            &path,
            r#"<?xml version="1.0"?>
<phpunit>
    <testsuites>
        <testsuite name="default">
            <directory>tests</directory>
        </testsuite>
    </testsuites>
    <source>
        <include>
            <directory>src</directory>
        </include>
    </source>
</phpunit>"#,
        )
        .unwrap();

        let cfg = parse(&path).unwrap();
        assert_eq!(cfg.test_suites, vec![dir.path().join("tests")]);
        assert_eq!(cfg.source_includes, vec![dir.path().join("src")]);
        assert!(cfg.source_excludes.is_empty());
    }

    #[test]
    fn parses_with_excludes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phpunit.xml");
        std::fs::write(
            &path,
            r#"<?xml version="1.0"?>
<phpunit>
    <testsuites>
        <testsuite name="unit"><directory>tests/Unit</directory></testsuite>
        <testsuite name="integration"><directory>tests/Integration</directory></testsuite>
    </testsuites>
    <source>
        <include><directory>src</directory></include>
        <exclude><directory>src/Migrations</directory></exclude>
    </source>
</phpunit>"#,
        )
        .unwrap();

        let cfg = parse(&path).unwrap();
        assert_eq!(
            cfg.test_suites,
            vec![
                dir.path().join("tests/Unit"),
                dir.path().join("tests/Integration"),
            ]
        );
        assert_eq!(cfg.source_includes, vec![dir.path().join("src")]);
        assert_eq!(cfg.source_excludes, vec![dir.path().join("src/Migrations")]);
    }

    #[test]
    fn errors_on_missing_file() {
        let result = parse(Path::new("/nonexistent/phpunit.xml"));
        assert!(matches!(result, Err(ConfigError::NotFound(_))));
    }

    #[test]
    fn errors_on_malformed_xml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phpunit.xml");
        std::fs::write(&path, "<phpunit><unclosed>").unwrap();
        let result = parse(&path);
        assert!(matches!(result, Err(ConfigError::Malformed { .. })));
    }
}
