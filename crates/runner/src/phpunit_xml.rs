//! Minimal parser for the bits of `phpunit.xml` we honor in our runner.
//! We extract: (a) the `bootstrap` attribute on `<phpunit>`, (b) the
//! `<directory>` / `<exclude>` entries inside `<testsuite>` blocks, and
//! (c) the `<const name=... value=.../>` declarations inside `<php>`.
//! We deliberately ignore `<source>`, `<extensions>`, `<groups>`, etc.

use quick_xml::events::Event;
use quick_xml::reader::Reader;

/// A `<testsuite>` block: include directories + exclude directories,
/// all as relative path strings (caller resolves against phpunit.xml's dir).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TestSuite {
    pub directories: Vec<String>,
    pub excludes: Vec<String>,
}

/// A `<const>` (or `<env>`) declaration from the `<php>` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhpConstant {
    pub name: String,
    pub value: String,
}

/// Returns the value of the `bootstrap` attribute on the root `<phpunit>`
/// element, or None if absent / file is malformed.
pub fn parse_bootstrap(xml: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if e.local_name().as_ref() == b"phpunit" {
                    for attr in e.attributes().flatten() {
                        if attr.key.local_name().as_ref() == b"bootstrap" {
                            return std::str::from_utf8(&attr.value).ok().map(String::from);
                        }
                    }
                    return None;
                }
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
}

/// Parse every `<testsuite>` block under `<testsuites>` (or direct children
/// of `<phpunit>`, for older configs). Each suite returns its `<directory>`
/// includes and `<exclude>` paths (both as raw strings, relative to the
/// phpunit.xml directory).
///
/// Returns empty if no testsuites are declared — caller falls back to its
/// own default (typically `tests/`).
pub fn parse_testsuites(xml: &str) -> Vec<TestSuite> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut suites: Vec<TestSuite> = Vec::new();
    let mut current: Option<TestSuite> = None;
    // Track the active leaf element so we know whether <directory>'s text
    // belongs to an include or an exclude.
    let mut in_exclude = false;
    let mut active_tag: Option<Vec<u8>> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.local_name().as_ref().to_vec();
                match name.as_slice() {
                    b"testsuite" => {
                        current = Some(TestSuite::default());
                        in_exclude = false;
                    }
                    b"exclude" => {
                        // `<exclude>path</exclude>` (text directly inside) OR
                        // `<exclude><directory>path</directory></exclude>` —
                        // we treat the exclude itself as an active tag so the
                        // text-only form is captured.
                        in_exclude = true;
                        active_tag = Some(name);
                    }
                    b"directory" | b"file" => {
                        active_tag = Some(name);
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let name = e.local_name().as_ref().to_vec();
                match name.as_slice() {
                    b"testsuite" => {
                        if let Some(s) = current.take() {
                            if !s.directories.is_empty() || !s.excludes.is_empty() {
                                suites.push(s);
                            }
                        }
                        in_exclude = false;
                        active_tag = None;
                    }
                    b"exclude" => {
                        in_exclude = false;
                        active_tag = None;
                    }
                    b"directory" | b"file" => active_tag = None,
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if let (Some(suite), Some(_)) = (current.as_mut(), active_tag.as_ref()) {
                    if let Ok(text) = std::str::from_utf8(t.as_ref()) {
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            if in_exclude {
                                suite.excludes.push(trimmed.to_string());
                            } else {
                                suite.directories.push(trimmed.to_string());
                            }
                        }
                    }
                }
            }
            // Self-closing `<exclude>path</exclude>` could also be `<exclude path="..."/>`
            // in some configs — we don't see those forms in the wild often. Ignored.
            Ok(Event::Empty(_)) => {}
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    suites
}

