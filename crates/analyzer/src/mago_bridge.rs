//! Bridge to the `mago-project` crate for PHP static analysis.
//!
//! # API investigation findings (Task 7)
//!
//! ## mago-project 0.26.1
//! - `ProjectBuilder::new(interner)` + `ProjectBuilder::add_module(module)` + `ProjectBuilder::build(populate_non_user_defined)` → `Project`
//! - `Module::build(&interner, version, source, options)` → `Module` (parses PHP, resolves names, reflects)
//! - `Source::standalone(&interner, name, content)` → `Source` (quick one-off source without SourceManager)
//! - `Project { modules: Vec<Module>, reflection: CodebaseReflection }`
//! - `CodebaseReflection { class_like_reflections: HashMap<ClassLikeName, ClassLikeReflection>, … }`
//! - `ClassLikeReflection { name: ClassLikeName, inheritance: InheritanceReflection, methods: MemeberCollection<FunctionLikeReflection>, span: Span, … }`
//! - `InheritanceReflection { direct_extended_class: Option<Name>, direct_implemented_interfaces: HashSet<Name>, … }`
//! - `ClassLikeName::get_key(&interner)` → `String` (FQCN or anonymous-class@…)
//! - `FunctionLikeName::get_key(&interner)` → `String` (method: "ClassName::methodName")
//! - `Span { start: Position { source: SourceIdentifier, offset: usize }, … }` (mago-span 0.26)
//! - `Source::line_number(offset)` → 0-based line index
//!
//! ## mago-analyzer 1.27.1 — NOT USED (incompatible type system)
//! mago-analyzer 1.27 is on a completely different major version series. It uses:
//!   - mago-codex::metadata::CodebaseMetadata (not mago-reflection::CodebaseReflection)
//!   - mago-database::file::File (not mago-source::Source)
//!   - mago-span 1.27 where Span has a FileId field (from mago-database) — incompatible with mago-span 0.26
//!   - mago-syntax 1.27 AST (not mago-syntax 0.26 AST)
//! These crates cannot interoperate with mago-project 0.26 — different AST, source, and span types.
//! Resolution: use only mago-project 0.26 for parsing and reflection.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use mago_interner::ThreadedInterner;
use mago_php_version::PHPVersion;
use mago_project::module::{Module, ModuleBuildOptions};
use mago_project::Project;
use mago_reflection::class_like::ClassLikeReflection;
use mago_reflection::identifier::ClassLikeName;
use mago_source::Source;
use mago_syntax::ast::Program;

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("IO error reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("PHP parse error in {path}: {message}")]
    Parse { path: String, message: String },
    #[error("mago error: {0}")]
    Mago(String),
}

/// A loaded PHP project, backed by mago-project's `Project`.
///
/// Exposes the parsed classes and their metadata for downstream analysis.
pub struct MagoProject {
    inner: Project,
    interner: ThreadedInterner,
    /// Lowercased FQCN → ClassLikeName, built once at load time for O(1) class lookups.
    class_index: HashMap<String, ClassLikeName>,
    /// Parse cache: source → parsed AST, shared via Arc. Parsed once per unique source file;
    /// RwLock allows concurrent reads during parallel per-test tracing.
    parse_cache: RwLock<HashMap<mago_source::SourceIdentifier, Arc<Program>>>,
}

impl MagoProject {
    /// Load all `.php` files under `root` and build the project reflection.
    pub fn load(root: &Path) -> Result<Self, BridgeError> {
        Self::load_filtered(root, |_| true)
    }

    /// Like `load` but skips files under `root/vendor/`. Used when all analysis
    /// results are already in cache and vendor reflection is not needed.
    pub fn load_excluding_vendor(root: &Path) -> Result<Self, BridgeError> {
        let vendor = root.join("vendor");
        Self::load_filtered(root, move |path| !path.starts_with(&vendor))
    }

    fn load_filtered(
        root: &Path,
        keep: impl Fn(&std::path::Path) -> bool + Sync,
    ) -> Result<Self, BridgeError> {
        use rayon::prelude::*;

        let interner = ThreadedInterner::new();
        let version = PHPVersion::LATEST;
        let options = ModuleBuildOptions {
            reflect: true,
            validate: false,
        };

        // Collect all matching paths first (walkdir is sequential by design).
        let entries: Vec<_> = walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("php"))
            .filter(|e| keep(e.path()))
            .collect();

        // Parallel read + parse (CPU-bound; ThreadedInterner is Send+Sync).
        let modules: Vec<Module> = entries
            .par_iter()
            .map(|entry| {
                let path = entry.path();
                let name = path.display().to_string();
                let bytes = std::fs::read(path).map_err(|e| BridgeError::Io {
                    path: name.clone(),
                    source: e,
                })?;
                let content = String::from_utf8_lossy(&bytes).into_owned();
                let source = Source::standalone(&interner, &name, &content);
                Ok(Module::build(&interner, version, source, options))
            })
            .collect::<Result<Vec<_>, BridgeError>>()?;

        // Sequential add (ProjectBuilder is not Sync).
        let mut builder = Project::builder(interner.clone());
        for module in modules {
            builder.add_module(module);
        }

