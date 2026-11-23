use std::env;

const VENDOR: &str = "vendor/libarchive";

// Hand-listed: the RAR and RAR5 readers and the minimum they link against. No writer, no other
// format or filter, no external library. The linker closure was checked with a driver that uses
// every function bound in src/lib.rs.
const SOURCES: &[&str] = &[
    "archive_read",
    "archive_read_support_format_rar",
    "archive_read_support_format_rar5",
    "archive_ppmd7",
    "archive_entry",
    "archive_entry_sparse",
    "archive_entry_xattr",
    "archive_acl",
    "archive_string",
    "archive_string_sprintf",
    "archive_util",
    "archive_check_magic",
    "archive_virtual",
    "archive_time",
    "archive_blake2s_ref",
    "archive_blake2sp_ref",
    "archive_crc32",
];

fn main() {
    let windows = env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows");

    let mut build = cc::Build::new();
    build
        .include("csrc")
        .include(VENDOR)
        .define("HAVE_CONFIG_H", "1")
        .define("__LIBARCHIVE_BUILD", "1")
        .define("LIBARCHIVE_STATIC", "1")
        .warnings(false)
        .extra_warnings(false);

    for name in SOURCES {
        build.file(format!("{VENDOR}/{name}.c"));
    }
    build.file("csrc/shim.c");

    if windows {
        // archive_util.c and archive_string.c call into the Windows CRT wrappers.
        build
            .file(format!("{VENDOR}/archive_windows.c"))
            .define("_CRT_SECURE_NO_DEPRECATE", "1")
            .define("_CRT_NONSTDC_NO_DEPRECATE", "1")
            .define("WIN32_LEAN_AND_MEAN", "1");
    }

    build.compile("zstar_libarchive");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=csrc");
    println!("cargo:rerun-if-changed={VENDOR}");
}
