#![no_main]

use std::io::{Cursor, Seek};

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut input = Cursor::new(data);
    if zstar_core::detect(&mut input).is_ok() {
        assert_eq!(input.stream_position().unwrap(), 0, "detect must rewind");
    }
    // Brotli has no signature: the name decides when the content matches nothing else.
    let mut input = Cursor::new(data);
    if zstar_core::detect_named(&mut input, Some("fuzz.br")).is_ok() {
        assert_eq!(
            input.stream_position().unwrap(),
            0,
            "detect_named must rewind"
        );
    }
});