        let project = builder.build(false);
        let class_index: HashMap<String, ClassLikeName> = project
            .reflection
            .class_like_reflections
            .keys()
            .map(|name| (name.get_key(&interner).to_lowercase(), name.clone()))
            .collect();
        Ok(Self {
            inner: project,
            interner,
            class_index,
            parse_cache: RwLock::new(HashMap::new()),
        })
    }

    /// Return the version string of the mago-project series we are bridging.
    pub fn version() -> &'static str {
        "mago-project 0.26"
    }

    /// Iterate over all class-like reflections in the project (classes, interfaces, enums, traits).
    ///
    /// The iterator yields `(&ClassLikeName, &ClassLikeReflection)` pairs.
    pub fn class_likes(&self) -> impl Iterator<Item = (&ClassLikeName, &ClassLikeReflection)> {
        self.inner.reflection.class_like_reflections.iter()
    }

    /// Look up the FQCN string for any `ClassLikeName`.
    pub fn class_name_str(&self, name: &ClassLikeName) -> String {
        name.get_key(&self.interner)
    }

    /// O(1) class lookup by (lowercased) FQCN — replaces O(n) `class_likes().find(…)` scans.
    pub(crate) fn find_class_reflection(&self, name: &str) -> Option<&ClassLikeReflection> {
        let cn = self.class_index.get(&name.to_lowercase())?;
        self.inner.reflection.class_like_reflections.get(cn)
    }

    /// Return the `Project` directly for callers that need lower-level access.
    pub(crate) fn inner(&self) -> &Project {
        &self.inner
    }

    /// Return the interner for name resolution.
    pub(crate) fn interner(&self) -> &ThreadedInterner {
        &self.interner
    }

    /// Count of class-like entities (classes, interfaces, enums, traits) found in the project.
    pub fn class_like_count(&self) -> usize {
        self.inner.reflection.class_like_reflections.len()
    }

    /// Count of modules (source files) that were loaded.
    pub fn module_count(&self) -> usize {
        self.inner.modules.len()
    }

    /// Find a `Source` by its `SourceIdentifier`.
    ///
    /// Iterates `inner.modules` and returns the first module whose `source.identifier` matches.
    /// Used by downstream analysis (e.g., line-number resolution from a `Span`).
    pub(crate) fn source_by_id(&self, id: mago_source::SourceIdentifier) -> Option<&Source> {
        self.inner
            .modules
            .iter()
            .find(|m| m.source.identifier == id)
            .map(|m| &m.source)
    }

    /// Return the parsed AST for `src`, re-using a cached `Program` when available.
    ///
    /// Eliminates redundant `parse_source` calls when the same source file is visited
    /// many times during tracing (e.g. one call per test × call-graph depth).
    pub(crate) fn get_or_parse(&self, src: &Source) -> Arc<Program> {
        let id = src.identifier;
        {
            let cache = self.parse_cache.read().unwrap();
            if let Some(prog) = cache.get(&id) {
                return Arc::clone(prog);
            }
        }
        let (program, _) = mago_syntax::parser::parse_source(&self.interner, src);
        let arc = Arc::new(program);
        self.parse_cache
            .write()
            .unwrap()
            .insert(id, Arc::clone(&arc));
        arc
    }

    /// O(1) lookup returning both the canonical FQCN and its reflection.
    /// Used by callers that need the FQCN (e.g. inheritance-chain traversal).
    pub(crate) fn find_class(&self, name: &str) -> Option<(String, &ClassLikeReflection)> {
        let cn = self.class_index.get(&name.to_lowercase())?;
        let refl = self.inner.reflection.class_like_reflections.get(cn)?;
        Some((cn.get_key(&self.interner), refl))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_a_tiny_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Hello.php"),
            r#"<?php
class Hello {
    public function greet(): string { return 'hi'; }
}
"#,
        )
        .unwrap();

        let project = MagoProject::load(dir.path()).expect("load should succeed");

        // We loaded exactly one module.
        assert_eq!(project.module_count(), 1, "expected 1 module");

        // The reflection should contain at least one class-like (Hello).
        assert!(
            project.class_like_count() >= 1,
            "expected at least 1 class-like, got {}",
            project.class_like_count()
        );

        // Find the Hello class by name.
        let hello = project.class_likes().find(|(name, _)| {
            let key = project.class_name_str(name);
            key == "Hello" || key.to_lowercase() == "hello"
        });

        assert!(
            hello.is_some(),
            "Hello class was not found in reflection; classes found: {:?}",
            project
                .class_likes()
                .map(|(n, _)| project.class_name_str(n))
                .collect::<Vec<_>>()
        );

        let (_, hello_reflection) = hello.unwrap();

        // Verify the method `greet` is present.
        assert!(
            !hello_reflection.methods.members.is_empty(),
            "Hello class should have at least one method"
        );
    }

    #[test]
    fn loads_inheritance() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Animal.php"),
            r#"<?php
class Animal {}
class Dog extends Animal {}
"#,
        )
        .unwrap();

        let project = MagoProject::load(dir.path()).expect("load should succeed");

        let dog = project
            .class_likes()
            .find(|(name, _)| project.class_name_str(name).to_lowercase() == "dog");
        assert!(dog.is_some(), "Dog class not found");

        let (_, dog_reflection) = dog.unwrap();
        let parent = &dog_reflection.inheritance.direct_extended_class;
        assert!(parent.is_some(), "Dog should have a parent class (Animal)");
        let parent_name = project
            .interner()
            .lookup(&parent.unwrap().value)
            .to_string();
        assert!(
            parent_name.to_lowercase() == "animal",
            "Dog's parent should be Animal, got: {parent_name}"
        );
    }

    #[test]
    fn loads_multi_file_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("A.php"), "<?php\nclass A {}\n").unwrap();
        std::fs::write(dir.path().join("B.php"), "<?php\nclass B {}\n").unwrap();
        std::fs::write(dir.path().join("C.php"), "<?php\nclass C {}\n").unwrap();

        let project = MagoProject::load(dir.path()).expect("load should succeed");
        assert_eq!(project.module_count(), 3, "expected 3 modules");
        assert!(
            project.class_like_count() >= 3,
            "expected at least 3 class-likes"
        );
    }
}
