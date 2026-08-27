//! Reading files into [`Bytes`] without necessarily copying them into the
//! heap first.

use std::fs::{self, File};
use std::io;
use std::ops::Range;
use std::path::Path;

use typst_library::foundations::{Bytes, Paged};

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
        Ok(mmap) => {
            // Large assets are read front-to-back exactly once (a PNG's
            // pixel data can only be decompressed sequentially), so tell the
            // kernel to read ahead aggressively and drop pages behind the
            // read position on its own. Best-effort: an error here only
            // costs performance.
            let _ = mmap.advise(memmap2::Advice::Sequential);
            Ok(Bytes::from_paged(MappedFile { mmap }))
        }
        Err(_) => fs::read(path).map(Bytes::new),
    }
}

/// A memory-mapped file, usable as a [`Bytes`] backing.
struct MappedFile {
    mmap: memmap2::Mmap,
}

impl Paged for MappedFile {
    fn as_bytes(&self) -> &[u8] {
        &self.mmap
    }

    /// Releases whole pages of `range` with `MADV_DONTNEED`.
    ///
    /// The mapping is a read-only private file mapping, so its pages are
    /// always clean: dropping them needs no writeback and loses nothing --
    /// a later read of the same range simply faults it back in from the
    /// file. This is what keeps a large asset (e.g. an 80 MiB poster
    /// background PNG) from staying fully resident for the whole export
    /// just because something read it from start to finish; a
    /// memory-constrained cgroup charges those clean pages the same as heap
    /// memory.
    ///
    /// Stateless, so a range that a previous pass already released can be
    /// released again after a later pass faulted it back in.
    fn release(&self, range: Range<usize>) {
        // Round the start up and the end down: a partially-consumed page at
        // either edge is likely still in use by the caller.
        let page = page_size();
        let start = range.start.next_multiple_of(page);
        let end = range.end.min(self.mmap.len()) & !(page - 1);
        if end <= start {
            return;
        }

        // SAFETY: `MADV_DONTNEED` is only unsafe for a mapping that can
        // hold un-written-back data. This is a read-only private mapping of
        // a file, so every page is clean and re-readable from the file, and
        // `memmap2` hands out only shared references to it.
        let _ = unsafe {
            self.mmap.unchecked_advise_range(
                memmap2::UncheckedAdvice::DontNeed,
                start,
                end - start,
            )
        };
    }
}

/// The system page size, which `advise_range` requires offsets to be aligned
/// to. Falls back to 4 KiB, the near-universal value, if the query fails.
fn page_size() -> usize {
    #[cfg(unix)]
    {
        // SAFETY: `sysconf` is always safe to call; it only reads a system
        // parameter.
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if size > 0 {
            return size as usize;
        }
    }
    4096
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
        let content = vec![0x42_u8; MMAP_THRESHOLD as usize + 1];
        fs::write(&path, &content).unwrap();
        assert_eq!(read_file(&path).unwrap().as_slice(), content.as_slice());
    }

    #[test]
    fn test_read_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_file(&dir.path().join("missing")).is_err());
    }
}
