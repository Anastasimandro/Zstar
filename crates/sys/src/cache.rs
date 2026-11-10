//! Page-cache hygiene for big sequential transfers (ADR 0012, step 3).
//!
//! A long copy fills the page cache with data nobody will read again, pushes out what the
//! user was using, and lets the dirty pages pile up until the kernel stalls every writer at
//! once. The engine cuts a transfer into windows and asks for two things, one window apart:
//! start writing the window it just finished ([`CacheOp::StartWriteback`]), and for the one
//! before it, wait until it is on its way to the drive and let the cache forget it
//! ([`CacheOp::Release`]). The wait is the bound: a drive slower than the engine slows the
//! engine down instead of the whole machine.
//!
//! Linux only for now. Android has no `sync_file_range` in the `libc` crate and Windows and
//! macOS manage their cache differently (nothing here was measured there), so on those
//! systems every call does nothing and reports `false`. No call changes what is stored and
//! none of them is `fsync`: the data is not promised to be durable.

use std::fs::File;

/// What to do with a stretch of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOp {
    /// Start writing the dirty pages of the range to the drive and return at once.
    StartWriteback,
    /// Wait until the range has been written (a clean or read range returns at once), then
    /// drop it from the page cache.
    Release,
}

/// Applies `op` to the bytes `offset..offset + len` of `file`; `len` 0 means up to the end of
/// the file. Best effort: returns whether the OS accepted it, and a `false` never means the
/// transfer is wrong, only that the hint did nothing.
pub fn cache_op(file: &File, offset: u64, len: u64, op: CacheOp) -> bool {
    #[cfg(target_os = "linux")]
    return linux(file, offset, len, op);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (file, offset, len, op);
        false
    }
}

#[cfg(target_os = "linux")]
fn linux(file: &File, offset: u64, len: u64, op: CacheOp) -> bool {
    use std::os::fd::AsRawFd;
    let (Ok(off), Ok(n)) = (
        libc::off64_t::try_from(offset),
        libc::off64_t::try_from(len),
    ) else {
        return false;
    };
    let fd = file.as_raw_fd();
    // SAFETY (both calls below): they take the descriptor, which `file` keeps open for the
    // whole call, and plain integers; they read and write no memory of ours.
    match op {
        CacheOp::StartWriteback => unsafe {
            libc::sync_file_range(fd, off, n, libc::SYNC_FILE_RANGE_WRITE) == 0
        },
        CacheOp::Release => {
            let flags = libc::SYNC_FILE_RANGE_WAIT_BEFORE
                | libc::SYNC_FILE_RANGE_WRITE
                | libc::SYNC_FILE_RANGE_WAIT_AFTER;
            let waited = unsafe { libc::sync_file_range(fd, off, n, flags) } == 0;
            // Dropped even if the wait failed: a clean page goes, a dirty one stays.
            let dropped =
                unsafe { libc::posix_fadvise(fd, off, n, libc::POSIX_FADV_DONTNEED) } == 0;
            waited && dropped
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::AsRawFd;

    const MIB: usize = 1 << 20;

    /// How many pages of the first `len` bytes of `file` are in the page cache.
    fn resident(file: &File, len: usize) -> usize {
        // SAFETY: the mapping is read-only and private, `len` is not more than the file's
        // size, `mincore` writes one byte per page into `vec`, which has room for all of them,
        // and the mapping is removed before returning.
        unsafe {
            let page = usize::try_from(libc::sysconf(libc::_SC_PAGESIZE)).unwrap();
            let map = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            );
            assert_ne!(map, libc::MAP_FAILED);
            let mut vec = vec![0u8; len.div_ceil(page)];
            assert_eq!(libc::mincore(map, len, vec.as_mut_ptr()), 0);
            libc::munmap(map, len);
            vec.iter().filter(|b| *b & 1 == 1).count()
        }
    }

    fn written_file(len: usize) -> (tempfile::TempDir, File) {
        let dir = tempfile::tempdir().unwrap();
        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.path().join("f"))
            .unwrap();
        file.write_all(&vec![7u8; len]).unwrap();
        (dir, file)
    }

    /// The temp dir may sit on tmpfs, where pages cannot be dropped, which says nothing
    /// about the code. Skip there instead of failing.
    fn on_tmpfs(file: &File) -> bool {
        // SAFETY: `fstatfs` fills the struct we give it and reads nothing else.
        unsafe {
            let mut st: libc::statfs = std::mem::zeroed();
            libc::fstatfs(file.as_raw_fd(), &mut st) == 0 && st.f_type as i64 == 0x0102_1994
        }
    }

    #[test]
    fn release_waits_for_the_range_and_drops_it_from_the_cache() {
        let (_dir, file) = written_file(8 * MIB);
        if on_tmpfs(&file) {
            return;
        }
        assert!(
            resident(&file, 8 * MIB) > 1000,
            "the write should be cached"
        );
        assert!(cache_op(
            &file,
            0,
            (4 * MIB) as u64,
            CacheOp::StartWriteback
        ));
        assert!(cache_op(&file, 0, (4 * MIB) as u64, CacheOp::Release));
        let page = 4096;
        let first = resident(&file, 4 * MIB);
        let second = resident(&file, 8 * MIB) - first;
        assert!(
            first < 4 * MIB / page / 10,
            "released half still resident: {first}"
        );
        assert!(
            second > 4 * MIB / page / 2,
            "the other half must stay: {second}"
        );
    }

    #[test]
    fn length_zero_means_to_the_end_and_a_bad_range_is_refused_not_fatal() {
        let (_dir, file) = written_file(2 * MIB);
        if on_tmpfs(&file) {
            return;
        }
        assert!(cache_op(&file, 0, 0, CacheOp::Release));
        assert!(resident(&file, 2 * MIB) < 2 * MIB / 4096 / 10);
        assert!(!cache_op(&file, u64::MAX, 1, CacheOp::Release));
        assert!(!cache_op(&file, 0, u64::MAX, CacheOp::StartWriteback));
    }
}
