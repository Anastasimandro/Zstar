#![no_main]

use libfuzzer_sys::fuzz_target;
use zstar_core::Format;

// RAR and RAR5 through libarchive (C code behind `zstar-rar`): the one target where a bug can be
// memory-unsafe, so ASan matters here. Limits are the fuzz defaults: 10 s and 512 MiB.
fuzz_target!(|data: &[u8]| {
    zstar_fuzz::list_and_extract(Format::Rar, data, None);
});
