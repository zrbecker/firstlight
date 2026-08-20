//! The real backend: safe wrappers over the SVBONY SDK, and enumeration.
//!
//! Every unsafe call in this crate is in [`sys`], and every one of them goes
//! through [`crate::status::check`], so no return code is ever discarded.

pub mod camera;
pub mod ffi;

use firstlight_core::camera::{Backend, Camera, CameraId, CameraInfo};
use firstlight_core::error::{Error, Result};

use crate::BACKEND_NAME;

/// Discovery and opening for SVBONY's native cameras (the SV305 series and
/// relatives).
///
/// Cameras SVBONY builds on Touptek hardware do not appear here; they belong
/// to the Touptek backend. If a camera is missing from one, try the other.
#[derive(Debug, Default)]
pub struct SvbonyBackend;

impl SvbonyBackend {
    pub fn new() -> SvbonyBackend {
        SvbonyBackend
    }

    /// SDK version string, worth putting in a bug report.
    pub fn version() -> String {
        sys::version()
    }
}

impl Backend for SvbonyBackend {
    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn enumerate(&self) -> Result<Vec<CameraInfo>> {
        Ok(sys::enumerate())
    }

    fn open(&self, id: &CameraId) -> Result<Box<dyn Camera>> {
        let info = sys::enumerate()
            .into_iter()
            .find(|c| &c.id == id)
            .ok_or_else(|| Error::NotFound(id.to_string()))?;
        let handle = sys::enumerate_handles()
            .into_iter()
            .find(|h| h.camera_id_matches(id))
            .ok_or_else(|| Error::NotFound(id.to_string()))?;
        let mut camera = camera::SvbonyCamera::new(info, handle.device_id);
        camera.connect()?;
        Ok(Box::new(camera))
    }
}

/// Safe wrappers over the raw SDK entry points.
pub mod sys {
    use std::os::raw::{c_int, c_long};

    use firstlight_core::camera::{CameraId, CameraInfo};
    use firstlight_core::control::{Binning, BitDepth};
    use firstlight_core::error::Result;
    use firstlight_core::frame::{BayerPattern, PixelFormat};

    use super::ffi;
    use crate::BACKEND_NAME;
    use crate::status;

    /// What enumeration knows before anything is opened.
    pub struct Handle {
        pub device_id: i32,
        pub serial: String,
        pub name: String,
    }

    impl Handle {
        pub fn camera_id_matches(&self, id: &CameraId) -> bool {
            id.as_str() == camera_id_for(&self.serial, self.device_id).as_str()
        }
    }

    /// A stable id: the serial number when the camera reports one, because
    /// the SDK's numeric id is a slot that moves when devices come and go.
    pub fn camera_id_for(serial: &str, device_id: i32) -> CameraId {
        if serial.is_empty() {
            CameraId::new(format!("svbony-{device_id}"))
        } else {
            CameraId::new(serial.to_string())
        }
    }

    pub fn version() -> String {
        // SAFETY: the SDK returns a pointer to a static string it owns.
        let ptr = unsafe { ffi::SVBGetSDKVersion() };
        if ptr.is_null() {
            return String::new();
        }
        unsafe { std::ffi::CStr::from_ptr(ptr) }
            .to_string_lossy()
            .to_string()
    }

    /// Enumeration handles, carrying the numeric id the SDK wants.
    pub fn enumerate_handles() -> Vec<Handle> {
        // SAFETY: no arguments, returns a count.
        let count = unsafe { ffi::SVBGetNumOfConnectedCameras() };
        let mut out = Vec::new();
        for index in 0..count {
            let mut info = ffi::SVB_CAMERA_INFO::default();
            // SAFETY: `info` is a valid, correctly sized output struct.
            let rc = unsafe { ffi::SVBGetCameraInfo(&mut info, index) } as i32;
            if rc != status::SVB_SUCCESS {
                tracing::warn!(index, rc, "SVBGetCameraInfo failed during enumeration");
                continue;
            }
            out.push(Handle {
                device_id: info.CameraID,
                serial: ffi::c_string(&info.CameraSN),
                name: ffi::c_string(&info.FriendlyName),
            });
        }
        out
    }

