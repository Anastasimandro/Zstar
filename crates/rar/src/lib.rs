//! Read-only RAR and RAR5 reader over any `Read + Seek` stream, built on vendored libarchive.
//!
//! This crate only reads: there is no way to create RAR here or anywhere else in Zstar. Together
//! with `zstar-rar-sys` it holds all the `unsafe` code that RAR support needs. Encrypted archives
//! and multi-volume sets are refused with a typed error.
//!
//! The reader is an iterator over entries: [`RarArchive::next_entry`] returns the next header and
//! [`RarArchive::read_data`] then copies that entry's data out from libarchive's block buffer, so
//! memory stays flat however large the entry is. Path safety and size limits are the
//! caller's job, as for every other format.
//!
//! # Panics at the C boundary
//!
//! Every callback libarchive calls into Rust runs under `catch_unwind`, so a panic in the user's
//! `Read` or `Seek` never unwinds through C: with the default `panic = "unwind"` it becomes
//! [`RarError::Internal`]. Zstar's release profile sets `panic = "abort"`, where `catch_unwind`
//! cannot catch anything and the same panic aborts the process. That is still memory-safe (nothing
//! unwinds through C frames), but it is not a typed error. The tests cover the unwind case only.

mod error;
mod header;

use std::ffi::{CStr, c_void};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::raw::c_int;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{self, NonNull};

use zstar_rar_sys as sys;

pub use error::RarError;
pub use header::{SIGNATURE_RAR4, SIGNATURE_RAR5, is_rar_signature};

/// Size of the block libarchive asks the stream for.
const READ_BLOCK: usize = 128 * 1024;

/// What kind of thing a RAR entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Hardlink,
    Other,
}

/// One entry header.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Name as stored, decoded to UTF-8; not sanitised.
    pub name: String,
    pub kind: EntryKind,
    /// Declared uncompressed size. A header can lie: count the bytes actually read.
    pub size: Option<u64>,
    /// Unix permission bits, when recorded.
    pub mode: Option<u32>,
    /// Modification time as seconds since the Unix epoch, when recorded.
    pub mtime: Option<i64>,
    pub link_target: Option<String>,
}

type Result<T> = std::result::Result<T, RarError>;

/// Everything the C callbacks touch. Lives behind a raw pointer for as long as the libarchive
/// handle does.
struct State<R> {
    reader: R,
    buf: Vec<u8>,
    io_error: Option<io::Error>,
    panicked: bool,
}

pub struct RarArchive<R: Read + Seek> {
    handle: NonNull<sys::archive>,
    // Owned through `Box::into_raw` and freed in `Drop`, after the handle: moving a `Box` would
    // invalidate the pointer libarchive keeps.
    state: *mut State<R>,
    /// Bytes of the current entry delivered so far.
    pos: u64,
    in_entry: bool,
    poisoned: bool,
    /// Undelivered rest of the last block libarchive returned. Points into libarchive's own
    /// buffer, which stays valid until the next call on the handle; that call is only made once
    /// this is empty.
    pending: *const u8,
    pending_len: usize,
}

