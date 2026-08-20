//! Generates the FFI bindings and links a Touptek SDK.
//!
//! Three shapes of build:
//!
//! * no features — pure Rust, no SDK, builds anywhere. This is the default so
//!   `cargo test` works on a machine that has never seen a camera.
//! * `sdk` — bindgen against the vendor `toupcam.h`, linked against the
//!   vendor library. The real thing.
//! * `mock-sdk` — bindgen against `mock/toupcam.h` and a small C camera
//!   compiled from `mock/toupcam_mock.c`, so the FFI layer can be compiled
//!   and exercised in CI. It proves the Rust side hangs together; only a
//!   `sdk` build proves the signatures match the vendor's.

fn main() {
    println!("cargo:rerun-if-env-changed=FIRSTLIGHT_TOUPTEK_SDK_DIR");

    let real = std::env::var_os("CARGO_FEATURE_SDK").is_some();
    let mock = std::env::var_os("CARGO_FEATURE_MOCK_SDK").is_some();
    if real && mock {
        panic!(
            "features `sdk` and `mock-sdk` are mutually exclusive: pick the \
             vendor SDK or the mock, not both"
        );
    }

    #[cfg(feature = "sdk")]
    if real {
        vendor::build();
    }

    #[cfg(feature = "mock-sdk")]
    if mock {
        mock_sdk::build();
    }
}

#[cfg(feature = "mock-sdk")]
mod mock_sdk {
    use std::path::PathBuf;

    pub fn build() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mock");
        let header = dir.join("toupcam.h");
        let source = dir.join("toupcam_mock.c");
        println!("cargo:rerun-if-changed={}", header.display());
        println!("cargo:rerun-if-changed={}", source.display());

        crate::generate_bindings(&header);

        cc::Build::new()
            .file(&source)
            .include(&dir)
            .flag_if_supported("-pthread")
            .warnings(true)
            .compile("toupcam_mock");
    }
}

#[cfg(feature = "sdk")]
mod vendor {
    use std::path::{Path, PathBuf};

    pub fn build() {
        let sdk_dir = locate_sdk();
        let header = find_header(&sdk_dir);
        println!("cargo:rerun-if-changed={}", header.display());
        crate::generate_bindings(&header);
        link(&sdk_dir);
    }

    /// `FIRSTLIGHT_TOUPTEK_SDK_DIR`, or a `vendor/` copy inside the crate.
    fn locate_sdk() -> PathBuf {
        if let Some(dir) = std::env::var_os("FIRSTLIGHT_TOUPTEK_SDK_DIR") {
            let dir = PathBuf::from(dir);
            assert!(
                dir.is_dir(),
                "FIRSTLIGHT_TOUPTEK_SDK_DIR points at {}, which is not a directory.\n\
                 The Touptek SDK is a free download from \
                 https://www.touptek-astro.com/download/ (pick the macOS/Windows/Linux\n\
                 SDK, not the Windows application). Unpack it and point this variable at\n\
                 the directory containing inc/toupcam.h.\n\
                 Note that not every camera speaks this SDK: SVBONY's SV305 series uses\n\
                 SVBONY's own SDK instead. See crates/firstlight-touptek/vendor/README.md.",
                dir.display()
            );
            return dir;
        }
        let vendor = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor");
        assert!(
            vendor.join("inc").is_dir() || vendor.join("toupcam.h").is_file(),
            "the `sdk` feature needs the Touptek SDK.\n\
             Download it from https://www.touptek-astro.com/download/ and either\n\
             unpack it into {} or set FIRSTLIGHT_TOUPTEK_SDK_DIR to where it lives.\n\
             See crates/firstlight-touptek/vendor/README.md.",
            vendor.display()
        );
        vendor
    }

    fn find_header(sdk: &Path) -> PathBuf {
        // The vendor archive puts it in `inc/`; a hand-assembled directory
        // often has it at the top level.
        for candidate in ["inc/toupcam.h", "include/toupcam.h", "toupcam.h"] {
            let path = sdk.join(candidate);
            if path.is_file() {
                return path;
            }
        }
        panic!(
            "no toupcam.h under {} (looked in inc/, include/ and the root)",
            sdk.display()
        );
    }

    /// Point the linker at the right per-platform library directory.
    fn link(sdk: &Path) {
        let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

        // The vendor archive layout, plus flat fallbacks.
        let candidates: Vec<PathBuf> = match target_os.as_str() {
            "macos" => vec![sdk.join("mac"), sdk.join("macos"), sdk.to_path_buf()],
            "linux" => {
                let arch = match target_arch.as_str() {
                    "x86_64" => "x64",
                    "x86" => "x86",
                    "aarch64" => "arm64",
                    "arm" => "armhf",
                    other => other,
                };
                vec![
                    sdk.join("linux").join(arch),
                    sdk.join("linux"),
                    sdk.to_path_buf(),
                ]
            }
            "windows" => {
                let arch = if target_arch == "x86_64" {
                    "x64"
                } else {
                    "win32"
                };
                vec![
                    sdk.join("win").join(arch),
                    sdk.join("win"),
                    sdk.to_path_buf(),
                ]
            }
            other => panic!("no Touptek SDK layout known for target OS {other}"),
        };

        let lib_dir = candidates
            .iter()
            .find(|dir| has_library(dir, &target_os))
            .unwrap_or_else(|| {
                panic!(
                    "no toupcam library found; looked in {}",
                    candidates
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            });

        println!("cargo:rustc-link-search=native={}", lib_dir.display());
        println!("cargo:rustc-link-lib=dylib=toupcam");
        // The rpath has to be recorded in the binary, and a library build
        // script cannot do that: `cargo:rustc-link-arg` applies only to the
        // package it comes from. Publish the directory (`links = "toupcam"`
        // exposes it as DEP_TOUPCAM_LIB_DIR) and let the binaries do it.
        println!("cargo:lib_dir={}", lib_dir.display());
    }

    fn has_library(dir: &Path, target_os: &str) -> bool {
        let names: &[&str] = match target_os {
            "macos" => &["libtoupcam.dylib"],
            "linux" => &["libtoupcam.so"],
            "windows" => &["toupcam.lib"],
            _ => &[],
        };
        names.iter().any(|name| dir.join(name).is_file())
    }
}

/// Generate `toupcam_bindings.rs` from a header.
#[cfg(any(feature = "sdk", feature = "mock-sdk"))]
fn generate_bindings(header: &std::path::Path) {
    let bindings = bindgen::Builder::default()
        .header(header.to_string_lossy())
        // Only the vendor surface: pulling in the whole platform SDK would
        // make this take minutes and break on every OS update.
        .allowlist_function("Toupcam_.*")
        .allowlist_type("Toupcam.*")
        .allowlist_type("HToupcam")
        .allowlist_var("TOUPCAM_.*")
        .derive_debug(true)
        .derive_default(true)
        .layout_tests(false)
        .generate()
        .expect("generating bindings from toupcam.h");

    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    bindings
        .write_to_file(out.join("toupcam_bindings.rs"))
        .expect("writing toupcam_bindings.rs");
}