    /// Enumerate cameras.
    ///
    /// Sensor geometry needs an open handle, so it is left at zero here and
    /// filled in by `connect`. Opening a camera just to describe it would
    /// steal it from whatever else has it open.
    pub fn enumerate() -> Vec<CameraInfo> {
        enumerate_handles()
            .into_iter()
            .map(|handle| CameraInfo {
                id: camera_id_for(&handle.serial, handle.device_id),
                display_name: handle.name.clone(),
                model: handle.name,
                serial: handle.serial,
                backend: BACKEND_NAME,
                max_width: 0,
                max_height: 0,
                pixel_size_um: 0.0,
                pixel_format: PixelFormat::Mono,
                bit_depths: Vec::new(),
                binnings: vec![Binning::ONE],
                has_cooler: false,
            })
            .collect()
    }

    /// When a camera was last closed in this process.
    ///
    /// Measured on an SV305C Pro: a camera opened within about a second of
    /// being closed comes back as a handle that accepts every call and then
    /// never delivers a frame. The SDK gives no way to ask whether the device
    /// is ready, so the only defence is to wait — and only when reopening
    /// quickly, which is why the time is recorded rather than the delay being
    /// paid on every open.
    static LAST_CLOSE: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

    /// How long a camera wants to itself after being closed.
    pub const REOPEN_QUIET: std::time::Duration = std::time::Duration::from_millis(1200);

    fn note_close() {
        *LAST_CLOSE.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
    }

    /// Sleep out whatever is left of the quiet period after the last close.
    pub fn wait_until_reopenable() {
        let since = LAST_CLOSE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map(|at| at.elapsed());
        if let Some(since) = since
            && since < REOPEN_QUIET
        {
            std::thread::sleep(REOPEN_QUIET - since);
        }
    }

    /// An open camera. Closed on drop, so an early return cannot leak the
    /// device and leave it unopenable until the process exits.
    pub struct Device {
        id: i32,
        open: bool,
    }

    impl Device {
        pub fn new(id: i32) -> Device {
            Device { id, open: false }
        }

        pub fn id(&self) -> i32 {
            self.id
        }

        /// Point this handle at a different numeric id.
        ///
        /// The SDK's camera id is a slot, not an identity: it goes stale when
        /// the camera is closed, and opening a stale one fails with "no
        /// camera with that id" no matter how long you wait. Re-enumerating
        /// is what recovers it.
        pub fn set_id(&mut self, id: i32) {
            debug_assert!(!self.open, "cannot renumber an open camera");
            self.id = id;
        }

        pub fn is_open(&self) -> bool {
            self.open
        }

        pub fn open(&mut self) -> Result<()> {
            if self.open {
                return Ok(());
            }
            // SAFETY: `id` came from enumeration.
            let code = unsafe { ffi::SVBOpenCamera(self.id) } as i32;
            if code != status::SVB_SUCCESS {
                return Err(status::to_error("SVBOpenCamera", code));
            }
            self.open = true;
            Ok(())
        }

        pub fn close(&mut self) {
            if self.open {
                // SAFETY: the camera was opened by us and is closed once.
                unsafe { ffi::SVBCloseCamera(self.id) };
                self.open = false;
                note_close();
            }
        }

        pub fn property(&self) -> Result<ffi::SVB_CAMERA_PROPERTY> {
            let mut property = ffi::SVB_CAMERA_PROPERTY::default();
            status::check("SVBGetCameraProperty", unsafe {
                ffi::SVBGetCameraProperty(self.id, &mut property)
            } as i32)?;
            Ok(property)
        }

        pub fn pixel_size_um(&self) -> Result<f32> {
            let mut size = 0.0f32;
            status::check("SVBGetSensorPixelSize", unsafe {
                ffi::SVBGetSensorPixelSize(self.id, &mut size)
            } as i32)?;
            Ok(size)
        }

