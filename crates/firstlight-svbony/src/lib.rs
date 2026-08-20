//! # firstlight-svbony
//!
//! [`firstlight_core::Camera`] over SVBONY's own camera SDK, which drives the
//! SV305 series and SVBONY's other native cameras.
//!
//! This is a different SDK from the one [`firstlight_touptek`] uses. SVBONY
//! sells both kinds: some of their models are built on Touptek hardware and
//! speak that SDK, while the SV305 series enumerates under SVBONY's own USB
//! vendor id and is invisible to Touptek's library however healthy it is. If
//! a camera does not appear in one backend, try the other.
//!
//! [`firstlight_touptek`]: https://docs.rs/firstlight-touptek
//!
//! ## Building
//!
//! ```sh
//! cargo build -p firstlight-svbony --features sdk
//! ```
//!
//! The vendor SDK is downloaded and hash-checked by the build script, once
//! per machine — there is nothing to install by hand. See `build.rs` for the
//! pinned sources and for how to point the build at a local copy instead.
//!
//! Without the feature the crate still compiles and still exposes
//! [`SvbonyBackend`]; it simply reports that it cannot see anything.
//!
//! ## How the frame path works
//!
//! Unlike Touptek's callback model, this SDK is a straight poll:
//! `SVBGetVideoData(id, buffer, size, wait_ms)` blocks for up to `wait_ms`
//! and hands back one frame. A pump thread does that in a loop and files
//! frames into a bounded, drop-oldest ring, so `next_frame` never waits on
//! the SDK and a wedged camera cannot outlast its caller's deadline.

pub mod controls;
pub mod status;

/// Name this backend reports in [`firstlight_core::CameraInfo::backend`].
pub const BACKEND_NAME: &str = "svbony";

#[cfg(any(feature = "sdk", feature = "mock-sdk"))]
pub mod sdk;

#[cfg(any(feature = "sdk", feature = "mock-sdk"))]
pub use sdk::SvbonyBackend;

#[cfg(not(any(feature = "sdk", feature = "mock-sdk")))]
mod unavailable;

#[cfg(not(any(feature = "sdk", feature = "mock-sdk")))]
pub use unavailable::SvbonyBackend;

/// Test hooks for the mock camera. Present only in a `mock-sdk` build.
#[cfg(feature = "mock-sdk")]
pub mod mock;

/// Whether this build can talk to real hardware.
pub const fn sdk_available() -> bool {
    cfg!(feature = "sdk")
}