/// Parse `<const name="..." value="..."/>` declarations inside the `<php>`
/// block. Returns empty if no `<php>` block or no constants.
pub fn parse_php_constants(xml: &str) -> Vec<PhpConstant> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_php = false;
    let mut out = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"php" => in_php = true,
            Ok(Event::End(e)) if e.local_name().as_ref() == b"php" => in_php = false,
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) if in_php => {
                if e.local_name().as_ref() == b"const" {
                    let mut name = None;
                    let mut value = None;
                    for attr in e.attributes().flatten() {
                        let key = attr.key.local_name();
                        if key.as_ref() == b"name" {
                            name = std::str::from_utf8(&attr.value).ok().map(String::from);
                        } else if key.as_ref() == b"value" {
                            value = std::str::from_utf8(&attr.value).ok().map(String::from);
                        }
                    }
                    if let (Some(n), Some(v)) = (name, value) {
                        out.push(PhpConstant { name: n, value: v });
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// Parse `<groups><exclude><group>name</group>...` declarations. Returns
/// the list of excluded group names. Tests annotated with one of these
/// groups via `#[Group('name')]` or `@group name` must be skipped (vanilla
/// PHPUnit behaviour). doctrine-orm uses this to exclude their
/// `performance` and `locking_functional` groups by default.
pub fn parse_excluded_groups(xml: &str) -> Vec<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut depth_groups = 0;
    let mut depth_exclude = 0;
    let mut in_group_elem = false;
    let mut current = String::new();
    let mut out = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                match e.local_name().as_ref() {
                    b"groups"  => depth_groups += 1,
                    b"exclude" if depth_groups > 0 => depth_exclude += 1,
                    b"group"   if depth_exclude > 0 => {
                        in_group_elem = true;
                        current.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                match e.local_name().as_ref() {
                    b"groups"  => depth_groups -= 1,
                    b"exclude" if depth_groups > 0 => depth_exclude -= 1,
                    b"group"   if in_group_elem => {
                        in_group_elem = false;
                        let name = current.trim().to_string();
                        if !name.is_empty() { out.push(name); }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) if in_group_elem => {
                if let Ok(s) = std::str::from_utf8(&t) { current.push_str(s); }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bootstrap_attribute_from_phpunit_xml() {
        let xml = r#"<?xml version="1.0"?>
<phpunit bootstrap="phpunit.php" colors="true">
    <testsuites>
        <testsuite name="default">
            <directory>tests</directory>
        </testsuite>
    </testsuites>
</phpunit>"#;
        let bootstrap = parse_bootstrap(xml);
        assert_eq!(bootstrap.as_deref(), Some("phpunit.php"));
    }

    #[test]
    fn returns_none_when_no_bootstrap_attribute() {
        let xml = r#"<?xml version="1.0"?>
<phpunit colors="true"></phpunit>"#;
        assert!(parse_bootstrap(xml).is_none());
    }

    #[test]
    fn returns_none_on_malformed_xml() {
        assert!(parse_bootstrap("this is not xml").is_none());
    }

    #[test]
    fn parses_multiple_testsuites_with_directories_and_excludes() {
        let xml = r#"<?xml version="1.0"?>
<phpunit>
    <testsuites>
        <testsuite name="Unit">
            <directory>tests</directory>
            <exclude>tests/Integration</exclude>
        </testsuite>
        <testsuite name="Integration">
            <directory>tests/Integration</directory>
        </testsuite>
        <testsuite name="External">
            <directory>./vendor/somepkg/tests</directory>
        </testsuite>
    </testsuites>
</phpunit>"#;
        let suites = parse_testsuites(xml);
        assert_eq!(suites.len(), 3);
        assert_eq!(suites[0].directories, vec!["tests"]);
        assert_eq!(suites[0].excludes, vec!["tests/Integration"]);
        assert_eq!(suites[1].directories, vec!["tests/Integration"]);
        assert_eq!(suites[2].directories, vec!["./vendor/somepkg/tests"]);
    }

    #[test]
    fn returns_empty_testsuites_when_no_suites_declared() {
        let xml = r#"<?xml version="1.0"?>
<phpunit bootstrap="boot.php"></phpunit>"#;
        assert!(parse_testsuites(xml).is_empty());
    }

    #[test]
    fn parses_php_const_declarations() {
        let xml = r#"<?xml version="1.0"?>
<phpunit>
    <php>
        <const name="API_KEY" value="abc123"/>
        <const name="DEBUG_MODE" value="1"/>
    </php>
</phpunit>"#;
        let consts = parse_php_constants(xml);
        assert_eq!(consts.len(), 2);
        assert_eq!(consts[0].name, "API_KEY");
        assert_eq!(consts[0].value, "abc123");
        assert_eq!(consts[1].name, "DEBUG_MODE");
        assert_eq!(consts[1].value, "1");
    }

    #[test]
    fn returns_empty_consts_when_no_php_block() {
        let xml = r#"<?xml version="1.0"?>
<phpunit bootstrap="boot.php"></phpunit>"#;
        assert!(parse_php_constants(xml).is_empty());
    }

    #[test]
    fn parses_excluded_groups() {
        // Real doctrine-orm pattern.
        let xml = r#"<?xml version="1.0"?>
<phpunit>
    <testsuites><testsuite name="x"><directory>tests</directory></testsuite></testsuites>
    <groups>
        <exclude>
            <group>performance</group>
            <group>locking_functional</group>
        </exclude>
    </groups>
</phpunit>"#;
        let excl = parse_excluded_groups(xml);
        assert_eq!(excl, vec!["performance".to_string(), "locking_functional".to_string()]);
    }

    #[test]
    fn returns_empty_excluded_groups_when_no_groups_block() {
        let xml = r#"<?xml version="1.0"?>
<phpunit><testsuites><testsuite name="x"><directory>tests</directory></testsuite></testsuites></phpunit>"#;
        assert!(parse_excluded_groups(xml).is_empty());
    }

    #[test]
    fn ignores_include_groups() {
        // `<include>` is the dual of `<exclude>` — we only return excludes.
        let xml = r#"<?xml version="1.0"?>
<phpunit>
    <groups>
        <include><group>fast</group></include>
        <exclude><group>slow</group></exclude>
    </groups>
</phpunit>"#;
        let excl = parse_excluded_groups(xml);
        assert_eq!(excl, vec!["slow".to_string()]);
    }
}
