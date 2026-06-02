use crate::config::ProjectConfig;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boundary {
    Project,
    Vendor,
    Builtin,
}

pub struct BoundaryResolver {
    includes: Vec<PathBuf>,
    excludes: Vec<PathBuf>,
    vendor_root: Option<PathBuf>,
}

impl BoundaryResolver {
    pub fn from_config(cfg: &ProjectConfig) -> Self {
        let vendor_root_raw = cfg.root.join("vendor");
        Self {
            includes: cfg
                .source_includes
                .iter()
                .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
                .collect(),
            excludes: cfg
                .source_excludes
                .iter()
                .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
                .collect(),
            vendor_root: vendor_root_raw.canonicalize().ok().filter(|p| p.exists()),
        }
    }

    pub fn classify(&self, path: &Path) -> Boundary {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        if let Some(vendor) = &self.vendor_root {
            if canonical.starts_with(vendor) {
                return Boundary::Vendor;
            }
        }

        for exclude in &self.excludes {
            if canonical.starts_with(exclude) {
                return Boundary::Vendor;
            }
        }

        for include in &self.includes {
            if canonical.starts_with(include) {
                return Boundary::Project;
            }
        }

        Boundary::Builtin
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cfg(root: &Path) -> ProjectConfig {
        ProjectConfig {
            root: root.to_path_buf(),
            test_suites: vec![root.join("tests")],
            source_includes: vec![root.join("src")],
            source_excludes: vec![root.join("src/Migrations")],
        }
    }

    #[test]
    fn classifies_project_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/Foo")).unwrap();
        let file = dir.path().join("src/Foo/Bar.php");
        std::fs::write(&file, "<?php").unwrap();

        let resolver = BoundaryResolver::from_config(&make_cfg(dir.path()));
        assert_eq!(resolver.classify(&file), Boundary::Project);
    }

    #[test]
    fn classifies_vendor_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vendor/symfony")).unwrap();
        let file = dir.path().join("vendor/symfony/Foo.php");
        std::fs::write(&file, "<?php").unwrap();

        let resolver = BoundaryResolver::from_config(&make_cfg(dir.path()));
        assert_eq!(resolver.classify(&file), Boundary::Vendor);
    }

    #[test]
    fn classifies_excluded_path_as_vendor() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/Migrations")).unwrap();
        let file = dir.path().join("src/Migrations/v1.php");
        std::fs::write(&file, "<?php").unwrap();

        let resolver = BoundaryResolver::from_config(&make_cfg(dir.path()));
        assert_eq!(resolver.classify(&file), Boundary::Vendor);
    }

    #[test]
    fn classifies_unknown_path_as_builtin() {
        let dir = tempfile::tempdir().unwrap();
        let resolver = BoundaryResolver::from_config(&make_cfg(dir.path()));
        assert_eq!(
            resolver.classify(Path::new("/usr/lib/php/builtin.so")),
            Boundary::Builtin
        );
    }
}