impl<R: Read + Seek> RarArchive<R> {
    /// Opens the archive at the start of `reader`.
    ///
    /// Fails with [`RarError::NotRar`], [`RarError::Encrypted`] (encrypted headers) or
    /// [`RarError::MultiVolume`] before libarchive sees any data.
    pub fn open(mut reader: R) -> Result<Self> {
        let mut head = [0u8; header::HEAD_LEN];
        let n = read_up_to(&mut reader, &mut head)?;
        reader.seek(SeekFrom::Start(0))?;
        let inspection = header::inspect(&head[..n])?;
        if inspection.encrypted_headers {
            return Err(RarError::Encrypted);
        }
        if inspection.multi_volume {
            return Err(RarError::MultiVolume);
        }

        let state = Box::into_raw(Box::new(State {
            reader,
            buf: vec![0u8; READ_BLOCK],
            io_error: None,
            panicked: false,
        }));

        // SAFETY: plain constructor call.
        let Some(handle) = NonNull::new(unsafe { sys::archive_read_new() }) else {
            // SAFETY: `state` came from `Box::into_raw` above and nothing else holds it.
            drop(unsafe { Box::from_raw(state) });
            return Err(RarError::Internal("out of memory".into()));
        };
        // From here `Drop` releases both the handle and the state on every early return.
        let mut archive = RarArchive {
            handle,
            state,
            pos: 0,
            in_entry: false,
            poisoned: false,
            pending: ptr::null(),
            pending_len: 0,
        };

        let h = archive.handle.as_ptr();
        // SAFETY: `h` is the live handle owned by `archive`. `state` outlives it (see `Drop`) and
        // is only touched by the callbacks, which libarchive calls from the thread that drives
        // the archive, never concurrently. The callbacks are instances of the same generic `R`
        // as the pointer they receive.
        let rc = unsafe {
            let mut rc = sys::archive_read_support_format_rar(h);
            if rc == sys::ARCHIVE_OK {
                rc = sys::archive_read_support_format_rar5(h);
            }
            sys::archive_read_set_callback_data(h, state.cast::<c_void>());
            sys::archive_read_set_read_callback(h, Some(read_cb::<R>));
            sys::archive_read_set_skip_callback(h, Some(skip_cb::<R>));
            sys::archive_read_set_seek_callback(h, Some(seek_cb::<R>));
            sys::archive_read_set_close_callback(h, Some(close_cb));
            if rc == sys::ARCHIVE_OK {
                rc = sys::archive_read_open1(h);
            }
            rc
        };
        if rc != sys::ARCHIVE_OK {
            return Err(archive.fail(rc));
        }
        Ok(archive)
    }

    /// Advances to the next entry. `None` after the last one.
    pub fn next_entry(&mut self) -> Result<Option<Entry>> {
        self.check_usable()?;
        self.pos = 0;
        self.in_entry = false;
        self.pending_len = 0;
        let mut raw: *mut sys::archive_entry = ptr::null_mut();
        // SAFETY: live handle; libarchive writes an entry pointer that stays valid until the next
        // call on this handle, and it is read only below, before that call.
        let rc = unsafe { sys::archive_read_next_header(self.handle.as_ptr(), &mut raw) };
        match rc {
            sys::ARCHIVE_EOF => Ok(None),
            sys::ARCHIVE_OK | sys::ARCHIVE_WARN if !raw.is_null() => {
                // SAFETY: `raw` is non-null and valid, as above.
                let entry = unsafe { read_entry(raw) };
                // SAFETY: same pointer.
                let encrypted = unsafe {
                    sys::archive_entry_is_encrypted(raw) != 0
                        || sys::archive_entry_is_data_encrypted(raw) != 0
                        || sys::archive_entry_is_metadata_encrypted(raw) != 0
                };
                if encrypted {
                    self.poisoned = true;
                    return Err(RarError::Encrypted);
                }
                self.in_entry = true;
                Ok(Some(entry))
            }
            other => Err(self.fail(other)),
        }
    }

    /// Copies the next bytes of the current entry's data into `out` and returns how many; 0
    /// means the entry is finished. Call after `next_entry`. Memory use does not depend on the
    /// entry size: data goes straight from libarchive's block to `out`.
    pub fn read_data(&mut self, out: &mut [u8]) -> Result<usize> {
        self.check_usable()?;
        if out.is_empty() {
            return Ok(0);
        }
        while self.pending_len == 0 {
            if !self.in_entry || !self.fill()? {
                return Ok(0);
            }
        }
        let n = out.len().min(self.pending_len);
        // SAFETY: `pending` points to `pending_len >= n` readable bytes of libarchive's block
        // (see the field), `out` has room for `n`, and the two cannot overlap: one is C-owned.
        unsafe {
            ptr::copy_nonoverlapping(self.pending, out.as_mut_ptr(), n);
            self.pending = self.pending.add(n);
        }
        self.pending_len -= n;
        Ok(n)
    }

