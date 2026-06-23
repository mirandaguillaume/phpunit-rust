//! Mock-baker preprocessing: rewrite test files that use `createMock()` into
//! files with baked anonymous-class stubs, written to a temp directory.
//! Activated by `--bake-mocks` on the CLI.

use anyhow::Result;
use discovery::TestCase;
use mock_baker::{parse_interface, parse_test, Interface};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Build a PSR-4 namespace-prefix → directory list map.
/// Reads the project's `composer.json` (autoload + autoload-dev) AND every
/// installed package via `vendor/composer/installed.json` so that vendor
/// interfaces (e.g. `Psr\Http\Message\*`, `Aws\*`) are resolvable too.
pub fn psr4_map(project: &Path) -> HashMap<String, Vec<PathBuf>> {
    let mut map: HashMap<String, Vec<PathBuf>> = HashMap::new();

    // 1. Project-level autoload entries.
    if let Ok(text) = std::fs::read_to_string(project.join("composer.json")) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
            for section in ["autoload", "autoload-dev"] {
                add_psr4_block(val.get(section), project, &mut map);
            }
        }
    }

    // 2. Installed vendor packages — `vendor/composer/installed.json`.
    // Each package has an `install-path` relative to `vendor/composer/` and
    // its own `autoload.psr-4` entries relative to that package root.
    let installed_path = project.join("vendor/composer/installed.json");
    if let Ok(text) = std::fs::read_to_string(&installed_path) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
            let packages = val
                .get("packages")
                .and_then(|v| v.as_array())
                .map(|a| a.as_slice())
                .unwrap_or(&[]);
            let composer_dir = installed_path.parent().unwrap_or(project);
            for pkg in packages {
                let Some(install_path) = pkg.get("install-path").and_then(|v| v.as_str()) else {
                    continue;
                };
                // install-path is relative to vendor/composer/.
                let pkg_root = composer_dir.join(install_path);
                add_psr4_block(pkg.get("autoload"), &pkg_root, &mut map);
            }
        }
    }

    map
}

fn add_psr4_block(
    block: Option<&serde_json::Value>,
    base: &Path,
    map: &mut HashMap<String, Vec<PathBuf>>,
) {
    add_ns_block(block, "psr-4", base, map);
    // PSR-0 resolution is identical for backslash-namespaces (no underscore expansion needed).
    add_ns_block(block, "psr-0", base, map);
}

fn add_ns_block(
    block: Option<&serde_json::Value>,
    key: &str,
    base: &Path,
    map: &mut HashMap<String, Vec<PathBuf>>,
) {
    let Some(entries) = block.and_then(|b| b.get(key)).and_then(|v| v.as_object()) else {
        return;
    };
    for (ns, dirs_val) in entries {
        let dirs: Vec<PathBuf> = match dirs_val {
            serde_json::Value::String(s) => vec![base.join(s)],
            serde_json::Value::Array(arr) => arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| base.join(s))
                .collect(),
            _ => continue,
        };
        map.entry(ns.clone()).or_default().extend(dirs);
    }
}

