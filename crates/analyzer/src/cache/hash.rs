use std::path::Path;

/// 32-byte BLAKE3 content hash, hex-encoded for filesystem use.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContentHash(String);

impl ContentHash {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }

    pub fn of_file(path: &Path) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        Ok(Self::of_bytes(&bytes))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_for_same_bytes() {
        assert_eq!(ContentHash::of_bytes(b"hello"), ContentHash::of_bytes(b"hello"));
    }

    #[test]
    fn differs_for_different_bytes() {
        assert_ne!(ContentHash::of_bytes(b"hello"), ContentHash::of_bytes(b"world"));
    }

    #[test]
    fn hashes_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "hello").unwrap();
        assert_eq!(ContentHash::of_file(&file).unwrap(), ContentHash::of_bytes(b"hello"));
    }
}
