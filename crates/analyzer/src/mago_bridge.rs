//! Bridge to mago 1.30 (mago-database + mago-syntax + mago-codex).
//!
//! # Architecture (1.30, arena-based)
//!
//! mago 1.x removed `mago-project` and `mago-reflection`. Their roles are now:
//!   - `mago-database` — the file/source model (`File`, `FileId`).
//!   - `mago-syntax::parser::parse_file(arena, file)` — parses into a bump arena,
//!     returning `&'arena Program<'arena>`.
//!   - `mago-names::resolver::NameResolver::new(arena).resolve(program)` — name resolution.
//!   - `mago-codex::scanner::scan_program(...)` → an OWNED `CodebaseMetadata`
//!     (byte-string keys; does NOT borrow the arena), merged across files with
//!     `CodebaseMetadata::extend`, finalized once with `populator::populate_codebase`.
//!
//! Because `CodebaseMetadata` is owned, the self-referential "store arena + AST"
//! problem disappears: the bridge owns only the `CodebaseMetadata` (reflection)
//! plus the loaded `File`s. AST-walking consumers re-parse on demand into a
//! scoped scratch arena via [`MagoProject::with_program`], which is dropped when
//! the closure returns — no caching unsafety, no `'arena` threaded into the bridge.

use std::collections::HashMap;
use std::path::Path;

use bumpalo::Bump;
use mago_codex::metadata::CodebaseMetadata;
use mago_codex::metadata::class_like::ClassLikeMetadata;
use mago_codex::populator::populate_codebase;
use mago_codex::reference::SymbolReferences;
use mago_codex::scanner::scan_program;
use mago_database::file::{File, FileType};
use mago_database::file::FileId;
use mago_names::ResolvedNames;
use mago_names::resolver::NameResolver;
use mago_php_version::PHPVersion;
use mago_span::Span;
use mago_syntax::ast::Program;
use mago_syntax::parser::parse_file;
use mago_word::Word;

/// Decode an interned `Word` (byte-string) into a Rust `String` (lossy).
pub(crate) fn word_to_string(w: &Word) -> String {
    String::from_utf8_lossy(w.as_bytes()).into_owned()
}

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

/// A loaded PHP project, backed by mago 1.30's codebase metadata.
pub struct MagoProject {
    /// Owned reflection: classes, interfaces, enums, traits, functions, and (via
    /// the analyzer) inferred types. Byte-string keyed; no lifetime.
    codebase: CodebaseMetadata,
    /// The loaded source files, kept so AST-walking consumers can re-parse on
    /// demand. `File` owns its contents (`Cow<'static, [u8]>`).
    files: Vec<File>,
    /// Lowercased logical-name → index into `files`, for `with_program` lookups.
    file_index: HashMap<String, usize>,
    /// `FileId` → index into `files`, for resolving a span's file (e.g. a class's
    /// declaring file from its `span.file_id`).
    by_file_id: HashMap<FileId, usize>,
}

impl MagoProject {
    /// Load all `.php` files under `root` and build the codebase metadata.
    pub fn load(root: &Path) -> Result<Self, BridgeError> {
        Self::load_filtered(root, |_| true)
    }

    /// Like `load` but skips files under `root/vendor/`.
    pub fn load_excluding_vendor(root: &Path) -> Result<Self, BridgeError> {
        let vendor = root.join("vendor");
        Self::load_filtered(root, move |path| !path.starts_with(&vendor))
    }

    fn load_filtered(
        root: &Path,
        keep: impl Fn(&Path) -> bool + Sync,
    ) -> Result<Self, BridgeError> {
        let version = PHPVersion::LATEST;

        // Collect matching .php paths (walkdir is sequential).
        let paths: Vec<_> = walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("php"))
            .filter(|e| keep(e.path()))
            .map(|e| e.path().to_path_buf())
            .collect();