    /// Asks libarchive for the next block. `false` at the end of the entry.
    fn fill(&mut self) -> Result<bool> {
        let mut buf: *const c_void = ptr::null();
        let mut len = 0usize;
        let mut offset = 0i64;
        // SAFETY: live handle; the three out-pointers are valid locals.
        let rc = unsafe {
            sys::archive_read_data_block(self.handle.as_ptr(), &mut buf, &mut len, &mut offset)
        };
        match rc {
            sys::ARCHIVE_EOF => {
                self.in_entry = false;
                Ok(false)
            }
            sys::ARCHIVE_OK | sys::ARCHIVE_WARN => {
                if u64::try_from(offset).ok() != Some(self.pos) {
                    self.poisoned = true;
                    return Err(RarError::Unsupported("sparse entry".into()));
                }
                self.pos += len as u64;
                self.pending = buf.cast::<u8>();
                self.pending_len = if buf.is_null() { 0 } else { len };
                Ok(true)
            }
            other => Err(self.fail(other)),
        }
    }

    fn check_usable(&self) -> Result<()> {
        if self.poisoned {
            Err(RarError::Internal(
                "archive unusable after an earlier error".into(),
            ))
        } else {
            Ok(())
        }
    }

    /// Turns a failing libarchive return code into the most specific error available.
    fn fail(&mut self, rc: c_int) -> RarError {
        if rc == sys::ARCHIVE_FATAL || rc == sys::ARCHIVE_RETRY || rc == sys::ARCHIVE_FAILED {
            self.poisoned = true;
        }
        // SAFETY: `state` is live (see `Drop`) and no callback is running.
        let state = unsafe { &mut *self.state };
        if state.panicked {
            return RarError::Internal("panic in the stream callback".into());
        }
        if let Some(e) = state.io_error.take() {
            return RarError::Io(e);
        }
        // SAFETY: live handle; the string is copied before the next libarchive call.
        let (message, has_encrypted) = unsafe {
            let p = sys::archive_error_string(self.handle.as_ptr());
            let msg = if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            };
            (
                msg,
                sys::archive_read_has_encrypted_entries(self.handle.as_ptr()) == 1,
            )
        };
        classify(message, has_encrypted)
    }
}

impl<R: Read + Seek> Drop for RarArchive<R> {
    fn drop(&mut self) {
        // SAFETY: the handle is freed exactly once, first, because `archive_read_free` runs the
        // close callback. `state` is freed after it, exactly once, and was never freed before.
        unsafe {
            sys::archive_read_free(self.handle.as_ptr());
            drop(Box::from_raw(self.state));
        }
    }
}

fn classify(message: String, has_encrypted: bool) -> RarError {
    let lower = message.to_ascii_lowercase();
    if has_encrypted || lower.contains("encrypt") || lower.contains("password") {
        RarError::Encrypted
    } else if lower.contains("unsupported")
        || lower.contains("unavailable")
        || lower.contains("not supported")
    {
        RarError::Unsupported(message)
    } else {
        RarError::Corrupt(message)
    }
}

