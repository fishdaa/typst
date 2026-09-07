//! Reading files into immutable byte snapshots.

use std::fs;
use std::io;
use std::path::Path;

use typst_library::foundations::Bytes;

/// Reads a file into owned bytes. Inputs may be rewritten or truncated by
/// editors and asset generators while compilation is running, so retaining
/// a file-backed mapping would invalidate shared references or cause SIGBUS.
pub fn read_file(path: &Path) -> io::Result<Bytes> {
    fs::read(path).map(Bytes::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_file_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty");
        fs::write(&path, []).unwrap();
        assert_eq!(read_file(&path).unwrap().as_slice(), b"");
    }

    #[test]
    fn test_read_file_small() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small");
        fs::write(&path, b"hello world").unwrap();
        assert_eq!(read_file(&path).unwrap().as_slice(), b"hello world");
    }

    #[test]
    fn test_read_file_large() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large");
        let content = vec![0x42_u8; 1024 * 1024 + 1];
        fs::write(&path, &content).unwrap();
        assert_eq!(read_file(&path).unwrap().as_slice(), content.as_slice());
    }

    #[test]
    fn test_read_file_survives_in_place_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mutable");
        let content = vec![0x42_u8; 1024 * 1024 + 1];
        fs::write(&path, &content).unwrap();
        let bytes = read_file(&path).unwrap();
        fs::write(&path, []).unwrap();
        assert_eq!(bytes.as_slice(), content.as_slice());
    }

    #[test]
    fn test_read_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_file(&dir.path().join("missing")).is_err());
    }
}
