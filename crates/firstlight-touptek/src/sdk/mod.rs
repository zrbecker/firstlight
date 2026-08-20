//! The real backend: safe wrappers over the Touptek SDK, and enumeration.
//!
//! Compiled only with the `sdk` feature. Everything unsafe in this crate is
//! in [`sys`], and every one of those calls goes through
//! [`crate::status::check`] so no HRESULT is ever discarded.
// The `as u32`/`as u64` casts on the vendor's constants are deliberate and
// stay even where a given header makes them redundant: bindgen picks the type
// of a `#define` from its literal form, and that has differed between SDK
// releases (`u32` in some, `i32` in others). The casts make this code compile
// against either.
#![allow(clippy::unnecessary_cast)]

pub mod camera;
pub mod ffi;

use std::sync::Arc;

use firstlight_core::camera::{Backend, Camera, CameraId, CameraInfo};
use firstlight_core::error::{Error, Result};

use crate::BACKEND_NAME;

/// Discovery and opening for Touptek-compatible cameras.
///
/// The same SDK drives SVBONY (SV305C Pro and friends), Altair, Omegon,
/// RisingCam and Touptek's own models, so one backend covers all of them.
#[derive(Debug, Default)]
pub struct TouptekBackend;

impl TouptekBackend {
    pub fn new() -> TouptekBackend {
        TouptekBackend
    }

    /// SDK version string, useful in bug reports.
    pub fn version() -> String {
        sys::version()
    }
}

impl Backend for TouptekBackend {
    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn enumerate(&self) -> Result<Vec<CameraInfo>> {
        Ok(sys::enumerate())
    }

    fn open(&self, id: &CameraId) -> Result<Box<dyn Camera>> {
        // Enumerate first so a missing camera is reported as NotFound rather
        // than as a null handle with no explanation.
        let info = sys::enumerate()
            .into_iter()
            .find(|c| &c.id == id)
            .ok_or_else(|| Error::NotFound(id.to_string()))?;
        let mut camera = camera::TouptekCamera::new(info);
        camera.connect()?;
        Ok(Box::new(camera))
    }
}

/// Safe wrappers over the raw SDK entry points.
///
/// The handle is a raw pointer the SDK owns. It is `Send` because the SDK
/// permits calls from any thread; it is *not* `Sync`, and every use here goes
/// through a mutex so two threads never call into one handle at once.
pub mod sys {
    use std::ffi::c_void;
    use std::os::raw::c_int;

    use firstlight_core::camera::{CameraId, CameraInfo};
    use firstlight_core::control::{Binning, BitDepth};
    use firstlight_core::error::{Error, Result};
    use firstlight_core::frame::{BayerPattern, PixelFormat};

    use super::ffi;
    use crate::BACKEND_NAME;
    use crate::status;

    /// An open camera handle.
    pub struct Handle(ffi::HToupcam);

    // SAFETY: the SDK documents its handles as usable from any thread; the
    // only requirement is that calls on one handle are not concurrent, which
    // the `Mutex<Handle>` in `camera.rs` guarantees.
    unsafe impl Send for Handle {}

    impl Handle {
        /// A handle that owns nothing, for a camera that is not open yet.
        pub const fn null() -> Handle {
            Handle(std::ptr::null_mut())
        }

        pub fn raw(&self) -> ffi::HToupcam {
            self.0
        }

        pub fn is_null(&self) -> bool {
            self.0.is_null()
        }

        /// Open a camera by its enumeration id.
        pub fn open(id: &str) -> Result<Handle> {
            let wide = ffi::to_wide(id);
            // SAFETY: `wide` is a NUL-terminated wide string that outlives the
            // call; the SDK copies what it needs.
            let handle = unsafe { ffi::Toupcam_Open(wide.as_ptr()) };
            if handle.is_null() {
                // The SDK gives no code here, and the two realistic causes
                // are very different, so say both.
                return Err(Error::Busy(format!(
                    "Toupcam_Open({id}) returned null: the camera is already \
                     open in another application, was just unplugged, or the \
                     process lacks permission to claim it"
                )));
            }
            Ok(Handle(handle))
        }