/// # Safety
/// `raw` must point to a valid entry returned by `archive_read_next_header`.
// The `from` calls are identity on Unix and a widening from `u16` on Windows (`la_mode_t`).
#[allow(clippy::useless_conversion)]
unsafe fn read_entry(raw: *mut sys::archive_entry) -> Entry {
    // SAFETY: the caller guarantees `raw`; every returned string is copied right away.
    unsafe {
        let name = text(sys::archive_entry_pathname_utf8(raw))
            .or_else(|| text(sys::archive_entry_pathname(raw)))
            .unwrap_or_default();
        let link = text(sys::archive_entry_hardlink_utf8(raw));
        let symlink = text(sys::archive_entry_symlink_utf8(raw));
        let kind = if link.is_some() {
            EntryKind::Hardlink
        } else {
            match sys::archive_entry_filetype(raw) & sys::AE_IFMT {
                sys::AE_IFREG => EntryKind::File,
                sys::AE_IFDIR => EntryKind::Dir,
                sys::AE_IFLNK => EntryKind::Symlink,
                _ => EntryKind::Other,
            }
        };
        let size = (sys::archive_entry_size_is_set(raw) != 0)
            .then(|| u64::try_from(sys::archive_entry_size(raw)).unwrap_or(0));
        let mtime = (sys::archive_entry_mtime_is_set(raw) != 0)
            .then(|| i64::from(sys::archive_entry_mtime(raw)));
        Entry {
            name,
            kind,
            size,
            mode: Some(u32::from(sys::archive_entry_perm(raw))),
            mtime,
            link_target: link.or(symlink),
        }
    }
}

/// # Safety
/// `p` must be null or a NUL-terminated string.
unsafe fn text(p: *const std::os::raw::c_char) -> Option<String> {
    if p.is_null() {
        None
    } else {
        // SAFETY: non-null and NUL-terminated per the caller.
        Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
    }
}

fn read_up_to<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Runs `f` on the callback state, never letting a panic cross into C. An error or panic is
/// parked in the state for [`RarArchive::fail`] and `fail_value` goes back to libarchive.
fn guard<R, T>(
    state: &mut State<R>,
    fail_value: T,
    f: impl FnOnce(&mut State<R>) -> io::Result<T>,
) -> T {
    match catch_unwind(AssertUnwindSafe(|| f(&mut *state))) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            state.io_error = Some(e);
            fail_value
        }
        Err(_) => {
            state.panicked = true;
            fail_value
        }
    }
}

unsafe extern "C" fn read_cb<R: Read + Seek>(
    _a: *mut sys::archive,
    data: *mut c_void,
    out: *mut *const c_void,
) -> sys::la_ssize_t {
    // SAFETY: `data` is the `State<R>` registered in `open`, alive and used by one thread at a
    // time; `out` is a valid pointer supplied by libarchive.
    let state = unsafe { &mut *data.cast::<State<R>>() };
    guard(state, -1, |st| {
        let n = loop {
            match st.reader.read(&mut st.buf) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        };
        // SAFETY: `out` is valid (above); the buffer stays alive and untouched until the next call.
        unsafe { *out = st.buf.as_ptr().cast::<c_void>() };
        Ok(n as sys::la_ssize_t)
    })
}

unsafe extern "C" fn skip_cb<R: Read + Seek>(
    _a: *mut sys::archive,
    data: *mut c_void,
    request: sys::la_int64_t,
) -> sys::la_int64_t {
    // SAFETY: as in `read_cb`.
    let state = unsafe { &mut *data.cast::<State<R>>() };
    guard(state, -1, |st| {
        let before = st.reader.stream_position()?;
        let after = st.reader.seek(SeekFrom::Current(request))?;
        Ok(i64::try_from(after.saturating_sub(before)).unwrap_or(i64::MAX))
    })
}

unsafe extern "C" fn seek_cb<R: Read + Seek>(
    _a: *mut sys::archive,
    data: *mut c_void,
    offset: sys::la_int64_t,
    whence: c_int,
) -> sys::la_int64_t {
    // SAFETY: as in `read_cb`.
    let state = unsafe { &mut *data.cast::<State<R>>() };
    guard(state, -1, |st| {
        let to = match whence {
            0 => SeekFrom::Start(u64::try_from(offset).map_err(|_| io::ErrorKind::InvalidInput)?),
            1 => SeekFrom::Current(offset),
            2 => SeekFrom::End(offset),
            _ => return Err(io::ErrorKind::InvalidInput.into()),
        };
        let pos = st.reader.seek(to)?;
        i64::try_from(pos).map_err(|_| io::ErrorKind::InvalidData.into())
    })
}