        pub fn control_count(&self) -> Result<i32> {
            let mut count: c_int = 0;
            status::check("SVBGetNumOfControls", unsafe {
                ffi::SVBGetNumOfControls(self.id, &mut count)
            } as i32)?;
            Ok(count)
        }

        pub fn control_caps(&self, index: i32) -> Result<ffi::SVB_CONTROL_CAPS> {
            let mut caps = ffi::SVB_CONTROL_CAPS::default();
            status::check("SVBGetControlCaps", unsafe {
                ffi::SVBGetControlCaps(self.id, index, &mut caps)
            } as i32)?;
            Ok(caps)
        }

        pub fn control(&self, control_type: i32) -> Result<i64> {
            let mut value: c_long = 0;
            let mut auto: c_int = 0;
            status::check("SVBGetControlValue", unsafe {
                ffi::SVBGetControlValue(self.id, control_type, &mut value, &mut auto)
            } as i32)?;
            Ok(value as i64)
        }

        pub fn set_control(&self, control_type: i32, value: i64, auto: bool) -> Result<()> {
            status::check("SVBSetControlValue", unsafe {
                ffi::SVBSetControlValue(self.id, control_type, value as c_long, c_int::from(auto))
            } as i32)
        }

        pub fn control_auto(&self, control_type: i32) -> Result<bool> {
            let mut value: c_long = 0;
            let mut auto: c_int = 0;
            status::check("SVBGetControlValue", unsafe {
                ffi::SVBGetControlValue(self.id, control_type, &mut value, &mut auto)
            } as i32)?;
            Ok(auto != 0)
        }

        pub fn set_roi(&self, x: u32, y: u32, width: u32, height: u32, bin: u32) -> Result<()> {
            status::check("SVBSetROIFormat", unsafe {
                ffi::SVBSetROIFormat(
                    self.id,
                    x as c_int,
                    y as c_int,
                    width as c_int,
                    height as c_int,
                    bin as c_int,
                )
            } as i32)
        }

        pub fn roi(&self) -> Result<(u32, u32, u32, u32, u32)> {
            let (mut x, mut y, mut w, mut h, mut bin) = (0, 0, 0, 0, 0);
            status::check("SVBGetROIFormat", unsafe {
                ffi::SVBGetROIFormat(self.id, &mut x, &mut y, &mut w, &mut h, &mut bin)
            } as i32)?;
            Ok((
                x.max(0) as u32,
                y.max(0) as u32,
                w.max(0) as u32,
                h.max(0) as u32,
                bin.max(1) as u32,
            ))
        }

        pub fn set_image_type(&self, image_type: i32) -> Result<()> {
            status::check("SVBSetOutputImageType", unsafe {
                ffi::SVBSetOutputImageType(self.id, image_type)
            } as i32)
        }

        pub fn image_type(&self) -> Result<i32> {
            let mut image_type = 0i32;
            status::check("SVBGetOutputImageType", unsafe {
                ffi::SVBGetOutputImageType(self.id, &mut image_type)
            } as i32)?;
            Ok(image_type)
        }

        /// Free-running video, as opposed to one of the trigger modes.
        pub fn set_normal_mode(&self) -> Result<()> {
            status::check("SVBSetCameraMode", unsafe {
                ffi::SVBSetCameraMode(self.id, ffi::SVB_CAMERA_MODE_SVB_MODE_NORMAL)
            } as i32)
        }

        pub fn start_video(&self) -> Result<()> {
            status::check(
                "SVBStartVideoCapture",
                unsafe { ffi::SVBStartVideoCapture(self.id) } as i32,
            )
        }

        pub fn stop_video(&self) -> Result<()> {
            status::check(
                "SVBStopVideoCapture",
                unsafe { ffi::SVBStopVideoCapture(self.id) } as i32,
            )
        }