/// Resolve a fully-qualified PHP class name to its source file using PSR-4 rules.
///
/// Algorithm:
/// 1. Strip any leading backslash from `class`.
/// 2. Among all namespace prefixes in `map`, find the LONGEST one that is a
///    prefix of `class` (longest-prefix wins: `App\Services\` before `App\`).
/// 3. For each directory mapped to that prefix:
///    - Strip the prefix from the class name.
///    - Replace remaining backslashes with `/`, append `.php`.
///    - Return the path if the file exists on disk.
/// 4. Return `None` if nothing matched or the file doesn't exist.
pub fn psr4_resolve(class: &str, map: &HashMap<String, Vec<PathBuf>>) -> Option<PathBuf> {
    let class = class.trim_start_matches('\\');

    // Longest-prefix wins: sort prefixes by length descending.
    let mut prefixes: Vec<(&str, &Vec<PathBuf>)> =
        map.iter().map(|(k, v)| (k.as_str(), v)).collect();
    prefixes.sort_by_key(|x| std::cmp::Reverse(x.0.len()));

    for (prefix, dirs) in prefixes {
        if let Some(stripped) = class.strip_prefix(prefix) {
            let relative = stripped.replace('\\', "/") + ".php";
            for dir in dirs {
                let candidate = dir.join(&relative);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// Qualify a PHP type token: resolve short names to FQN using the interface's
/// use-map and namespace. Primitives and already-FQN types pass through unchanged.
fn qualify_single_type(raw: &str, ns: &str, use_map: &HashMap<String, String>) -> String {
    let t = raw.trim();
    // Already FQN, nullable marker, or primitive — pass through unchanged.
    if t.starts_with('\\')
        || t.is_empty()
        || matches!(
            t,
            "int"
                | "string"
                | "bool"
                | "float"
                | "array"
                | "callable"
                | "iterable"
                | "null"
                | "void"
                | "never"
                | "mixed"
                | "self"
                | "static"
                | "object"
                | "false"
                | "true"
                | "resource"
                | "numeric"
                | "scalar"
        )
    {
        return t.to_string();
    }
    // Nullable prefix.
    if let Some(inner) = t.strip_prefix('?') {
        return format!("?{}", qualify_single_type(inner, ns, use_map));
    }
    // Union / intersection — split on | or & and qualify each part.
    for sep in ['|', '&'] {
        if t.contains(sep) {
            return t
                .split(sep)
                .map(|p| qualify_single_type(p.trim(), ns, use_map))
                .collect::<Vec<_>>()
                .join(&sep.to_string());
        }
    }
    // Short name — try use-map first, then qualify with interface namespace.
    if let Some(fqn) = use_map.get(t) {
        return format!("\\{}", fqn.trim_start_matches('\\'));
    }
    if !ns.is_empty() {
        format!("\\{}\\{}", ns, t)
    } else {
        t.to_string()
    }
}

/// Qualify all types in a method signature using the interface's namespace + use-map.
fn qualify_sig(
    sig: mock_baker::MethodSig,
    ns: &str,
    use_map: &HashMap<String, String>,
) -> mock_baker::MethodSig {
    // Qualify return type (strip leading `: ` if present).
    let ret_raw = sig.return_ty.trim_start_matches(':').trim();
    let qualified_ret = qualify_single_type(ret_raw, ns, use_map);
    let return_ty = if sig.return_ty.trim_start().starts_with(':') {
        format!(": {}", qualified_ret)
    } else {
        qualified_ret
    };

    // Qualify parameter types. Parameters look like: `TypeA $a, ?TypeB $b, ...`
    // Strategy: tokenize, qualify word-tokens that are directly before `$`.
    let params = qualify_param_types(&sig.params, ns, use_map);

    mock_baker::MethodSig {
        name: sig.name,
        params,
        return_ty,
        is_static: sig.is_static,
    }
}

fn qualify_param_types(params: &str, ns: &str, use_map: &HashMap<String, String>) -> String {
    // Split by comma (top-level only — no nested generics in PHP), qualify each param.
    let mut result = Vec::new();
    for param in params.split(',') {
        let p = param.trim();
        if p.is_empty() {
            result.push(p.to_string());
            continue;
        }
        // Strip leading PHP 8.x attribute groups (#[...]) before qualifying the type.
        let (attr_prefix, p_rest) = extract_attr_prefix(p);
        let p_rest = p_rest.trim();
        // A param looks like `[?]Type[|Type2] $name [= default]` or just `$name`.
        // Find the first `$` to split type from var.
        if let Some(dollar_pos) = p_rest.find('$') {
            let type_part = p_rest[..dollar_pos].trim();
            let rest = &p_rest[dollar_pos..];
            let prefix = if attr_prefix.is_empty() {
                String::new()
            } else {
                format!("{} ", attr_prefix)
            };
            if type_part.is_empty() {
                result.push(format!("{}{}", prefix, p_rest));
            } else {
                // Strip variadic marker `...` before qualifying — it's not part of the type.
                let (type_core, variadic) = if let Some(stripped) = type_part.strip_suffix("...") {
                    (stripped.trim(), "...")
                } else {
                    (type_part, "")
                };
                let qualified = qualify_single_type(type_core, ns, use_map);
                let sep = if variadic.is_empty() { " " } else { " ... " };
                result.push(format!("{}{}{}{}", prefix, qualified, sep, rest));
            }
        } else {
            result.push(p.to_string());
        }
    }
    result.join(", ")
}

/// Strip leading PHP 8.x attribute groups (`#[...]`) from a parameter string.
/// Returns `(attributes, remainder)`. Handles back-to-back attributes.
fn extract_attr_prefix(p: &str) -> (String, &str) {
    let mut attrs = String::new();
    let mut s = p;
    loop {
        let t = s.trim_start();
        if !t.starts_with("#[") {
            return (attrs, s);
        }
        if let Some(close) = t[1..].find(']') {
            let end = close + 2; // '#' + content + ']'
            if !attrs.is_empty() {
                attrs.push(' ');
            }
            attrs.push_str(&t[..end]);
            s = &t[end..];
        } else {
            return (attrs, s);
        }
    }
}

/// Recursively collect all methods from an interface and all its parent interfaces.
/// `iface_use_map` is the use-statement map of the interface's own source file,
/// used to expand short parent names to FQN.
fn collect_all_methods(
    iface: &Interface,
    iface_use_map: &HashMap<String, String>,
    psr4: &HashMap<String, Vec<PathBuf>>,
    visited: &mut std::collections::HashSet<String>,
) -> Vec<mock_baker::MethodSig> {
    // Qualify the current interface's own methods using its namespace + use-map.
    let mut all: Vec<mock_baker::MethodSig> = iface
        .methods
        .iter()
        .map(|m| qualify_sig(m.clone(), &iface.namespace, iface_use_map))
        .collect();

    for short in &iface.extends_names {
        // Expand the short parent name via the current interface's use-map.
        // If not found there, check whether it's in the same namespace (no use needed).
        let fqn = iface_use_map
            .get(short.as_str())
            .cloned()
            .unwrap_or_else(|| {
                if !iface.namespace.is_empty() && !short.contains('\\') {
                    format!("{}\\{}", iface.namespace, short)
                } else {
                    short.clone()
                }
            });
        let fqn_norm = fqn.trim_start_matches('\\').to_string();
        if !visited.insert(fqn_norm.clone()) {
            continue;
        }
        let Some(file) = psr4_resolve(&fqn_norm, psr4) else {
            continue;
        };
        let Ok(src) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Ok(parent) = parse_interface(&src) else {
            continue;
        };
        if !parent.is_interface {
            continue;
        }
        let parent_use_map = mock_baker::extract_use_map(&src).unwrap_or_default();
        let parent_methods = collect_all_methods(&parent, &parent_use_map, psr4, visited);
        for m in parent_methods {
            if !all.iter().any(|existing| existing.name == m.name) {
                all.push(m);
            }
        }
    }
    all
}

/// Resolve all interfaces/abstract-classes mocked in `src`.
/// Returns a `(iface_name, Interface)` map for every mock that could be resolved.
/// Mocks targeting concrete classes, final classes, or unresolvable types are
/// silently skipped — `emit_anon_class` will fall back to `$this->createMock()`
/// for those, giving per-createMock granularity instead of all-or-nothing per file.
/// Returns `None` only when no mock in the file can be baked at all.
fn resolve_ifaces(
    src: &str,
    psr4: &HashMap<String, Vec<PathBuf>>,
) -> Result<Option<HashMap<String, Interface>>> {
    let blocks = parse_test(src)?;
    if blocks.is_empty() {
        return Ok(None);
    }
    let use_map = mock_baker::extract_use_map(src).unwrap_or_default();

    let mut ifaces: HashMap<String, Interface> = HashMap::new();
    for block in &blocks {
        if ifaces.contains_key(&block.iface_name) {
            continue;
        }
        let fqn = use_map
            .get(&block.iface_name)
            .cloned()
            .unwrap_or_else(|| block.iface_name.clone());

        // If this interface appears in a `&MockObject` intersection type (e.g.,
        // `EntityManagerInterface&MockObject $var`), our anonymous class won't satisfy
        // the MockObject part of the intersection — skip to avoid a TypeError.
        if src.contains(&format!("{}&MockObject", block.iface_name))
            || src.contains(&format!("MockObject&{}", block.iface_name))
        {
            if std::env::var("BAKE_DEBUG").is_ok() {
                eprintln!(
                    "[bake]   skip (MockObject intersection) '{}'",
                    block.iface_name
                );
            }
            continue;
        }

        let Some(file) = psr4_resolve(&fqn, psr4) else {
            if std::env::var("BAKE_DEBUG").is_ok() {
                eprintln!(
                    "[bake]   skip (unresolvable) '{}' (fqn='{}')",
                    block.iface_name, fqn
                );
            }
            continue; // leave this mock as createMock(), try the rest
        };
        let iface_src = std::fs::read_to_string(&file)?;
        let parsed = parse_interface(&iface_src)?;
        if !parsed.is_interface && !parsed.is_abstract {
            if std::env::var("BAKE_DEBUG").is_ok() {
                eprintln!(
                    "[bake]   skip (concrete class) '{}' ({})",
                    block.iface_name,
                    file.display()
                );
            }
            continue; // leave this mock as createMock(), try the rest
        }
        // For interfaces: merge methods from the full inheritance chain.
        // For abstract classes: only use the class's own methods (chain-walking
        // abstract-class parents is left for a future improvement).
        let iface_use_map = mock_baker::extract_use_map(&iface_src).unwrap_or_default();
        let all_methods = if parsed.is_interface {
            let mut visited = std::collections::HashSet::new();
            visited.insert(fqn.trim_start_matches('\\').to_string());
            collect_all_methods(&parsed, &iface_use_map, psr4, &mut visited)
        } else {
            parsed
                .methods
                .iter()
                .map(|m| qualify_sig(m.clone(), &parsed.namespace, &iface_use_map))
                .collect()
        };
        let full = Interface {
            use_lines: parsed.use_lines,
            methods: all_methods,
            is_interface: parsed.is_interface,
            is_abstract: parsed.is_abstract,
            extends_names: parsed.extends_names,
            namespace: parsed.namespace,
        };
        ifaces.insert(block.iface_name.clone(), full);
    }
    if ifaces.is_empty() {
        Ok(None)
    } else {
        Ok(Some(ifaces))
    }
}

/// Preprocessing pass: for every test case whose source file contains a
/// `createMock` pattern, bake it into `temp_dir` and update `TestCase.file`.
///
/// Files that cannot be resolved (missing interface on disk) are left untouched.
/// The `temp_dir` must outlive the returned `Vec<TestCase>`.
pub fn bake_test_cases(
    cases: &[TestCase],
    project: &Path,
    temp_dir: &tempfile::TempDir,
) -> Vec<TestCase> {
    use rayon::prelude::*;

    let psr4 = psr4_map(project);

    // Deduplicate source files — many TestCase entries share the same file.
    let mut unique: Vec<&TestCase> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for tc in cases {
        if seen.insert(&tc.file) {
            unique.push(tc);
        }
    }

    // Bake all unique source files in parallel.
    let rewrites: HashMap<PathBuf, Option<PathBuf>> = unique
        .par_iter()
        .map(|tc| {
            let dest = try_bake_file(tc, &psr4, temp_dir);
            (tc.file.clone(), dest)
        })
        .collect();

    // Reassemble TestCase list preserving original order.
    cases
        .iter()
        .map(|tc| match rewrites.get(&tc.file) {
            Some(Some(new_file)) => TestCase {
                file: new_file.clone(),
                ..tc.clone()
            },
            _ => tc.clone(),
        })
        .collect()
}

/// Persistent bake cache directory: `~/.cache/proust/bake/`.
/// Each entry is `<blake3-of-source>.php` → the pre-baked anonymous-class file.
/// Uses the source file content as the cache key — vendor interface changes
/// invalidate the cache only if they also change the test file (good enough
/// for the common dev workflow where tests change more often than vendor).
fn bake_cache_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let dir = PathBuf::from(home).join(".cache/proust/bake");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn try_bake_file(
    tc: &TestCase,
    psr4: &HashMap<String, Vec<PathBuf>>,
    temp_dir: &tempfile::TempDir,
) -> Option<PathBuf> {
    let src = std::fs::read_to_string(&tc.file).ok()?;
    let debug = std::env::var("BAKE_DEBUG").is_ok();

    // Fast path: check disk cache before doing any tree-sitter work.
    let cache_key = blake3::hash(src.as_bytes()).to_hex();
    let cache_dir = bake_cache_dir();
    if let Some(ref dir) = cache_dir {
        let cached = dir.join(format!("{}.php", cache_key));
        if cached.is_file() {
            // Copy cached baked file into temp_dir under this class's stem.
            let stem = tc.class.replace('\\', "_");
            let dest = temp_dir.path().join(format!("{stem}.php"));
            if std::fs::copy(&cached, &dest).is_ok() {
                if debug {
                    eprintln!("[bake] cache hit → {}", tc.file.display());
                }
                return Some(dest);
            }
        }
    }
    match resolve_ifaces(&src, psr4) {
        Ok(None) => {
            if debug {
                eprintln!(
                    "[bake] skip (no mock / unresolvable): {}",
                    tc.file.display()
                );
            }
            return None;
        }
        Err(e) => {
            if debug {
                eprintln!("[bake] error {}: {e:#}", tc.file.display());
            }
            return None;
        }
        Ok(Some(_)) => {}
    }
    let ifaces = resolve_ifaces(&src, psr4).ok()??;
    let mut baked = match mock_baker::bake(&src, &ifaces) {
        Ok(b) => b,
        Err(e) => {
            if debug {
                eprintln!("[bake] bake() failed {}: {e:#}", tc.file.display());
            }
            return None;
        }
    };

    // Replace PHP magic constants __DIR__ and __FILE__ with the original file's
    // absolute paths so that relative path computations in the baked file still work.
    if let Ok(abs) = tc.file.canonicalize() {
        let orig_dir = abs.parent().unwrap_or(&abs);
        let escape = |s: &str| s.replace('\\', "\\\\").replace('\'', "\\'");
        baked = baked.replace(
            "__DIR__",
            &format!("'{}'", escape(&orig_dir.to_string_lossy())),
        );
        baked = baked.replace("__FILE__", &format!("'{}'", escape(&abs.to_string_lossy())));
    }

    // Use the class name as file stem to avoid collisions between classes
    // that share a filename (same-file multi-class, rare but possible).
    let stem = tc.class.replace('\\', "_");
    let dest = temp_dir.path().join(format!("{stem}.php"));
    if let Err(e) = std::fs::write(&dest, &baked) {
        if debug {
            eprintln!("[bake] write failed {}: {e}", tc.file.display());
        }
        return None;
    }
    // Persist to disk cache so subsequent runs skip re-parsing.
    if let Some(ref dir) = cache_dir {
        let _ = std::fs::write(dir.join(format!("{}.php", cache_key)), &baked);
    }
    if let Ok(dump_dir) = std::env::var("BAKE_DUMP_DIR") {
        let dump_name = format!("{}.php", tc.class.replace('\\', "_"));
        let _ = std::fs::write(std::path::Path::new(&dump_dir).join(&dump_name), &baked);
    }
    if debug {
        eprintln!("[bake] OK → {}", tc.file.display());
    }
    Some(dest)
}
