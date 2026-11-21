#![no_main]

use libfuzzer_sys::fuzz_target;
use zstar_core::{Format, Options};

const TARS: [Format; 6] = [
    Format::Tar,
    Format::TarGz,
    Format::TarZst,
    Format::TarBz2,
    Format::TarXz,
    Format::TarLz4,
];

// The first byte picks the wrapper, so a mutated stream reaches each decoder even when its
// magic bytes are wrong; the extra value goes through detection, as the CLI does.
fuzz_target!(|data: &[u8]| {
    let Some((&selector, body)) = data.split_first() else {
        return;
    };
    if usize::from(selector) % (TARS.len() + 1) < TARS.len() {
        zstar_fuzz::list_and_extract(zstar_fuzz::pick(selector, &TARS), body, None);
        return;
    }
    let sandbox = zstar_fuzz::Sandbox::new();
    let opts = Options::default();
    let _ = zstar_core::list(&mut std::io::Cursor::new(body), &opts, &mut |_| {});
    let _ = zstar_core::extract(&mut std::io::Cursor::new(body), &sandbox.dest(), &opts);
    sandbox.check();
});
