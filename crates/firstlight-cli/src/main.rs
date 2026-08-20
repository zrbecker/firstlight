//! FirstLight command line harness.
//!
//! Deliberately thin: it drives [`firstlight_core::Camera`] directly rather
//! than going through the GUI's worker thread, so what you see here is what
//! the library contract actually is. Every wait has a timeout, every failure
//! prints the reason and sets a non-zero exit status, and an interrupted
//! recording is finalised rather than truncated.

mod args;
mod report;

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use clap::Parser;
use firstlight_core::camera::{Camera, CameraInfo};
use firstlight_core::control::ControlId;
use firstlight_core::error::{Error, Result};
use firstlight_core::format::fits::FitsMetadata;
use firstlight_core::format::ser::{SerMetadata, SerWriter};
use firstlight_core::registry::Registry;
use firstlight_core::write_fits;

use args::{Cli, Command, Select, Settings};

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(&cli.log);

    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            if e.is_fatal() {
                eprintln!("hint: the camera handle is dead. Unplug and replug it, then try again.");
            }
            ExitCode::FAILURE
        }
    }
}

fn init_logging(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("FIRSTLIGHT_LOG")
        .or_else(|_| EnvFilter::try_new(level))
        .unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::List { verbose } => list(verbose),
        Command::Info { select } => info(&select),
        Command::Capture {
            select,
            settings,
            output,
            frames,
            duration,
            observer,
            telescope,
        } => capture(
            &select,
            &settings,
            CaptureOptions {
                output,
                frames,
                duration,
                observer,
                telescope,
            },
        ),
        Command::Snap {
            select,
            settings,
            output,
            count,
            object,
        } => snap(&select, &settings, output, count, &object),
        Command::Watch {
            select,
            settings,
            duration,
        } => watch(&select, &settings, duration),
    }
}

/// Every backend this binary was built with.
fn registry(select: &Select) -> Registry {
    let mut registry = Registry::new();
    #[cfg(feature = "simulator")]
    registry.push(Arc::new(firstlight_core::simulator::SimulatorBackend::new()));
    registry.push(Arc::new(firstlight_svbony::SvbonyBackend::new()));
    registry.push(Arc::new(firstlight_touptek::TouptekBackend::new()));

    match &select.backend {
        Some(name) => {
            let mut filtered = Registry::new();
            if let Some(backend) = registry.backend(name) {
                filtered.push(backend);
            }
            filtered
        }
        None => registry,
    }
}

/// Resolve `--camera`, or take the first one attached.
fn choose(registry: &Registry, select: &Select) -> Result<CameraInfo> {
    let (cameras, errors) = registry.enumerate();
    for (backend, error) in &errors {
        eprintln!("warning: backend {backend} could not enumerate: {error}");
    }
    match &select.camera {
        Some(wanted) => cameras
            .into_iter()
            .find(|c| c.id.as_str() == wanted || c.serial == *wanted)
            .ok_or_else(|| Error::NotFound(format!("camera {wanted:?}"))),
        None => cameras.into_iter().next().ok_or_else(|| {
            Error::NotFound(
                "no cameras attached (run `firstlight-cli list`; a build without \
                 --features touptek only sees the simulator)"
                    .into(),
            )
        }),
    }
}

fn open(registry: &Registry, select: &Select) -> Result<(CameraInfo, Box<dyn Camera>)> {
    let info = choose(registry, select)?;
    let camera = registry.open(info.backend, &info.id)?;
    Ok((info, camera))
}

fn list(verbose: bool) -> Result<()> {
    let registry = registry(&Select {
        camera: None,
        backend: None,
    });
    let (cameras, errors) = registry.enumerate();
    for (backend, error) in &errors {
        eprintln!("warning: backend {backend} could not enumerate: {error}");
    }
    if cameras.is_empty() {
        println!("No cameras found.");
    } else {
        report::print_camera_table(&cameras);
    }
    // Why the list may be short for reasons unrelated to what is plugged in.
    for note in registry.notes() {
        println!("note: {note}");
    }
    if cameras.is_empty() {
        return Ok(());
    }
    if verbose {
        for camera in &cameras {
            println!();
            match registry.open(camera.backend, &camera.id) {
                Ok(open) => report::print_controls(open.as_ref())?,
                Err(e) => println!("  {}: cannot open ({e})", camera.id),
            }
        }
    }
    Ok(())
}

