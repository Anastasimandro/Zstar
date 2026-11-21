#![no_main]

use std::path::{Component, Path};

use libfuzzer_sys::fuzz_target;

fuzz_target!(|name: &[u8]| {
    // Independent of the sanitiser: a name that must never be accepted.
    let split_has_parent = name.split(|&b| b == b'/' || b == b'\\').any(|c| c == b"..");
    let must_reject =
        name.contains(&0) || matches!(name.first(), Some(b'/' | b'\\')) || split_has_parent;

    for windows_rules in [false, true] {
        match zstar_core::sanitize_entry_name(name, windows_rules) {
            Err(_) => {}
            Ok(path) => {
                assert!(!must_reject, "accepted an unsafe name: {name:?}");
                assert!(
                    path.components().all(|c| matches!(c, Component::Normal(_))),
                    "not a plain relative path: {path:?}"
                );
                let root = Path::new("/zstar-dest");
                let joined = zstar_fuzz::lexical(&root.join(&path));
                assert!(
                    joined.starts_with(root),
                    "escapes the destination: {path:?}"
                );
            }
        }
    }
});
