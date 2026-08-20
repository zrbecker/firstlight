//! Obtains the SVBONY SDK, generates bindings and links it.
//!
//! # Why this downloads things
//!
//! The SVBONY SDK carries no licence text of any kind — not in the header,
//! not in the archive, not on the vendor's site — so the default legal
//! position is all rights reserved and this repository does not redistribute
//! it. Making every developer hunt for it by hand instead would be worse, so
//! the build fetches it: pinned URLs, pinned SHA-256 for every file, cached
//! under the user's cache directory so it is downloaded once per machine.
//!
//! Nothing is fetched unless the `sdk` or `mock-sdk` feature is on.
//!
//! # Escape hatches
//!
//! * `FIRSTLIGHT_SVBONY_SDK_DIR` — use a local SDK instead of downloading.
//!   Must contain `SVBCameraSDK.h` and the platform library.
//! * `FIRSTLIGHT_SDK_CACHE` — where downloads are cached.
//! * `FIRSTLIGHT_OFFLINE=1` — never download; fail with instructions instead.

fn main() {
    for var in [
        "FIRSTLIGHT_SVBONY_SDK_DIR",
        "FIRSTLIGHT_SDK_CACHE",
        "FIRSTLIGHT_OFFLINE",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }

    let real = std::env::var_os("CARGO_FEATURE_SDK").is_some();
    let mock = std::env::var_os("CARGO_FEATURE_MOCK_SDK").is_some();
    if real && mock {
        panic!("features `sdk` and `mock-sdk` are mutually exclusive");
    }

    #[cfg(any(feature = "sdk", feature = "mock-sdk"))]
    if real || mock {
        sdk::build(real);
    }
}

#[cfg(any(feature = "sdk", feature = "mock-sdk"))]
mod sdk {
    use std::path::{Path, PathBuf};

    /// One file of the SDK, pinned by URL and content hash.
    struct Pinned {
        url: &'static str,
        sha256: &'static str,
        name: &'static str,
    }

    /// SDK version 1.13.4.
    ///
    /// The header and the macOS library come from INDIGO's vendored copy,
    /// which is the only public build of the macOS library that is universal
    /// (x86_64 + arm64) — the copy in indi-3rdparty is x86_64 only and will
    /// not link on Apple Silicon. Linux libraries come from indi-3rdparty.
    /// Both are pinned to a specific commit, and every download is checked
    /// against the hash below before it is used.
    const HEADER: Pinned = Pinned {
        url: "https://raw.githubusercontent.com/indigo-astronomy/indigo/462bc73a3571e1e420b18f263d7ed03fb293320b/indigo_drivers/ccd_svb/bin_externals/libsvbcamera/include/SVBCameraSDK.h",
        sha256: "c0dde3333efe5e0e5c42ca5d524e913ef037ae4d80225613b209ce040b7d65b1",
        name: "SVBCameraSDK.h",
    };

    fn library() -> Pinned {
        let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
        match (os.as_str(), arch.as_str()) {
            ("macos", _) => Pinned {
                url: "https://raw.githubusercontent.com/indigo-astronomy/indigo/462bc73a3571e1e420b18f263d7ed03fb293320b/indigo_drivers/ccd_svb/bin_externals/libsvbcamera/lib/macOS/libSVBCameraSDK.dylib",
                sha256: "924e394ef96b358f7235066bd191f56a81f927bcd0be4653e767ad61a4ef8ced",
                name: "libSVBCameraSDK.dylib",
            },
            ("linux", "x86_64") => Pinned {
                url: "https://raw.githubusercontent.com/indilib/indi-3rdparty/4a5c3a99232cd7f481f255fb1ff5d6af25cafa64/libsvbony/libSVBCameraSDK_amd64.bin",
                sha256: "371bcf7f515b4d273c461a41fd494e079637d74ee5f7a26238b022979d4613e4",
                name: "libSVBCameraSDK.so",
            },
            ("linux", "aarch64") => Pinned {
                url: "https://raw.githubusercontent.com/indilib/indi-3rdparty/4a5c3a99232cd7f481f255fb1ff5d6af25cafa64/libsvbony/libSVBCameraSDK_armv8.bin",
                sha256: "d8c6c1848d4cc95de6594449f43ee339693dfe52a8341bc857a3fe183d16e0e3",
                name: "libSVBCameraSDK.so",
            },
            (os, arch) => panic!(
                "no SVBONY SDK build is published for {os}/{arch}. Set \
                 FIRSTLIGHT_SVBONY_SDK_DIR to a directory containing one."
            ),
        }
    }

    pub fn build(link_library: bool) {
        let header = obtain(&HEADER);
        println!("cargo:rerun-if-changed={}", header.display());
        generate_bindings(&header);

        if link_library {
            let lib = obtain(&library());
            link(&lib);
        } else {
            #[cfg(feature = "mock-sdk")]
            build_mock(&header);
        }
    }

    /// A local SDK directory wins over anything downloaded.
    fn local_dir() -> Option<PathBuf> {
        let dir = PathBuf::from(std::env::var_os("FIRSTLIGHT_SVBONY_SDK_DIR")?);
        assert!(
            dir.is_dir(),
            "FIRSTLIGHT_SVBONY_SDK_DIR points at {}, which is not a directory",
            dir.display()
        );
        Some(dir)
    }

