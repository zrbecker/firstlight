//! # firstlight-touptek
//!
//! [`firstlight_core::Camera`] over the Touptek SDK, which also drives the
//! cameras rebadged from Touptek hardware: Altair, Omegon, RisingCam,
//! Bresser, Meade and others, each shipping the same library under its own
//! symbol prefix.
//!
//! It does **not** cover every camera sold by those brands. SVBONY in
//! particular ships both kinds: some models are Touptek-based (the SDK calls
//! them `svbonycam`), while the SV305 series uses SVBONY's own, unrelated
//! SDK and is invisible to this backend.
//!
//! ## Building
//!
//! The vendor SDK is not redistributable, so the FFI is behind the `sdk`
//! feature and off by default. Without it this crate still compiles and still
//! exposes [`TouptekBackend`]; enumeration simply reports that no SDK is
//! present, which keeps `cargo test` working on a machine with no camera.
//!
//! ```sh
//! # with a real camera:
//! FIRSTLIGHT_TOUPTEK_SDK_DIR=~/sdk/toupcam cargo build -p firstlight-touptek --features sdk
//! ```
//!
//! See `vendor/README.md` for where to put the SDK.
//!
//! ## How the frame path works
//!
//! Pull mode with the vendor event callback. The callback runs on the SDK's
//! own thread and does one thing — push an event code into a channel. A pump
//! thread pulls the image and files it into a bounded, drop-oldest ring.
//! Nothing in the caller's path ever blocks inside the SDK. See
//! [`sdk::camera`] for the details.
//!
//! ## Error handling
//!
//! Every SDK call is checked and its `HRESULT` mapped in [`status`]. A
//! stalled USB pipe, a camera another process already holds and a value the
//! sensor rejected are three different errors here, because the recovery for
//! each is different.

pub mod events;
pub mod status;
pub mod tuning;

/// Name this backend reports in [`firstlight_core::CameraInfo::backend`].
pub const BACKEND_NAME: &str = "touptek";

#[cfg(any(feature = "sdk", feature = "mock-sdk"))]
pub mod sdk;

#[cfg(any(feature = "sdk", feature = "mock-sdk"))]
pub use sdk::TouptekBackend;

#[cfg(not(any(feature = "sdk", feature = "mock-sdk")))]
mod unavailable;

#[cfg(not(any(feature = "sdk", feature = "mock-sdk")))]
pub use unavailable::TouptekBackend;

/// Test hooks for the mock camera. Present only in a `mock-sdk` build.
#[cfg(feature = "mock-sdk")]
pub mod mock;

/// Whether this build can talk to real hardware.
///
/// False for a `mock-sdk` build: the mock proves the code compiles and the
/// frame path works, not that a camera is present.
pub const fn sdk_available() -> bool {
    cfg!(feature = "sdk")
}
