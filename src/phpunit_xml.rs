//! Minimal parser for the bits of `phpunit.xml` we honor in our runner.
//! We deliberately do NOT parse `<testsuites>`, `<source>`, `<extensions>`,
//! etc. — our own discovery handles test enumeration. The only attribute
//! we currently care about is `bootstrap` on the root `<phpunit>` element.

use quick_xml::events::Event;
use quick_xml::reader::Reader;

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
}
