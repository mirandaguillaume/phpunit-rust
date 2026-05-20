//! Opacity decision rules for call sites.
//!
//! For each call observed by the analyzer, we decide:
//!   - Trace: follow the callee into its body, mark its lines covered.
//!   - Opaque: mark only the call site line, do not recurse.
//!
//! Rules consume a `ReceiverType` populated elsewhere (Tasks 16-17 will use
//! a lexical type tracker for this). The opacity layer stays independent
//! of how receiver types are inferred.

use crate::boundary::{Boundary, BoundaryResolver};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opacity {
    /// Follow the callee into its body; mark its lines covered.
    Trace,
    /// Mark only the call site; do not recurse into the callee.
    Opaque,
}

#[derive(Debug, Clone)]
pub struct CallSite {
    pub callee_class: Option<String>,
    pub callee_method: String,
    pub callee_file: Option<std::path::PathBuf>,
    pub receiver_type: ReceiverType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiverType {
    /// `Foo::bar()` — static dispatch with known class.
    Static,
    /// `$x->method()` where `$x` resolves to a concrete class.
    Concrete(String),
    /// `$x->method()` where `$x` resolves to an interface type only.
    Interface(String),
    /// Receiver created via `$this->createMock(...)` or similar.
    Mock,
    /// Receiver type couldn't be inferred (mixed, unknown, untyped).
    Mixed,
    /// Closure with a known local body.
    LocalClosure,
}

/// Decide whether the analyzer should trace into a call or treat it opaquely.
pub fn decide(call: &CallSite, boundary: &BoundaryResolver) -> Opacity {
    // 1. Callee outside the project (vendor or builtin) → opaque.
    if let Some(file) = &call.callee_file {
        match boundary.classify(file) {
            Boundary::Vendor | Boundary::Builtin => return Opacity::Opaque,
            Boundary::Project => {}
        }
    } else {
        // No file → builtin or generated code → opaque.
        return Opacity::Opaque;
    }

    // 2. Receiver-type rules.
    match &call.receiver_type {
        ReceiverType::Mock | ReceiverType::Interface(_) | ReceiverType::Mixed => Opacity::Opaque,
        ReceiverType::Concrete(_) | ReceiverType::Static | ReceiverType::LocalClosure => Opacity::Trace,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProjectConfig;
    use std::path::Path;

    fn make_resolver(root: &Path) -> BoundaryResolver {
        BoundaryResolver::from_config(&ProjectConfig {
            root: root.to_path_buf(),
            test_suites: vec![root.join("tests")],
            source_includes: vec![root.join("src")],
            source_excludes: vec![],
        })
    }

    #[test]
    fn vendor_call_is_opaque() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vendor")).unwrap();
        let vendor_file = dir.path().join("vendor/x.php");
        std::fs::write(&vendor_file, "<?php").unwrap();
        let call = CallSite {
            callee_class: Some("X".into()),
            callee_method: "y".into(),
            callee_file: Some(vendor_file),
            receiver_type: ReceiverType::Concrete("X".into()),
        };
        assert_eq!(decide(&call, &make_resolver(dir.path())), Opacity::Opaque);
    }

    #[test]
    fn project_concrete_call_is_traced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let f = dir.path().join("src/U.php");
        std::fs::write(&f, "<?php").unwrap();
        let call = CallSite {
            callee_class: Some("U".into()),
            callee_method: "save".into(),
            callee_file: Some(f),
            receiver_type: ReceiverType::Concrete("U".into()),
        };
        assert_eq!(decide(&call, &make_resolver(dir.path())), Opacity::Trace);
    }

    #[test]
    fn interface_dispatch_is_opaque() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let f = dir.path().join("src/R.php");
        std::fs::write(&f, "<?php").unwrap();
        let call = CallSite {
            callee_class: Some("R".into()),
            callee_method: "find".into(),
            callee_file: Some(f),
            receiver_type: ReceiverType::Interface("R".into()),
        };
        assert_eq!(decide(&call, &make_resolver(dir.path())), Opacity::Opaque);
    }

    #[test]
    fn mock_dispatch_is_opaque() {
        let dir = tempfile::tempdir().unwrap();
        let call = CallSite {
            callee_class: Some("R".into()),
            callee_method: "find".into(),
            callee_file: None,
            receiver_type: ReceiverType::Mock,
        };
        assert_eq!(decide(&call, &make_resolver(dir.path())), Opacity::Opaque);
    }

    #[test]
    fn no_file_is_opaque() {
        let dir = tempfile::tempdir().unwrap();
        let call = CallSite {
            callee_class: None,
            callee_method: "strlen".into(),
            callee_file: None,
            receiver_type: ReceiverType::Static,
        };
        assert_eq!(decide(&call, &make_resolver(dir.path())), Opacity::Opaque);
    }
}