fn info(select: &Select) -> Result<()> {
    let registry = registry(select);
    let (_, camera) = open(&registry, select)?;
    // Print what the *open* camera reports: enumeration cannot see sensor
    // geometry on every backend, so the pre-connect record has holes in it.
    report::print_camera_detail(camera.info());
    report::print_controls(camera.as_ref())?;
    Ok(())
}

struct CaptureOptions {
    output: std::path::PathBuf,
    frames: Option<u64>,
    duration: Option<Duration>,
    observer: String,
    telescope: String,
}

fn capture(select: &Select, settings: &Settings, options: CaptureOptions) -> Result<()> {
    let registry = registry(select);
    let (info, mut camera) = open(&registry, select)?;
    apply(camera.as_mut(), settings)?;

    let interrupted = install_interrupt_handler();
    let mut writer = SerWriter::create(
        &options.output,
        SerMetadata {
            observer: options.observer.clone(),
            instrument: info.display_name.clone(),
            telescope: options.telescope.clone(),
            little_endian_flag: false,
        },
    )?;

    println!(
        "Recording {} to {} (Ctrl-C to stop)",
        info.display_name,
        options.output.display()
    );
    camera.start_streaming()?;

    let started = Instant::now();
    let mut written = 0u64;
    let mut timeouts = 0u64;
    let mut progress = report::Progress::new();
    let outcome = loop {
        if interrupted.load(Ordering::SeqCst) {
            break Ok("interrupted");
        }
        if options.frames.is_some_and(|limit| written >= limit) {
            break Ok("frame limit reached");
        }
        if options
            .duration
            .is_some_and(|limit| started.elapsed() >= limit)
        {
            break Ok("time limit reached");
        }

        match camera.next_frame(settings.timeout) {
            Ok(frame) => {
                writer.write_frame(&frame)?;
                written += 1;
                progress.frame(&frame);
                progress.maybe_print(written, camera.dropped_frames());
            }
            Err(Error::Timeout(waited)) => {
                // Normal during long exposures; only worth reporting.
                timeouts += 1;
                eprintln!("warning: no frame within {waited:?} ({timeouts} so far); still waiting");
            }
            Err(e) => break Err(e),
        }
    };

    let _ = camera.stop_streaming();
    let frames = writer.finish()?;
    println!(
        "\nWrote {frames} frame(s) to {} in {:.1}s ({} dropped by the camera)",
        options.output.display(),
        started.elapsed().as_secs_f32(),
        camera.dropped_frames()
    );

    match outcome {
        Ok(reason) => {
            println!("Stopped: {reason}.");
            Ok(())
        }
        Err(e) => {
            // The file is already finalised, so the partial capture survives.
            eprintln!("Capture ended early: {e}");
            Err(e)
        }
    }
}

fn snap(
    select: &Select,
    settings: &Settings,
    output: std::path::PathBuf,
    count: u32,
    object: &str,
) -> Result<()> {
    let registry = registry(select);
    let (info, mut camera) = open(&registry, select)?;
    apply(camera.as_mut(), settings)?;

    let meta = FitsMetadata {
        instrument: info.display_name.clone(),
        object: object.to_string(),
        pixel_size_um: Some(info.pixel_size_um),
        ..FitsMetadata::default()
    };

    camera.start_streaming()?;
    let interrupted = install_interrupt_handler();
    let mut saved = 0;
    let result = (|| -> Result<()> {
        for index in 0..count {
            if interrupted.load(Ordering::SeqCst) {
                break;
            }
            let frame = camera.next_frame(settings.timeout)?;
            let path = numbered_path(&output, index, count);
            write_fits(&path, &frame, &meta)?;
            saved += 1;
            println!(
                "Saved {} ({}x{}, {}, {})",
                path.display(),
                frame.width(),
                frame.height(),
                frame.meta.bit_depth,
                frame.meta.format
            );
        }
        Ok(())
    })();
    let _ = camera.stop_streaming();

    if saved > 0 {
        println!("{saved} still(s) written.");
    }
    result
}

