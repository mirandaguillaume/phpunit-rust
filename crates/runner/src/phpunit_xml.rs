//! Minimal parser for the bits of `phpunit.xml` we honor in our runner.
//! We extract: (a) the `bootstrap` attribute on `<phpunit>`, (b) the
//! `<directory>` / `<exclude>` entries inside `<testsuite>` blocks, and
//! (c) the `<const name=... value=.../>` declarations inside `<php>`.
//! We deliberately ignore `<source>`, `<extensions>`, `<groups>`, etc.

use quick_xml::events::Event;
use quick_xml::reader::Reader;

/// One `<directory>` entry inside a `<testsuite>`. `suffix` is captured
/// verbatim from the XML attribute (PHPUnit defaults to `Test.php` when
/// absent). Non-`.php` suffixes (notably `.phpt` for end-to-end script
/// tests) signal a directory we should not walk for class-based discovery
/// — only PHPUnit's runtime knows how to invoke `.phpt` files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestSuiteDir {
    pub path:   String,
    pub suffix: Option<String>,
}

impl TestSuiteDir {
    /// True when this directory's suffix is compatible with PHP class
    /// discovery (i.e. the `suffix` attribute either is absent — PHPUnit's
    /// default `Test.php` — or ends with `.php` case-insensitively). Returns
    /// false for `.phpt` and other non-class suffixes so the caller can skip
    /// the directory entirely. PHPUnit-itself's end-to-end testsuite uses
    /// `suffix=".phpt"`; without this filter we descend into its fixture
    /// `_files/` subtree and try to run scripts that hang waiting for input
    /// they would normally receive from their parent test's `proc_open()`.
    pub fn is_class_discoverable(&self) -> bool {
        match &self.suffix {
            None => true,
            Some(s) => {
                let lower = s.to_ascii_lowercase();
                lower.ends_with(".php")
            }
        }
    }
}

/// A `<testsuite>` block: include directories + exclude directories,
/// all as relative path strings (caller resolves against phpunit.xml's dir).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TestSuite {
    /// The `name` attribute (used by `--testsuite NAME` to pick one).
    pub name: String,
    pub directories: Vec<TestSuiteDir>,
    pub excludes: Vec<String>,
}

/// A `<const>` (or `<env>`) declaration from the `<php>` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhpConstant {
    pub name: String,
    pub value: String,
}

/// Everything we extract from the `<php>` block in one pass. PHPUnit
/// supports `<const>`, `<env>`, `<server>`, `<ini>`, `<var>`, `<get>`,
/// `<post>`, `<cookie>`, `<files>`, `<request>` — we currently handle
/// the five most-used (const/env/server/ini/var); the others can be added
/// the same way when a real project needs them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PhpBlock {
    pub constants: Vec<PhpConstant>,
    pub env:       Vec<PhpEnv>,
    pub server:    Vec<PhpConstant>,
    pub ini:       Vec<PhpConstant>,
    /// `<var name="..." value="..."/>` — PHPUnit's PhpHandler assigns these to
    /// `$GLOBALS[$name]` (NOT the environment). Projects like doctrine-orm read
    /// `$GLOBALS['db_driver']` to find their test database connection params.
    pub vars:      Vec<PhpConstant>,
}

