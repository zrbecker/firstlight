//! Records the runtime search paths for the vendor SDKs.
//!
//! The SDK libraries live in a cache directory or a vendor drop, not on the
//! system library path, so the binary needs an rpath pointing at them. Only
//! the build script of the package that *produces* the binary can add link
//! arguments, which is why this exists rather than living in the backend
//! crates: they publish their directory through Cargo's `links` metadata and
//! this turns it into an rpath.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    for dir in [
        std::env::var("DEP_SVBCAMERASDK_LIB_DIR").ok(),
        std::env::var("DEP_TOUPCAM_LIB_DIR").ok(),
    ]
    .into_iter()
    .flatten()
    {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
    }

    // Also look beside the executable, so a released build works with the
    // libraries copied next to it or into a macOS bundle.
    match target_os.as_str() {
        "macos" => {
            println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path");
            println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../Frameworks");
        }
        "linux" => println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN"),
        _ => {}
    }
}
