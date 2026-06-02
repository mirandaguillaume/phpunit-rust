//! Like list_discovered but emits "file@class" per discovered class
//! (one line per class, no methods). Used by list_planned.php so the
//! script can require_once the file before reflecting.
use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;

fn main() {
    let dirs: Vec<PathBuf> = env::args().skip(1).map(PathBuf::from).collect();
    if dirs.is_empty() {
        eprintln!("usage: list_file_class <dir1> [dir2 ...]");
        std::process::exit(1);
    }
    let cases = discovery::discover_in_dirs(&dirs, &[], &[]).expect("discover_in_dirs");
    let mut uniq: BTreeMap<String, std::path::PathBuf> = BTreeMap::new();
    for c in cases {
        uniq.entry(c.class).or_insert(c.file);
    }
    for (cls, file) in uniq {
        println!("{}@{}", file.display(), cls);
    }
}