        pub fn close(&mut self) {
            if !self.0.is_null() {
                // SAFETY: the handle came from `Toupcam_Open` and is closed
                // exactly once; the SDK guarantees no callback fires after
                // this returns.
                unsafe { ffi::Toupcam_Close(self.0) };
                self.0 = std::ptr::null_mut();
            }
        }

        pub fn stop(&self) -> Result<()> {
            status::check("Toupcam_Stop", unsafe { ffi::Toupcam_Stop(self.0) } as i32)
        }

        /// Start pull mode. `context` is handed back to the callback and must
        /// stay alive and pinned until [`Handle::close`] returns.
        ///
        /// # Safety
        /// `context` must point at a live `Shared` for as long as the stream
        /// is running.
        pub unsafe fn start_pull_mode(
            &self,
            callback: ffi::PTOUPCAM_EVENT_CALLBACK,
            context: *mut c_void,
        ) -> Result<()> {
            status::check("Toupcam_StartPullModeWithCallback", unsafe {
                ffi::Toupcam_StartPullModeWithCallback(self.0, callback, context)
            } as i32)
        }

        /// Pull the frame the SDK has signalled.
        ///
        /// Returns `Ok(None)` when there is nothing ready, which the SDK
        /// reports as `S_FALSE`/`E_PENDING` rather than as an error.
        ///
        /// # Safety
        /// `buffer` must be large enough for the current frame: the caller
        /// sizes it for the *full sensor*, so a geometry change that races
        /// with a pull cannot overflow it.
        pub unsafe fn pull_image(
            &self,
            buffer: &mut [u8],
            bits: i32,
        ) -> Result<Option<ffi::ToupcamFrameInfoV3>> {
            let mut info = ffi::ToupcamFrameInfoV3::default();
            let hr = unsafe {
                ffi::Toupcam_PullImageV3(
                    self.0,
                    buffer.as_mut_ptr() as *mut c_void,
                    0,    // video frame, not a still
                    bits, // 8 or 16 for raw data
                    -1,   // tightly packed rows, no padding
                    &mut info,
                )
            } as i32;
            if status::is_no_data(hr) {
                return Ok(None);
            }
            status::check("Toupcam_PullImageV3", hr)?;
            Ok(Some(info))
        }

        pub fn put_option(&self, option: u32, value: i32) -> Result<()> {
            status::check("Toupcam_put_Option", unsafe {
                ffi::Toupcam_put_Option(self.0, option, value)
            } as i32)
        }

        pub fn get_option(&self, option: u32) -> Result<i32> {
            let mut value: c_int = 0;
            status::check("Toupcam_get_Option", unsafe {
                ffi::Toupcam_get_Option(self.0, option, &mut value)
            } as i32)?;
            Ok(value)
        }

        pub fn put_exposure_us(&self, micros: u32) -> Result<()> {
            status::check("Toupcam_put_ExpoTime", unsafe {
                ffi::Toupcam_put_ExpoTime(self.0, micros)
            } as i32)
        }

        pub fn exposure_us(&self) -> Result<u32> {
            let mut value = 0u32;
            status::check("Toupcam_get_ExpoTime", unsafe {
                ffi::Toupcam_get_ExpoTime(self.0, &mut value)
            } as i32)?;
            Ok(value)
        }

        /// (min, max, default) in microseconds.
        pub fn exposure_range(&self) -> Result<(u32, u32, u32)> {
            let (mut min, mut max, mut def) = (0u32, 0u32, 0u32);
            status::check("Toupcam_get_ExpTimeRange", unsafe {
                ffi::Toupcam_get_ExpTimeRange(self.0, &mut min, &mut max, &mut def)
            } as i32)?;
            Ok((min, max, def))
        }

        pub fn put_gain(&self, percent: u16) -> Result<()> {
            status::check("Toupcam_put_ExpoAGain", unsafe {
                ffi::Toupcam_put_ExpoAGain(self.0, percent)
            } as i32)
        }

