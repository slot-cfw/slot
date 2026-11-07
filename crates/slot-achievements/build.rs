//! Compiles the vendored rcheevos sources and slot.advance's small C shim into one static
//! library. Only the parts Phase 1 uses are compiled: the runtime (achievement parsing,
//! evaluation and progress), rc_libretro (memory maps) and what they need. The request
//! builders (`rapi`) and the full hashing set are added when a later phase uses them.

use std::path::PathBuf;

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let rc = root.join("vendor/rcheevos");
    let src = rc.join("src");

    let mut files: Vec<PathBuf> = [
        "rc_compat.c",
        "rc_util.c",
        "rc_version.c",
        "rc_libretro.c",
        "rhash/hash.c",
        "rhash/hash_rom.c",
        "rhash/md5.c",
        // Requests and responses (0.6.0 Phase 3): rcheevos builds and parses every call.
        "rapi/rc_api_common.c",
        "rapi/rc_api_runtime.c",
        "rapi/rc_api_user.c",
    ]
    .iter()
    .map(|f| src.join(f))
    .collect();
    for entry in std::fs::read_dir(src.join("rcheevos")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "c") {
            files.push(path);
        }
    }
    files.sort();
    files.push(root.join("src/shim.c"));
    files.push(root.join("src/shim_api.c"));

    let mut build = cc::Build::new();
    build
        .files(&files)
        .include(rc.join("include"))
        .include(&src)
        .include(root.join("vendor/libretro"))
        .std("gnu11")
        // Hashing is only needed for whole-ROM MD5 (GB, GBC, GBA); never discs, zips or
        // encrypted formats.
        .define("RC_HASH_NO_DISC", None)
        .define("RC_HASH_NO_ZIP", None)
        .define("RC_HASH_NO_ENCRYPTED", None)
        .define("RC_CLIENT_SUPPORTS_HASH", None)
        .define("_GNU_SOURCE", None)
        .flag_if_supported("-fno-strict-aliasing")
        // The vendored sources stay byte for byte as upstream ships them, so their own
        // warnings are not ours to fix.
        .warnings(false);
    build.compile("rcheevos");

    println!("cargo:rerun-if-changed=src/shim.c");
    println!("cargo:rerun-if-changed=src/shim_api.c");
    println!("cargo:rerun-if-changed=vendor");
    println!("cargo:rerun-if-changed=build.rs");
}