unsafe extern "C" fn close_cb(_a: *mut sys::archive, _data: *mut c_void) -> c_int {
    // The stream belongs to `State`, which `Drop` frees; nothing to release here.
    sys::ARCHIVE_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn refuses_data_that_is_not_rar() {
        let err = RarArchive::open(Cursor::new(b"PK\x03\x04 not rar".to_vec()))
            .err()
            .unwrap();
        assert!(matches!(err, RarError::NotRar));
    }

    #[test]
    fn refuses_an_empty_stream() {
        let err = RarArchive::open(Cursor::new(Vec::new())).err().unwrap();
        assert!(matches!(err, RarError::NotRar));
    }

    #[test]
    fn refuses_volume_flag_and_encrypted_headers_before_libarchive() {
        let mut volume = SIGNATURE_RAR5.to_vec();
        volume.extend_from_slice(&[0, 0, 0, 0, 3, 1, 0, 0x01]);
        assert!(matches!(
            RarArchive::open(Cursor::new(volume)).err().unwrap(),
            RarError::MultiVolume
        ));

        let mut encrypted = SIGNATURE_RAR4.to_vec();
        encrypted.extend_from_slice(&[0, 0, 0x73, 0x80, 0x00]);
        assert!(matches!(
            RarArchive::open(Cursor::new(encrypted)).err().unwrap(),
            RarError::Encrypted
        ));
    }

    /// Serves a valid-looking signature on the first pass, then panics once libarchive reads.
    struct PanicsAfterPeek {
        head: Cursor<Vec<u8>>,
        peeked: bool,
    }
    impl Read for PanicsAfterPeek {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.peeked {
                panic!("stream panicked on purpose");
            }
            self.head.read(buf)
        }
    }
    impl Seek for PanicsAfterPeek {
        fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
            self.peeked = true;
            self.head.seek(to)
        }
    }

    #[test]
    fn a_panicking_stream_becomes_an_error_not_an_unwind_through_c() {
        let mut bytes = SIGNATURE_RAR5.to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 0, 3, 1, 0, 0x00]);
        let stream = PanicsAfterPeek {
            head: Cursor::new(bytes),
            peeked: false,
        };
        let err = match RarArchive::open(stream) {
            Err(e) => e,
            Ok(mut a) => a.next_entry().expect_err("reading must fail"),
        };
        assert!(matches!(err, RarError::Internal(_)), "{err:?}");
    }

    struct FailsAfterPeek {
        head: Cursor<Vec<u8>>,
        peeked: bool,
    }
    impl Read for FailsAfterPeek {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.peeked {
                return Err(io::Error::other("disk gone"));
            }
            self.head.read(buf)
        }
    }
    impl Seek for FailsAfterPeek {
        fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
            self.peeked = true;
            self.head.seek(to)
        }
    }

    #[test]
    fn stream_errors_are_returned_as_io_errors() {
        let mut bytes = SIGNATURE_RAR5.to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 0, 3, 1, 0, 0x00]);
        let stream = FailsAfterPeek {
            head: Cursor::new(bytes),
            peeked: false,
        };
        let err = match RarArchive::open(stream) {
            Err(e) => e,
            Ok(mut a) => a.next_entry().expect_err("reading must fail"),
        };
        assert!(
            matches!(&err, RarError::Io(e) if e.to_string().contains("disk gone")),
            "{err:?}"
        );
    }

    #[test]
    fn classify_picks_typed_errors() {
        assert!(matches!(
            classify("RAR encryption support unavailable".into(), false),
            RarError::Encrypted
        ));
        assert!(matches!(classify("x".into(), true), RarError::Encrypted));
        assert!(matches!(
            classify("Parsing filters is unsupported.".into(), false),
            RarError::Unsupported(_)
        ));
        assert!(matches!(
            classify("Bad RAR file".into(), false),
            RarError::Corrupt(_)
        ));
    }
}
