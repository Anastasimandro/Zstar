#![no_main]

use libfuzzer_sys::fuzz_target;
use zstar_core::Format;

fuzz_target!(|data: &[u8]| {
    zstar_fuzz::list_and_extract(Format::SevenZ, data, None);
});
