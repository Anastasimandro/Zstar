//! Looks at the first bytes of a stream before libarchive does, so that encryption and
//! multi-volume sets get a typed error that does not depend on libarchive's message text.

use crate::error::RarError;

pub const SIGNATURE_RAR4: [u8; 7] = [0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x00];
pub const SIGNATURE_RAR5: [u8; 8] = [0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x01, 0x00];

/// Bytes `inspect` needs to see to decide.
pub const HEAD_LEN: usize = 64;

/// True when `prefix` starts with a RAR 1.5-4.x or RAR 5 signature.
pub fn is_rar_signature(prefix: &[u8]) -> bool {
    prefix.starts_with(&SIGNATURE_RAR5) || prefix.starts_with(&SIGNATURE_RAR4)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Inspection {
    pub multi_volume: bool,
    pub encrypted_headers: bool,
}

/// Reads the signature and the main (or encryption) header flags. A header cut short is not an
/// error here: both flags stay false and libarchive reports the truncation.
pub(crate) fn inspect(head: &[u8]) -> Result<Inspection, RarError> {
    if head.starts_with(&SIGNATURE_RAR5) {
        Ok(inspect_rar5(&head[SIGNATURE_RAR5.len()..]))
    } else if head.starts_with(&SIGNATURE_RAR4) {
        Ok(inspect_rar4(&head[SIGNATURE_RAR4.len()..]))
    } else {
        Err(RarError::NotRar)
    }
}

const NONE: Inspection = Inspection {
    multi_volume: false,
    encrypted_headers: false,
};

// RAR 1.5-4.x main header: CRC16 (2), type (1) = 0x73, flags (2, little endian).
fn inspect_rar4(h: &[u8]) -> Inspection {
    const MAIN: u8 = 0x73;
    const VOLUME: u16 = 0x0001;
    const HEADERS_ENCRYPTED: u16 = 0x0080;
    if h.len() < 5 || h[2] != MAIN {
        return NONE;
    }
    let flags = u16::from_le_bytes([h[3], h[4]]);
    Inspection {
        multi_volume: flags & VOLUME != 0,
        encrypted_headers: flags & HEADERS_ENCRYPTED != 0,
    }
}

// RAR 5: CRC32 (4), size (vint), type (vint), flags (vint), [extra size], [data size], then the
// type-specific fields, which for the main header start with the archive flags (vint).
fn inspect_rar5(h: &[u8]) -> Inspection {
    const MAIN: u64 = 1;
    const ENCRYPTION: u64 = 4;
    const HAS_EXTRA: u64 = 0x01;
    const HAS_DATA: u64 = 0x02;
    const VOLUME: u64 = 0x01;

    let mut pos = 4;
    let Some(_size) = vint(h, &mut pos) else {
        return NONE;
    };
    let Some(kind) = vint(h, &mut pos) else {
        return NONE;
    };
    if kind == ENCRYPTION {
        return Inspection {
            multi_volume: false,
            encrypted_headers: true,
        };
    }
    if kind != MAIN {
        return NONE;
    }
    let Some(flags) = vint(h, &mut pos) else {
        return NONE;
    };
    if flags & HAS_EXTRA != 0 && vint(h, &mut pos).is_none() {
        return NONE;
    }
    if flags & HAS_DATA != 0 && vint(h, &mut pos).is_none() {
        return NONE;
    }
    match vint(h, &mut pos) {
        Some(archive_flags) => Inspection {
            multi_volume: archive_flags & VOLUME != 0,
            encrypted_headers: false,
        },
        None => NONE,
    }
}

/// RAR 5 variable-length integer: 7 bits per byte, low group first, high bit means "more".
fn vint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let b = *buf.get(*pos)?;
        *pos += 1;
        value |= u64::from(b & 0x7F) << shift;
        if b & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rar5(tail: &[u8]) -> Vec<u8> {
        let mut v = SIGNATURE_RAR5.to_vec();
        v.extend_from_slice(tail);
        v
    }

    fn rar4(tail: &[u8]) -> Vec<u8> {
        let mut v = SIGNATURE_RAR4.to_vec();
        v.extend_from_slice(tail);
        v
    }

    #[test]
    fn rejects_other_data() {
        assert!(matches!(inspect(b"PK\x03\x04"), Err(RarError::NotRar)));
        assert!(matches!(inspect(&[]), Err(RarError::NotRar)));
    }

    #[test]
    fn signature_helper_matches_both_generations() {
        assert!(is_rar_signature(&rar4(&[])));
        assert!(is_rar_signature(&rar5(&[])));
        assert!(!is_rar_signature(b"Rar!\x1a\x07\x02"));
    }

    #[test]
    fn rar5_main_header_flags() {
        // CRC (ignored here), size, type 1, header flags 0, archive flags.
        let plain = rar5(&[0, 0, 0, 0, 3, 1, 0, 0x00]);
        let volume = rar5(&[0, 0, 0, 0, 3, 1, 0, 0x01]);
        assert_eq!(inspect(&plain).unwrap(), NONE);
        assert!(inspect(&volume).unwrap().multi_volume);
    }

    #[test]
    fn rar5_extra_and_data_fields_are_skipped_before_archive_flags() {
        // header flags 0x03: extra size and data size present, then archive flags = volume.
        let h = rar5(&[0, 0, 0, 0, 9, 1, 0x03, 0x05, 0x00, 0x01]);
        assert!(inspect(&h).unwrap().multi_volume);
    }

    #[test]
    fn rar5_encryption_header_means_encrypted_headers() {
        let h = rar5(&[0, 0, 0, 0, 5, 4, 0]);
        assert!(inspect(&h).unwrap().encrypted_headers);
    }

    #[test]
    fn rar4_main_header_flags() {
        let plain = rar4(&[0, 0, 0x73, 0x00, 0x00]);
        let volume = rar4(&[0, 0, 0x73, 0x01, 0x00]);
        let encrypted = rar4(&[0, 0, 0x73, 0x80, 0x00]);
        assert_eq!(inspect(&plain).unwrap(), NONE);
        assert!(inspect(&volume).unwrap().multi_volume);
        assert!(inspect(&encrypted).unwrap().encrypted_headers);
    }

    #[test]
    fn truncated_headers_defer_to_libarchive() {
        assert_eq!(inspect(&rar5(&[0, 0])).unwrap(), NONE);
        assert_eq!(inspect(&rar4(&[0])).unwrap(), NONE);
        assert_eq!(inspect(&SIGNATURE_RAR5).unwrap(), NONE);
    }

    #[test]
    fn vint_rejects_runaway_values() {
        let mut pos = 0;
        assert_eq!(vint(&[0x80; 12], &mut pos), None);
    }
}
