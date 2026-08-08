//! Reading files into [`Bytes`] without necessarily copying them into the
//! heap first.

use std::fs::{self, File};
use std::io;
use std::path::Path;

use typst_library::foundations::Bytes;

/// Below this size, a file is just read into a `Vec<u8>` via [`fs::read`].
/// `mmap`'s fixed costs (a syscall, page table setup) aren't worth it for
/// small files, and this also sidesteps `memmap2` erroring on a zero-length
/// file, without needing to special-case that separately.
const MMAP_THRESHOLD: u64 = 1024 * 1024;

/// Reads a file into [`Bytes`], memory-mapping it rather than copying it
/// into the heap when it's large enough for that to be worthwhile.
///
/// A memory-mapped file's pages are clean and file-backed, so the OS can
/// reclaim them under memory pressure for free (no writeback needed, unlike
/// heap memory) and re-fault them back in on demand -- unlike a
/// [`fs::read`]'d buffer, which is pinned in memory for as long as the
/// resulting `Bytes` is alive. This matters most for large assets (e.g. a
/// full-bleed poster background image), which would otherwise dominate peak
/// memory regardless of how carefully code that later reads the `Bytes`
/// avoids materializing extra copies.
///
/// This trades away one guarantee that a plain read has: if the file is
/// truncated or overwritten in place (same inode) while still mapped and
/// being read, that read can crash the process (`SIGBUS` on Unix) instead of
/// seeing old or new -- but still valid -- content. This is why only files
/// at or above [`MMAP_THRESHOLD`] are mapped at all: it's a narrow race
/// (in-place mutation of a specific file, concurrent with it being read),
/// and editors/build tools overwhelmingly save via atomic rename rather than
/// in-place truncation, which this is immune to (the old mapping is over an
/// unlinked inode, already dropped by the time a new one is read). If `mmap`
/// itself fails for any reason (unsupported filesystem, resource limits,
/// platform), this transparently falls back to a plain read.
pub fn read_file(path: &Path) -> io::Result<Bytes> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();

    if len < MMAP_THRESHOLD {
        return fs::read(path).map(Bytes::new);
    }

    // SAFETY: See this function's doc comment for the accepted risk if
    // `file` is mutated in place while mapped.
    match unsafe { memmap2::Mmap::map(&file) } {
        Ok(mmap) => Ok(Bytes::new(MappedFile(mmap))),
        Err(_) => fs::read(path).map(Bytes::new),
    }
}

/// A memory-mapped file, usable as a [`Bytes`] backing.
struct MappedFile(memmap2::Mmap);

impl AsRef<[u8]> for MappedFile {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
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
    fn test_read_file_large_mmap_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large");
        let content = vec![0x42u8; MMAP_THRESHOLD as usize + 1];
        fs::write(&path, &content).unwrap();
        assert_eq!(read_file(&path).unwrap().as_slice(), content.as_slice());
    }

    #[test]
    fn test_read_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_file(&dir.path().join("missing")).is_err());
    }
}