    /// Return a path to `file`, downloading and verifying it if necessary.
    fn obtain(file: &Pinned) -> PathBuf {
        if let Some(dir) = local_dir() {
            for candidate in [
                dir.join(file.name),
                dir.join("include").join(file.name),
                dir.join("lib").join(file.name),
            ] {
                if candidate.is_file() {
                    return candidate;
                }
            }
            panic!(
                "{} is not in FIRSTLIGHT_SVBONY_SDK_DIR ({})",
                file.name,
                dir.display()
            );
        }

        // Cache by hash, so a changed pin never collides with an old download.
        let cache = cache_dir().join(&file.sha256[..16]);
        let path = cache.join(file.name);
        if path.is_file() && hash_of(&path) == file.sha256 {
            return path;
        }
        if std::env::var_os("FIRSTLIGHT_OFFLINE").is_some() {
            panic!(
                "FIRSTLIGHT_OFFLINE is set and {} is not cached.\n\
                 Fetch it from {} or point FIRSTLIGHT_SVBONY_SDK_DIR at a local SDK.",
                file.name, file.url
            );
        }

        std::fs::create_dir_all(&cache)
            .unwrap_or_else(|e| panic!("creating {}: {e}", cache.display()));
        println!("cargo:warning=downloading {} (once per machine)", file.name);
        download(file.url, &path);

        let actual = hash_of(&path);
        assert_eq!(
            actual, file.sha256,
            "{} downloaded from {} has the wrong SHA-256.\n\
             Expected {}, got {}. Refusing to use it.",
            file.name, file.url, file.sha256, actual
        );
        path
    }

    fn cache_dir() -> PathBuf {
        if let Some(dir) = std::env::var_os("FIRSTLIGHT_SDK_CACHE") {
            return PathBuf::from(dir);
        }
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .expect("HOME is not set; set FIRSTLIGHT_SDK_CACHE instead");
        let base = if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
            home.join("Library/Caches")
        } else {
            std::env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".cache"))
        };
        base.join("firstlight").join("svbony-sdk")
    }

    fn download(url: &str, dest: &Path) {
        // curl ships with macOS and every Linux distribution this targets;
        // pulling an HTTP stack into the build graph for one file would cost
        // more than it saves.
        let status = std::process::Command::new("curl")
            .args([
                "--fail",
                "--location",
                "--silent",
                "--show-error",
                "--output",
            ])
            .arg(dest)
            .arg(url)
            .status();
        match status {
            Ok(status) if status.success() => {}
            Ok(status) => panic!("curl failed ({status}) downloading {url}"),
            Err(e) => panic!(
                "could not run curl ({e}). Install curl, or download {url} to {} yourself.",
                dest.display()
            ),
        }
    }

    fn hash_of(path: &Path) -> String {
        use sha2::{Digest, Sha256};
        let bytes = std::fs::read(path).unwrap_or_default();
        let digest = Sha256::digest(&bytes);
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn generate_bindings(header: &Path) {
        let bindings = bindgen::Builder::default()
            .header(header.to_string_lossy())
            .allowlist_function("SVB.*")
            .allowlist_type("SVB.*")
            .allowlist_var("SVB.*")
            .derive_debug(true)
            .derive_default(true)
            .layout_tests(false)
            .generate()
            .expect("generating bindings from SVBCameraSDK.h");
        let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
        bindings
            .write_to_file(out.join("svbony_bindings.rs"))
            .expect("writing svbony_bindings.rs");
    }

    fn link(lib: &Path) {
        // The cached file has the right contents but the linker wants a
        // conventionally named library in a directory of its own.
        let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
        let dir = out.join("svbony-lib");
        std::fs::create_dir_all(&dir).expect("creating the link directory");
        let staged = dir.join(lib.file_name().expect("library file name"));
        std::fs::copy(lib, &staged).expect("staging the SDK library");

        println!("cargo:rustc-link-search=native={}", dir.display());
        println!("cargo:rustc-link-lib=dylib=SVBCameraSDK");
        // So the built binary finds the library without DYLD_LIBRARY_PATH.
        let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        if target_os == "macos" {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", dir.display());
            println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path");
            println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../Frameworks");
        } else {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", dir.display());
            println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
        }
        // Tell dependent crates where the real library is, so an application
        // can copy it into its bundle.
        println!("cargo:svbony_library={}", staged.display());
    }

    /// The mock camera compiles against the *real* header, so the FFI layer
    /// is checked against the genuine declarations rather than a hand-written
    /// approximation of them.
    #[cfg(feature = "mock-sdk")]
    fn build_mock(header: &Path) {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mock");
        let source = dir.join("svb_mock.c");
        println!("cargo:rerun-if-changed={}", source.display());
        cc::Build::new()
            .file(&source)
            .include(header.parent().expect("header directory"))
            .include(&dir)
            .flag_if_supported("-pthread")
            .warnings(false)
            .compile("svb_mock");
    }
}
