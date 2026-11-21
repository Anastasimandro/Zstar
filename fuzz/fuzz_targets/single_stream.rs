#![no_main]

use libfuzzer_sys::fuzz_target;
use zstar_core::{Format, Options};

const SINGLES: [Format; 6] = [
    Format::Gz,
    Format::Zst,
    Format::Bz2,
    Format::Xz,
    Format::Lz4,
    Format::Br,
];

// The first byte picks the codec (or detection by name), the next up to 16 bytes the file
// name the output is called after, the rest is the stream. A real file name never holds a
// separator or NUL.
fuzz_target!(|data: &[u8]| {
    let Some((&selector, rest)) = data.split_first() else {
        return;
    };
    let (name, body) = rest.split_at(rest.len().min(16));
    let name: String = name
        .iter()
        .map(|&b| {
            if b == b'/' || b == 0 {
                '_'
            } else {
                char::from(b)
            }
        })
        .collect();
    if name.is_empty() || name == "." || name == ".." {
        return;
    }

    if usize::from(selector) % (SINGLES.len() + 1) < SINGLES.len() {
        zstar_fuzz::list_and_extract(zstar_fuzz::pick(selector, &SINGLES), body, Some(&name));
        return;
    }
    let sandbox = zstar_fuzz::Sandbox::new();
    let opts = Options {
        name_hint: Some(format!("{name}.br")),
        ..Options::default()
    };
    let _ = zstar_core::extract(&mut std::io::Cursor::new(body), &sandbox.dest(), &opts);
    sandbox.check();
});
