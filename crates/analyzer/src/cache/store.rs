use super::hash::ContentHash;
use serde::{de::DeserializeOwned, Serialize};
use std::path::{Path, PathBuf};

const CACHE_FORMAT_VERSION: u32 = 1;

pub struct CacheStore {
    root: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("encode error: {0}")]
    Encode(#[from] bincode::Error),
}

impl CacheStore {
    /// Create a store at `.pcov-rs/cache/<namespace>` relative to project root.
    /// Namespace combines tool version, Mago version, and format version so
    /// binary upgrades silently invalidate the cache.
    pub fn open(project_root: &Path, mago_version: &str) -> Result<Self, CacheError> {
        let pcov_version = env!("CARGO_PKG_VERSION");
        let namespace = format!("v{CACHE_FORMAT_VERSION}-pcov{pcov_version}-mago{mago_version}");
        let root = project_root.join(".pcov-rs").join("cache").join(&namespace);
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn put<T: Serialize>(&self, kind: &str, key: &ContentHash, value: &T) -> Result<(), CacheError> {
        let path = self.entry_path(kind, key);
        std::fs::create_dir_all(path.parent().unwrap())?;
        let bytes = bincode::serialize(value)?;
        std::fs::write(&path, bytes)?;
        Ok(())
    }

    pub fn get<T: DeserializeOwned>(&self, kind: &str, key: &ContentHash) -> Result<Option<T>, CacheError> {
        let path = self.entry_path(kind, key);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&path)?;
        let value = bincode::deserialize(&bytes)?;
        Ok(Some(value))
    }

    pub fn root(&self) -> &Path { &self.root }

    fn entry_path(&self, kind: &str, key: &ContentHash) -> PathBuf {
        let hex = key.as_str();
        self.root.join(kind).join(&hex[..2]).join(&hex[2..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Sample { name: String, count: u32 }

    #[test]
    fn put_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = CacheStore::open(dir.path(), "0.26.1").unwrap();
        let key = ContentHash::of_bytes(b"some-file");
        let value = Sample { name: "alice".into(), count: 42 };
        store.put("discovery", &key, &value).unwrap();
        let got: Option<Sample> = store.get("discovery", &key).unwrap();
        assert_eq!(got, Some(value));
    }

    #[test]
    fn missing_entry_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = CacheStore::open(dir.path(), "0.26.1").unwrap();
        let got: Option<Sample> = store.get("discovery", &ContentHash::of_bytes(b"no-such")).unwrap();
        assert_eq!(got, None);
    }

    #[test]
    fn different_namespaces_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let a = CacheStore::open(dir.path(), "0.26.1").unwrap();
        let b = CacheStore::open(dir.path(), "0.27.0").unwrap();
        let key = ContentHash::of_bytes(b"same-file");
        let value = Sample { name: "x".into(), count: 1 };
        a.put("discovery", &key, &value).unwrap();
        let got: Option<Sample> = b.get("discovery", &key).unwrap();
        assert_eq!(got, None, "different mago versions must not share cache");
    }
}