        pub fn gain(&self) -> Result<u16> {
            let mut value = 0u16;
            status::check("Toupcam_get_ExpoAGain", unsafe {
                ffi::Toupcam_get_ExpoAGain(self.0, &mut value)
            } as i32)?;
            Ok(value)
        }

        /// (min, max, default) as a percentage, where 100 is unity gain.
        pub fn gain_range(&self) -> Result<(u16, u16, u16)> {
            let (mut min, mut max, mut def) = (0u16, 0u16, 0u16);
            status::check("Toupcam_get_ExpoAGainRange", unsafe {
                ffi::Toupcam_get_ExpoAGainRange(self.0, &mut min, &mut max, &mut def)
            } as i32)?;
            Ok((min, max, def))
        }

        pub fn put_roi(&self, x: u32, y: u32, width: u32, height: u32) -> Result<()> {
            status::check("Toupcam_put_Roi", unsafe {
                ffi::Toupcam_put_Roi(self.0, x, y, width, height)
            } as i32)
        }

        pub fn roi(&self) -> Result<(u32, u32, u32, u32)> {
            let (mut x, mut y, mut w, mut h) = (0u32, 0u32, 0u32, 0u32);
            status::check("Toupcam_get_Roi", unsafe {
                ffi::Toupcam_get_Roi(self.0, &mut x, &mut y, &mut w, &mut h)
            } as i32)?;
            Ok((x, y, w, h))
        }

        /// Current output size, which binning and ROI both affect.
        pub fn size(&self) -> Result<(u32, u32)> {
            let (mut width, mut height) = (0i32, 0i32);
            status::check("Toupcam_get_Size", unsafe {
                ffi::Toupcam_get_Size(self.0, &mut width, &mut height)
            } as i32)?;
            Ok((width.max(0) as u32, height.max(0) as u32))
        }

        /// Raw pixel layout: the FourCC and the significant bit count.
        pub fn raw_format(&self) -> Result<(u32, u32)> {
            let (mut fourcc, mut bitdepth) = (0u32, 0u32);
            status::check("Toupcam_get_RawFormat", unsafe {
                ffi::Toupcam_get_RawFormat(self.0, &mut fourcc, &mut bitdepth)
            } as i32)?;
            Ok((fourcc, bitdepth))
        }

        /// Sensor temperature in degrees Celsius.
        pub fn temperature_c(&self) -> Result<f32> {
            let mut tenths = 0i16;
            status::check("Toupcam_get_Temperature", unsafe {
                ffi::Toupcam_get_Temperature(self.0, &mut tenths)
            } as i32)?;
            Ok(f32::from(tenths) / 10.0)
        }

        pub fn serial_number(&self) -> Result<String> {
            let mut buffer = [0i8; 32];
            status::check("Toupcam_get_SerialNumber", unsafe {
                ffi::Toupcam_get_SerialNumber(self.0, buffer.as_mut_ptr())
            } as i32)?;
            let bytes: Vec<u8> = buffer
                .iter()
                .map(|&c| c as u8)
                .take_while(|&c| c != 0)
                .collect();
            Ok(String::from_utf8_lossy(&bytes).trim().to_string())
        }

        pub fn put_white_balance_gain(&self, gains: [i32; 3]) -> Result<()> {
            let mut gains = gains;
            status::check("Toupcam_put_WhiteBalanceGain", unsafe {
                ffi::Toupcam_put_WhiteBalanceGain(self.0, gains.as_mut_ptr())
            } as i32)
        }

        pub fn white_balance_gain(&self) -> Result<[i32; 3]> {
            let mut gains = [0i32; 3];
            status::check("Toupcam_get_WhiteBalanceGain", unsafe {
                ffi::Toupcam_get_WhiteBalanceGain(self.0, gains.as_mut_ptr())
            } as i32)?;
            Ok(gains)
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            self.close();
        }
    }

    pub fn version() -> String {
        // SAFETY: the SDK returns a static wide string it owns.
        unsafe { ffi::from_wide_ptr(ffi::Toupcam_Version(), 64) }
    }