        // Read each file into a mago `File`, then scan into the accumulator.
        // Each file gets a scratch arena that is dropped right after scanning —
        // the resulting CodebaseMetadata is owned, so nothing escapes the arena.
        let mut codebase = CodebaseMetadata::default();
        let mut files = Vec::with_capacity(paths.len());
        let mut file_index = HashMap::with_capacity(paths.len());
        let mut by_file_id = HashMap::with_capacity(paths.len());

        for path in paths {
            let file = File::read(root, &path, FileType::Host).map_err(|e| BridgeError::Io {
                path: path.display().to_string(),
                source: std::io::Error::other(e.to_string()),
            })?;

            let arena = Bump::new();
            let program = parse_file(&arena, &file);
            let resolved = NameResolver::new(&arena).resolve(program);
            let meta = scan_program(&arena, &file, program, &resolved, version);
            codebase.extend(meta);

            let idx = files.len();
            file_index.insert(String::from_utf8_lossy(&file.name).to_lowercase(), idx);
            by_file_id.insert(file.id, idx);
            files.push(file);
            // `arena`, `program`, `resolved` dropped here — `meta` was owned.
        }

        // Finalize cross-file references (inheritance, etc.).
        let mut symbol_refs = SymbolReferences::default();
        populate_codebase(
            &mut codebase,
            &mut symbol_refs,
            Default::default(),
            Default::default(),
        );

        Ok(Self { codebase, files, file_index, by_file_id })
    }

    /// The `File` a span belongs to (e.g. a class's declaring file from `span.file_id`).
    pub(crate) fn file_of_span(&self, span: &Span) -> Option<&File> {
        let idx = *self.by_file_id.get(&span.file_id)?;
        Some(&self.files[idx])
    }

    /// Version string of the mago series we are bridging.
    pub fn version() -> &'static str {
        "mago 1.30"
    }

    /// The owned codebase metadata (reflection + types).
    pub(crate) fn codebase(&self) -> &CodebaseMetadata {
        &self.codebase
    }

    /// Look up a class-like by FQCN (case-insensitive — mago lowercases keys).
    ///
    /// Uses `get_class_like` so interfaces, enums, and traits resolve too —
    /// `get_class` only matches the `class` symbol kind.
    pub(crate) fn find_class(&self, name: &str) -> Option<&ClassLikeMetadata> {
        let key = name.trim_start_matches('\\').to_lowercase();
        self.codebase.get_class_like(key.as_bytes())
    }

    /// Iterate over all class-like metadata (classes, interfaces, enums, traits).
    pub fn class_likes(&self) -> impl Iterator<Item = &ClassLikeMetadata> {
        self.codebase.class_likes.values()
    }

    /// Count of class-like entities found.
    pub fn class_like_count(&self) -> usize {
        self.codebase.class_likes.len()
    }

    /// Count of source files loaded.
    pub fn module_count(&self) -> usize {
        self.files.len()
    }

    /// Parse a loaded file on demand into a scoped scratch arena and run `f`
    /// against its AST + resolved names. The arena (and thus the `Program`) is
    /// dropped when `f` returns — callers must extract owned data inside `f`.
    pub(crate) fn with_program<R>(
        &self,
        logical_name: &str,
        f: impl FnOnce(&Program, &File, &ResolvedNames) -> R,
    ) -> Option<R> {
        let idx = *self.file_index.get(&logical_name.to_lowercase())?;
        let file = &self.files[idx];
        let arena = Bump::new();
        let program = parse_file(&arena, file);
        let resolved = NameResolver::new(&arena).resolve(program);
        Some(f(program, file, &resolved))
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
            "<?php\nclass Hello {\n    public function greet(): string { return 'hi'; }\n}\n",
        )
        .unwrap();

        let project = MagoProject::load(dir.path()).expect("load should succeed");
        assert_eq!(project.module_count(), 1, "expected 1 module");
        assert!(
            project.class_like_count() >= 1,
            "expected >=1 class-like, got {}",
            project.class_like_count()
        );
        assert!(
            project.find_class("Hello").is_some(),
            "Hello class not found in codebase"
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
        assert!(project.class_like_count() >= 3, "expected >=3 class-likes");
    }
}