/// `<env name="..." value="..." force="true"/>`. `force` controls whether
/// to overwrite an existing environment variable; PHPUnit defaults to
/// false (do NOT clobber a value present in the shell).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhpEnv {
    pub name:  String,
    pub value: String,
    pub force: bool,
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
    // `suffix` attribute on the currently-open `<directory>` (None when
    // the attribute is absent or we're not inside a directory tag).
    let mut current_suffix: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.local_name().as_ref().to_vec();
                match name.as_slice() {
                    b"testsuite" => {
                        let mut s = TestSuite::default();
                        for attr in e.attributes().flatten() {
                            if attr.key.local_name().as_ref() == b"name" {
                                if let Ok(v) = std::str::from_utf8(&attr.value) {
                                    s.name = v.to_string();
                                }
                            }
                        }
                        current = Some(s);
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
                        // Capture `suffix` so the caller can skip directories
                        // whose files we can't run (e.g. `.phpt`).
                        current_suffix = None;
                        for attr in e.attributes().flatten() {
                            if attr.key.local_name().as_ref() == b"suffix" {
                                if let Ok(v) = std::str::from_utf8(&attr.value) {
                                    current_suffix = Some(v.to_string());
                                }
                            }
                        }
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
                    b"directory" | b"file" => {
                        active_tag = None;
                        current_suffix = None;
                    }
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
                                suite.directories.push(TestSuiteDir {
                                    path:   trimmed.to_string(),
                                    suffix: current_suffix.clone(),
                                });
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

/// Parse the full `<php>` block: `<const>`, `<env>`, `<server>`, `<ini>`.
/// One walk over the XML. Symfony/Laravel tests rely on env/server vars
/// being set before any code runs; ini lets projects bump memory_limit,
/// error_reporting, etc. that PHPUnit normally sets at boot.
pub fn parse_php_block(xml: &str) -> PhpBlock {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_php = false;
    let mut out = PhpBlock::default();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"php" => in_php = true,
            Ok(Event::End(e))   if e.local_name().as_ref() == b"php" => in_php = false,
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) if in_php => {
                let tag = e.local_name().as_ref().to_vec();
                let mut name = None;
                let mut value = None;
                let mut force = false;
                for attr in e.attributes().flatten() {
                    let k = attr.key.local_name();
                    match k.as_ref() {
                        b"name"  => name  = std::str::from_utf8(&attr.value).ok().map(String::from),
                        b"value" => value = std::str::from_utf8(&attr.value).ok().map(String::from),
                        b"force" => force = matches!(attr.value.as_ref(), b"true" | b"1"),
                        _ => {}
                    }
                }
                let (Some(n), Some(v)) = (name, value) else { continue };
                match tag.as_slice() {
                    b"const"  => out.constants.push(PhpConstant { name: n, value: v }),
                    b"env"    => out.env.push(PhpEnv { name: n, value: v, force }),
                    b"server" => out.server.push(PhpConstant { name: n, value: v }),
                    b"ini"    => out.ini.push(PhpConstant { name: n, value: v }),
                    b"var"    => out.vars.push(PhpConstant { name: n, value: v }),
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
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

/// Parse `<listeners><listener class="..."/>` entries. Returns the list of
/// listener class FQCNs. We don't dispatch into them generically (that
/// would require running arbitrary PHP), but we *detect* well-known ones
/// (currently Symfony\Bridge\PhpUnit\SymfonyTestsListener) to replicate
/// their visible side-effect: emitting one SkippedTestCase outcome per
/// `@group legacy` test method.
pub fn parse_listeners(xml: &str) -> Vec<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) if e.local_name().as_ref() == b"listener" => {
                for attr in e.attributes().flatten() {
                    if attr.key.local_name().as_ref() == b"class" {
                        if let Ok(v) = std::str::from_utf8(&attr.value) {
                            out.push(v.to_string());
                        }
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
        let paths: Vec<&str> = suites[0].directories.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, vec!["tests"]);
        assert_eq!(suites[0].excludes, vec!["tests/Integration"]);
        let paths: Vec<&str> = suites[1].directories.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, vec!["tests/Integration"]);
        let paths: Vec<&str> = suites[2].directories.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, vec!["./vendor/somepkg/tests"]);
        // Default (no suffix attribute) → directory is class-discoverable.
        assert!(suites[0].directories[0].is_class_discoverable());
        assert_eq!(suites[0].directories[0].suffix, None);
    }

    #[test]
    fn parses_directory_suffix_and_filters_non_php() {
        let xml = r#"<?xml version="1.0"?>
<phpunit>
    <testsuites>
        <testsuite name="end-to-end">
            <directory suffix=".phpt">tests/end-to-end/baseline</directory>
            <directory suffix="Test.php">tests/unit</directory>
            <directory>tests/integration</directory>
        </testsuite>
    </testsuites>
</phpunit>"#;
        let suites = parse_testsuites(xml);
        assert_eq!(suites.len(), 1);
        let dirs = &suites[0].directories;
        assert_eq!(dirs.len(), 3);
        assert_eq!(dirs[0].suffix.as_deref(), Some(".phpt"));
        assert!(!dirs[0].is_class_discoverable(),
            ".phpt directory must be skipped for class discovery");
        assert_eq!(dirs[1].suffix.as_deref(), Some("Test.php"));
        assert!(dirs[1].is_class_discoverable(),
            "Test.php suffix walks as normal");
        assert_eq!(dirs[2].suffix, None);
        assert!(dirs[2].is_class_discoverable(),
            "absent suffix defaults to PHPUnit's Test.php — walk normally");
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
    fn parses_full_php_block() {
        // Real Symfony/Laravel pattern.
        let xml = r#"<?xml version="1.0"?>
<phpunit>
    <php>
        <const name="API_KEY" value="abc123"/>
        <env name="APP_ENV" value="testing"/>
        <env name="DB_DSN" value="sqlite::memory:" force="true"/>
        <server name="HTTPS" value="on"/>
        <ini name="memory_limit" value="-1"/>
        <ini name="error_reporting" value="-1"/>
    </php>
</phpunit>"#;
        let b = parse_php_block(xml);
        assert_eq!(b.constants.len(), 1);
        assert_eq!(b.constants[0].name, "API_KEY");
        assert_eq!(b.env.len(), 2);
        assert_eq!(b.env[0].name, "APP_ENV");
        assert!(!b.env[0].force, "default force=false");
        assert!(b.env[1].force, "force=\"true\" recognised");
        assert_eq!(b.server.len(), 1);
        assert_eq!(b.server[0].name, "HTTPS");
        assert_eq!(b.ini.len(), 2);
        assert_eq!(b.ini[1].value, "-1");
    }

    #[test]
    fn parses_php_var_into_dedicated_bucket_not_env() {
        // doctrine-orm pattern: PHPUnit's `<var>` populates $GLOBALS (NOT the
        // environment). They must be captured in their own bucket, never dropped
        // nor lumped into `<env>`. Regression for the 1558 "You must provide ...
        // db_driver" errors caused by `<var>` being silently ignored.
        let xml = r#"<?xml version="1.0"?>
<phpunit>
    <php>
        <ini name="error_reporting" value="-1"/>
        <var name="db_driver" value="pdo_sqlite"/>
        <var name="db_memory" value="true"/>
        <env name="COLUMNS" value="120"/>
    </php>
</phpunit>"#;
        let b = parse_php_block(xml);
        assert_eq!(b.vars.len(), 2, "both <var> entries captured");
        assert_eq!(b.vars[0].name, "db_driver");
        assert_eq!(b.vars[0].value, "pdo_sqlite");
        assert_eq!(b.vars[1].name, "db_memory");
        assert_eq!(b.env.len(), 1, "<var> must NOT be counted as <env>");
        assert_eq!(b.env[0].name, "COLUMNS");
    }

    #[test]
    fn returns_empty_php_block_when_no_block() {
        let xml = r#"<?xml version="1.0"?><phpunit bootstrap="boot.php"></phpunit>"#;
        let b = parse_php_block(xml);
        assert!(b.constants.is_empty() && b.env.is_empty()
             && b.server.is_empty() && b.ini.is_empty());
    }

    #[test]
    fn parses_listeners() {
        // Faker's actual phpunit.xml.dist pattern.
        let xml = r#"<?xml version="1.0"?>
<phpunit>
    <listeners>
        <listener class="Symfony\Bridge\PhpUnit\SymfonyTestsListener"/>
        <listener class="My\Other\Listener"/>
    </listeners>
</phpunit>"#;
        let listeners = parse_listeners(xml);
        assert_eq!(listeners, vec![
            "Symfony\\Bridge\\PhpUnit\\SymfonyTestsListener".to_string(),
            "My\\Other\\Listener".to_string(),
        ]);
    }

    #[test]
    fn returns_empty_listeners_when_no_block() {
        let xml = r#"<?xml version="1.0"?>
<phpunit bootstrap="boot.php"></phpunit>"#;
        assert!(parse_listeners(xml).is_empty());
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