    /// Enumerate attached cameras. Never fails: the SDK reports "none".
    pub fn enumerate() -> Vec<CameraInfo> {
        let mut devices: Vec<ffi::ToupcamDeviceV2> =
            vec![ffi::ToupcamDeviceV2::default(); ffi::TOUPCAM_MAX as usize];
        // SAFETY: the SDK fills at most TOUPCAM_MAX entries into our buffer.
        let count = unsafe { ffi::Toupcam_EnumV2(devices.as_mut_ptr()) } as usize;
        devices
            .into_iter()
            .take(count.min(ffi::TOUPCAM_MAX as usize))
            .map(describe)
            .collect()
    }

    /// Turn an enumeration entry into a [`CameraInfo`].
    fn describe(device: ffi::ToupcamDeviceV2) -> CameraInfo {
        let id = ffi::from_wide(&device.id);
        let display_name = ffi::from_wide(&device.displayname);

        // SAFETY: the model pointer is owned by the SDK and static for the
        // lifetime of the process.
        let model = unsafe { device.model.as_ref() };
        let (model_name, flags, pixel_um, max_width, max_height) = match model {
            Some(model) => {
                let name = unsafe { ffi::from_wide_ptr(model.name, 64) };
                let (width, height) = model
                    .res
                    .iter()
                    .map(|r| (r.width, r.height))
                    .max_by_key(|(w, h)| u64::from(*w) * u64::from(*h))
                    .unwrap_or((0, 0));
                (
                    name,
                    model.flag as u64,
                    model.xpixsz,
                    width as u32,
                    height as u32,
                )
            }
            None => (display_name.clone(), 0, 0.0, 0, 0),
        };

        let mono = flags & (ffi::TOUPCAM_FLAG_MONO as u64) != 0;
        let mut bit_depths = vec![BitDepth::EIGHT];
        for (flag, depth) in [
            (ffi::TOUPCAM_FLAG_RAW10 as u64, BitDepth::TEN),
            (ffi::TOUPCAM_FLAG_RAW12 as u64, BitDepth::TWELVE),
            (ffi::TOUPCAM_FLAG_RAW14 as u64, BitDepth::FOURTEEN),
            (ffi::TOUPCAM_FLAG_RAW16 as u64, BitDepth::SIXTEEN),
        ] {
            if flags & flag != 0 {
                bit_depths.push(depth);
            }
        }

        CameraInfo {
            id: CameraId::new(id),
            display_name,
            model: model_name,
            // The real serial needs an open handle; `TouptekCamera::connect`
            // fills it in, and until then the enumeration id is the key.
            serial: String::new(),
            backend: BACKEND_NAME,
            max_width,
            max_height,
            pixel_size_um: pixel_um,
            // The exact Bayer phase only comes from `get_RawFormat` on an
            // open handle; assume the most common one until then.
            pixel_format: if mono {
                PixelFormat::Mono
            } else {
                PixelFormat::Bayer(BayerPattern::Grbg)
            },
            bit_depths,
            binnings: vec![Binning(1), Binning(2), Binning(3), Binning(4)],
            has_cooler: flags & (ffi::TOUPCAM_FLAG_TEC_ONOFF as u64) != 0,
        }
    }

    /// Map a raw-format FourCC onto a pixel layout.
    pub fn pixel_format_from_fourcc(fourcc: u32, mono: bool) -> PixelFormat {
        if mono {
            return PixelFormat::Mono;
        }
        match fourcc {
            ffi::FOURCC_RGGB => PixelFormat::Bayer(BayerPattern::Rggb),
            ffi::FOURCC_BGGR => PixelFormat::Bayer(BayerPattern::Bggr),
            ffi::FOURCC_GRBG => PixelFormat::Bayer(BayerPattern::Grbg),
            ffi::FOURCC_GBRG => PixelFormat::Bayer(BayerPattern::Gbrg),
            // Anything else means the camera is not in raw mode; that is a
            // configuration bug on our side, so say so rather than guessing.
            _ => PixelFormat::Mono,
        }
    }
}

/// Convenience for applications that only want the Touptek backend.
pub fn backend() -> Arc<TouptekBackend> {
    Arc::new(TouptekBackend::new())
}
