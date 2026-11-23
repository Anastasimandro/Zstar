//! Raw bindings to the RAR and RAR5 readers of the vendored libarchive 3.8.11.
//!
//! Hand-written on purpose: no bindgen, no libclang. Only the read side that `zstar-rar` uses is
//! declared. This crate has no writer and no other format, so RAR can never be created through it.
//! Everything here is `unsafe` to call; `zstar-rar` is the only intended user.
#![allow(non_camel_case_types)]

use std::os::raw::{c_char, c_int, c_void};

/// Opaque `struct archive`.
#[repr(C)]
pub struct archive {
    _private: [u8; 0],
}

/// Opaque `struct archive_entry`.
#[repr(C)]
pub struct archive_entry {
    _private: [u8; 0],
}

pub type la_int64_t = i64;
pub type la_ssize_t = isize;

// `time_t` is 64-bit on every supported target (Linux, macOS and Android arm64/x86_64, Windows).
#[cfg(windows)]
pub type time_t = i64;
#[cfg(not(windows))]
pub type time_t = std::os::raw::c_long;

// libarchive declares `__LA_MODE_T` as `unsigned short` on Windows and `mode_t` elsewhere.
#[cfg(windows)]
pub type la_mode_t = u16;
#[cfg(not(windows))]
pub type la_mode_t = u32;

pub const ARCHIVE_EOF: c_int = 1;
pub const ARCHIVE_OK: c_int = 0;
pub const ARCHIVE_RETRY: c_int = -10;
pub const ARCHIVE_WARN: c_int = -20;
pub const ARCHIVE_FAILED: c_int = -25;
pub const ARCHIVE_FATAL: c_int = -30;

pub const AE_IFMT: la_mode_t = 0o170000;
pub const AE_IFREG: la_mode_t = 0o100000;
pub const AE_IFLNK: la_mode_t = 0o120000;
pub const AE_IFDIR: la_mode_t = 0o040000;

pub type archive_read_callback =
    unsafe extern "C" fn(*mut archive, *mut c_void, *mut *const c_void) -> la_ssize_t;
pub type archive_skip_callback =
    unsafe extern "C" fn(*mut archive, *mut c_void, la_int64_t) -> la_int64_t;
pub type archive_seek_callback =
    unsafe extern "C" fn(*mut archive, *mut c_void, la_int64_t, c_int) -> la_int64_t;
pub type archive_close_callback = unsafe extern "C" fn(*mut archive, *mut c_void) -> c_int;

unsafe extern "C" {
    pub fn archive_version_number() -> c_int;

    pub fn archive_read_new() -> *mut archive;
    pub fn archive_read_free(a: *mut archive) -> c_int;
    pub fn archive_read_close(a: *mut archive) -> c_int;

    pub fn archive_read_support_format_rar(a: *mut archive) -> c_int;
    pub fn archive_read_support_format_rar5(a: *mut archive) -> c_int;

    pub fn archive_read_set_callback_data(a: *mut archive, data: *mut c_void) -> c_int;
    pub fn archive_read_set_read_callback(
        a: *mut archive,
        cb: Option<archive_read_callback>,
    ) -> c_int;
    pub fn archive_read_set_skip_callback(
        a: *mut archive,
        cb: Option<archive_skip_callback>,
    ) -> c_int;
    pub fn archive_read_set_seek_callback(
        a: *mut archive,
        cb: Option<archive_seek_callback>,
    ) -> c_int;
    pub fn archive_read_set_close_callback(
        a: *mut archive,
        cb: Option<archive_close_callback>,
    ) -> c_int;
    pub fn archive_read_open1(a: *mut archive) -> c_int;

    pub fn archive_read_next_header(a: *mut archive, entry: *mut *mut archive_entry) -> c_int;
    pub fn archive_read_data_block(
        a: *mut archive,
        buf: *mut *const c_void,
        len: *mut usize,
        offset: *mut la_int64_t,
    ) -> c_int;
    pub fn archive_read_data_skip(a: *mut archive) -> c_int;
    pub fn archive_read_has_encrypted_entries(a: *mut archive) -> c_int;

    pub fn archive_error_string(a: *mut archive) -> *const c_char;
    pub fn archive_errno(a: *mut archive) -> c_int;

    pub fn archive_entry_pathname(e: *mut archive_entry) -> *const c_char;
    pub fn archive_entry_pathname_utf8(e: *mut archive_entry) -> *const c_char;
    pub fn archive_entry_symlink_utf8(e: *mut archive_entry) -> *const c_char;
    pub fn archive_entry_hardlink_utf8(e: *mut archive_entry) -> *const c_char;
    pub fn archive_entry_size(e: *mut archive_entry) -> la_int64_t;
    pub fn archive_entry_size_is_set(e: *mut archive_entry) -> c_int;
    pub fn archive_entry_filetype(e: *mut archive_entry) -> la_mode_t;
    pub fn archive_entry_perm(e: *mut archive_entry) -> la_mode_t;
    pub fn archive_entry_mtime(e: *mut archive_entry) -> time_t;
    pub fn archive_entry_mtime_is_set(e: *mut archive_entry) -> c_int;
    pub fn archive_entry_is_encrypted(e: *mut archive_entry) -> c_int;
    pub fn archive_entry_is_data_encrypted(e: *mut archive_entry) -> c_int;
    pub fn archive_entry_is_metadata_encrypted(e: *mut archive_entry) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_the_pinned_version() {
        // SAFETY: plain query with no arguments.
        assert_eq!(unsafe { archive_version_number() }, 3_008_011);
    }

    #[test]
    fn rar_readers_register_and_free() {
        // SAFETY: `a` is the handle just returned by `archive_read_new`, used only on this thread
        // and freed exactly once.
        unsafe {
            let a = archive_read_new();
            assert!(!a.is_null());
            assert_eq!(archive_read_support_format_rar(a), ARCHIVE_OK);
            assert_eq!(archive_read_support_format_rar5(a), ARCHIVE_OK);
            assert_eq!(archive_read_free(a), ARCHIVE_OK);
        }
    }
}