        /// Wait up to `wait_ms` for a frame.
        ///
        /// `Ok(false)` means the wait expired with nothing ready, which is
        /// ordinary during a long exposure and must not be treated as a
        /// failure.
        ///
        /// # Safety of the buffer
        /// The caller sizes `buffer` for the full sensor at the widest pixel
        /// format, so a geometry change racing with a read cannot overflow it.
        pub fn read_frame(&self, buffer: &mut [u8], wait_ms: i32) -> Result<bool> {
            // SAFETY: `buffer` is valid for `len` bytes and the SDK is told
            // exactly how many it may write.
            let rc = unsafe {
                ffi::SVBGetVideoData(
                    self.id,
                    buffer.as_mut_ptr(),
                    buffer.len() as c_long,
                    wait_ms as c_int,
                )
            } as i32;
            if status::is_timeout(rc) {
                return Ok(false);
            }
            status::check("SVBGetVideoData", rc)?;
            Ok(true)
        }

        /// Frames the camera or the driver threw away, cumulative.
        pub fn dropped_frames(&self) -> Result<u64> {
            let mut dropped: c_int = 0;
            status::check("SVBGetDroppedFrames", unsafe {
                ffi::SVBGetDroppedFrames(self.id, &mut dropped)
            } as i32)?;
            Ok(dropped.max(0) as u64)
        }

        /// Turn the SDK's parameter auto-save on or off.
        ///
        /// With it on — which is the default — the SDK writes a
        /// `<model>-AST_Cfg_*.bin` file into the process's *current working
        /// directory* and reloads it the next time a camera is opened there.
        /// That makes the camera's settings depend on where the application
        /// was launched from, and litters that directory.
        pub fn set_auto_save(&self, enable: bool) -> Result<()> {
            status::check("SVBSetAutoSaveParam", unsafe {
                ffi::SVBSetAutoSaveParam(self.id, i32::from(enable))
            } as i32)
        }

        /// Ask the camera to measure a white balance and store the result in
        /// its own gains.
        pub fn white_balance_once(&self) -> Result<()> {
            status::check(
                "SVBWhiteBalanceOnce",
                unsafe { ffi::SVBWhiteBalanceOnce(self.id) } as i32,
            )
        }

        pub fn serial_number(&self) -> Result<String> {
            let mut sn = ffi::SVB_SN::default();
            status::check(
                "SVBGetSerialNumber",
                unsafe { ffi::SVBGetSerialNumber(self.id, &mut sn) } as i32,
            )?;
            let bytes: Vec<u8> = sn.id.iter().copied().take_while(|&c| c != 0).collect();
            Ok(String::from_utf8_lossy(&bytes).trim().to_string())
        }
    }

    impl Drop for Device {
        fn drop(&mut self) {
            self.close();
        }
    }

    /// SVBONY's Bayer numbering, in terms of the top-left 2x2 cell.
    pub fn bayer_from_sdk(pattern: u32) -> BayerPattern {
        match pattern {
            ffi::SVB_BAYER_PATTERN_SVB_BAYER_RG => BayerPattern::Rggb,
            ffi::SVB_BAYER_PATTERN_SVB_BAYER_BG => BayerPattern::Bggr,
            ffi::SVB_BAYER_PATTERN_SVB_BAYER_GB => BayerPattern::Gbrg,
            // SVB_BAYER_GR, and anything unexpected: GRBG is what the SV305
            // series reports and the safest default.
            _ => BayerPattern::Grbg,
        }
    }

    /// The image type to request for a given depth and sensor colour.
    pub fn image_type_for(depth: BitDepth, colour: bool) -> i32 {
        match (depth.bytes_per_sample(), colour) {
            (1, true) => ffi::SVB_IMG_TYPE_SVB_IMG_RAW8,
            (1, false) => ffi::SVB_IMG_TYPE_SVB_IMG_Y8,
            (_, true) => ffi::SVB_IMG_TYPE_SVB_IMG_RAW16,
            (_, false) => ffi::SVB_IMG_TYPE_SVB_IMG_Y16,
        }
    }

    /// Bytes per sample for an image type the SDK reports.
    pub fn bytes_per_sample(image_type: i32) -> usize {
        if image_type == ffi::SVB_IMG_TYPE_SVB_IMG_RAW8
            || image_type == ffi::SVB_IMG_TYPE_SVB_IMG_Y8
        {
            1
        } else {
            2
        }
    }
}