fn watch(select: &Select, settings: &Settings, duration: Option<Duration>) -> Result<()> {
    let registry = registry(select);
    let (info, mut camera) = open(&registry, select)?;
    apply(camera.as_mut(), settings)?;

    let events = camera.events();
    let interrupted = install_interrupt_handler();
    camera.start_streaming()?;
    println!("Watching {} (Ctrl-C to stop)", info.display_name);

    let started = Instant::now();
    let mut progress = report::Progress::new();
    let mut frames = 0u64;
    loop {
        if interrupted.load(Ordering::SeqCst) {
            println!("\nInterrupted.");
            break;
        }
        if duration.is_some_and(|limit| started.elapsed() >= limit) {
            println!("\nTime limit reached.");
            break;
        }
        while let Ok(event) = events.try_recv() {
            println!("\nevent: {event}");
        }
        match camera.next_frame(settings.timeout) {
            Ok(frame) => {
                frames += 1;
                progress.frame(&frame);
                progress.maybe_print(frames, camera.dropped_frames());
            }
            Err(Error::Timeout(waited)) => {
                eprintln!("\nwarning: no frame within {waited:?}");
            }
            Err(e) => {
                println!();
                let _ = camera.stop_streaming();
                return Err(e);
            }
        }
    }
    let _ = camera.stop_streaming();
    Ok(())
}

/// Apply the settings flags, in the order the hardware needs: geometry first,
/// because binning and bit depth reset the ROI and rescale control ranges.
fn apply(camera: &mut dyn Camera, settings: &Settings) -> Result<()> {
    if let Some(bits) = settings.bits {
        camera.set_bit_depth(bits)?;
    }
    if let Some(binning) = settings.bin {
        camera.set_binning(binning)?;
    }
    if let Some(roi) = settings.roi {
        camera.set_roi(roi)?;
    }
    if let Some(exposure) = settings.exposure {
        camera.set_exposure_us(exposure.as_micros().min(u64::MAX as u128) as u64)?;
    }
    if let Some(gain) = settings.gain {
        camera.set_gain(gain)?;
    }
    if let Some(offset) = settings.offset {
        camera.set_offset(offset)?;
    }
    if let Some([r, g, b]) = settings.wb {
        camera.set_white_balance(firstlight_core::WhiteBalance {
            red: r,
            green: g,
            blue: b,
        })?;
    }
    for (id, value) in &settings.controls {
        camera.set_control(*id, *value)?;
    }
    // Report what the camera actually settled on, which is not always what
    // was asked for: ranges get clamped and ROIs get rounded.
    let exposure = camera.exposure_us().unwrap_or(0);
    println!(
        "Settings: exposure {}, gain {}, offset {}, ROI {}, bin {}, {}",
        report::format_exposure(exposure),
        camera.control(ControlId::Gain).unwrap_or(-1),
        camera.control(ControlId::Offset).unwrap_or(-1),
        camera.roi()?,
        camera.binning()?,
        camera.bit_depth()?
    );
    Ok(())
}

/// `still.fits` with count 3 becomes `still_0001.fits`, `still_0002.fits`, ...
fn numbered_path(base: &std::path::Path, index: u32, count: u32) -> std::path::PathBuf {
    if count <= 1 {
        return base.to_path_buf();
    }
    let stem = base
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "still".into());
    let extension = base
        .extension()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "fits".into());
    base.with_file_name(format!("{stem}_{:04}.{extension}", index + 1))
}

/// Ctrl-C sets a flag rather than killing the process, so a recording in
/// progress gets its trailer and frame count written.
fn install_interrupt_handler() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    let handler_flag = flag.clone();
    if let Err(e) = ctrlc::set_handler(move || {
        handler_flag.store(true, Ordering::SeqCst);
    }) {
        eprintln!(
            "warning: could not install a Ctrl-C handler ({e}); an interrupt will lose the file trailer"
        );
    }
    flag
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_paths_only_appear_when_there_are_several() {
        let base = std::path::Path::new("/tmp/still.fits");
        assert_eq!(numbered_path(base, 0, 1), base);
        assert_eq!(
            numbered_path(base, 0, 3),
            std::path::Path::new("/tmp/still_0001.fits")
        );
        assert_eq!(
            numbered_path(base, 11, 20),
            std::path::Path::new("/tmp/still_0012.fits")
        );
    }
}
