//! Byte-level checks on the SER and FITS writers. Both formats are read by
//! other people's software, so "it opened in my viewer" is not enough — the
//! offsets and keywords have to be exactly where the specs say.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use firstlight_core::control::{Binning, BitDepth, Roi};
use firstlight_core::format::ser::{SerColorId, SerMetadata, SerWriter};
use firstlight_core::frame::{BayerPattern, Frame, FrameMeta, PixelFormat};
use firstlight_core::{Error, FitsMetadata, write_fits};

const SER_HEADER: usize = 178;

fn temp_path(name: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("firstlight-test-{}-{name}", std::process::id()));
    path
}

fn frame(width: u32, height: u32, depth: BitDepth, format: PixelFormat, fill: u16) -> Frame {
    let meta = FrameMeta {
        sequence: 3,
        timestamp: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        width,
        height,
        format,
        bit_depth: depth,
        exposure_us: 1_500_000,
        gain: 220,
        offset: 12,
        binning: Binning(2),
        roi: Roi::new(4, 8, width, height),
        dropped: 0,
        temperature_c: Some(-10.5),
    };
    let samples = (width * height) as usize * format.samples_per_pixel();
    let data = if depth.bytes_per_sample() == 1 {
        vec![fill as u8; samples]
    } else {
        fill.to_le_bytes().repeat(samples)
    };
    Frame::new(meta, data).unwrap()
}

fn read_i32(bytes: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

#[test]
fn ser_header_matches_the_v3_layout() {
    let path = temp_path("layout.ser");
    let mut writer = SerWriter::create(&path, SerMetadata::for_camera("SIM-1080C")).unwrap();
    for _ in 0..3 {
        writer
            .write_frame(&frame(
                4,
                2,
                BitDepth::SIXTEEN,
                PixelFormat::Bayer(BayerPattern::Grbg),
                0x1234,
            ))
            .unwrap();
    }
    assert_eq!(writer.finish().unwrap(), 3);

    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[0..14], b"LUCAM-RECORDER");
    assert_eq!(read_i32(&bytes, 18), SerColorId::BayerGrbg as i32);
    assert_eq!(read_i32(&bytes, 26), 4, "ImageWidth");
    assert_eq!(read_i32(&bytes, 30), 2, "ImageHeight");
    assert_eq!(read_i32(&bytes, 34), 16, "PixelDepthPerPlane");
    assert_eq!(read_i32(&bytes, 38), 3, "FrameCount");
    assert_eq!(&bytes[82..91], b"SIM-1080C", "Instrument field");

    // Header, then three 16 byte frames, then a 64 bit timestamp per frame.
    let frame_bytes = 4 * 2 * 2;
    assert_eq!(bytes.len(), SER_HEADER + 3 * frame_bytes + 3 * 8);
    let first_pixel = u16::from_le_bytes([bytes[SER_HEADER], bytes[SER_HEADER + 1]]);
    assert_eq!(first_pixel, 0x1234, "frame data must be stored verbatim");

    // Timestamps are .NET ticks: 2023-11-14T22:13:20Z, well past year 2000.
    let ticks = i64::from_le_bytes(
        bytes[bytes.len() - 24..bytes.len() - 16]
            .try_into()
            .unwrap(),
    );
    assert!(ticks > 621_355_968_000_000_000, "ticks look wrong: {ticks}");
    std::fs::remove_file(&path).ok();
}

#[test]
fn ser_rejects_a_mid_file_geometry_change() {
    let path = temp_path("geometry.ser");
    let mut writer = SerWriter::create(&path, SerMetadata::default()).unwrap();
    writer
        .write_frame(&frame(4, 2, BitDepth::SIXTEEN, PixelFormat::Mono, 1))
        .unwrap();
    let result = writer.write_frame(&frame(8, 2, BitDepth::SIXTEEN, PixelFormat::Mono, 1));
    assert!(
        matches!(result, Err(Error::InvalidGeometry(_))),
        "got {result:?}"
    );
    assert_eq!(writer.frame_count(), 1, "the bad frame must not be counted");
    std::fs::remove_file(&path).ok();
}

#[test]
fn ser_is_finalised_even_if_the_writer_is_just_dropped() {
    let path = temp_path("dropped.ser");
    {
        let mut writer = SerWriter::create(&path, SerMetadata::default()).unwrap();
        writer
            .write_frame(&frame(2, 2, BitDepth::EIGHT, PixelFormat::Mono, 9))
            .unwrap();
        // No `finish`: an aborted capture must still leave a readable file.
    }
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(read_i32(&bytes, 38), 1, "FrameCount was never patched in");
    assert_eq!(bytes.len(), SER_HEADER + 4 + 8);
    std::fs::remove_file(&path).ok();
}

fn cards(bytes: &[u8]) -> Vec<String> {
    bytes
        .chunks(80)
        .map(|c| String::from_utf8_lossy(c).to_string())
        .take_while(|card| !card.starts_with("END "))
        .collect()
}

fn card_value(cards: &[String], key: &str) -> Option<String> {
    cards
        .iter()
        .find(|c| c.starts_with(&format!("{key:<8}=")))
        .map(|c| c[10..].split('/').next().unwrap_or("").trim().to_string())
}

#[test]
fn fits_header_carries_the_acquisition_keywords() {
    let path = temp_path("still.fits");
    let frame = frame(
        4,
        2,
        BitDepth::SIXTEEN,
        PixelFormat::Bayer(BayerPattern::Rggb),
        40_000,
    );
    let mut meta = FitsMetadata::for_camera("SIM-1080C");
    meta.pixel_size_um = Some(2.9);
    meta.telescope = "Newtonian 200/1000".into();
    meta.white_balance = Some(firstlight_core::WhiteBalance {
        red: 150,
        green: 128,
        blue: 333,
    });
    write_fits(&path, &frame, &meta).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes.len() % 2880, 0, "FITS files are 2880-byte blocked");
    let cards = cards(&bytes);
    assert!(cards[0].starts_with("SIMPLE  =                    T"));
    assert_eq!(card_value(&cards, "BITPIX").as_deref(), Some("16"));
    assert_eq!(card_value(&cards, "NAXIS").as_deref(), Some("2"));
    assert_eq!(card_value(&cards, "NAXIS1").as_deref(), Some("4"));
    assert_eq!(card_value(&cards, "NAXIS2").as_deref(), Some("2"));
    assert_eq!(card_value(&cards, "BZERO").as_deref(), Some("32768.0"));
    assert_eq!(card_value(&cards, "EXPTIME").as_deref(), Some("1.5"));
    assert_eq!(card_value(&cards, "GAIN").as_deref(), Some("220"));
    assert_eq!(card_value(&cards, "XBINNING").as_deref(), Some("2"));
    assert_eq!(
        card_value(&cards, "BAYERPAT").as_deref(),
        Some("'RGGB    '")
    );
    assert_eq!(card_value(&cards, "CCD-TEMP").as_deref(), Some("-10.5"));
    // These cameras apply the white balance to the raw data itself, so the
    // file has to record what was applied or it cannot be undone later.
    assert_eq!(card_value(&cards, "WB_R").as_deref(), Some("150"));
    assert_eq!(card_value(&cards, "WB_G").as_deref(), Some("128"));
    assert_eq!(card_value(&cards, "WB_B").as_deref(), Some("333"));
    assert!(
        card_value(&cards, "DATE-OBS")
            .unwrap()
            .starts_with("'2023-11-14T22:13:20"),
        "DATE-OBS was {:?}",
        card_value(&cards, "DATE-OBS")
    );
    assert!(
        card_value(&cards, "XPIXSZ").unwrap().starts_with("5.8"),
        "pixel size should account for 2x2 binning"
    );

    // 16-bit data is big-endian and offset by BZERO.
    let data = &bytes[2880..];
    let stored = i16::from_be_bytes([data[0], data[1]]);
    assert_eq!(i32::from(stored) + 32768, 40_000);
    std::fs::remove_file(&path).ok();
}

#[test]
fn fits_writes_mono_eight_bit_without_bzero() {
    let path = temp_path("mono8.fits");
    let frame = frame(4, 2, BitDepth::EIGHT, PixelFormat::Mono, 200);
    write_fits(&path, &frame, &FitsMetadata::default()).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    let cards = cards(&bytes);
    assert_eq!(card_value(&cards, "BITPIX").as_deref(), Some("8"));
    assert!(card_value(&cards, "BZERO").is_none());
    assert!(
        card_value(&cards, "BAYERPAT").is_none(),
        "mono data must not claim a Bayer pattern"
    );
    assert_eq!(bytes[2880], 200);
    std::fs::remove_file(&path).ok();
}

#[test]
fn fits_writes_rgb_as_separate_planes() {
    let path = temp_path("rgb.fits");
    let meta = FrameMeta {
        sequence: 0,
        timestamp: SystemTime::now(),
        width: 2,
        height: 1,
        format: PixelFormat::Rgb,
        bit_depth: BitDepth::EIGHT,
        exposure_us: 1000,
        gain: 0,
        offset: 0,
        binning: Binning::ONE,
        roi: Roi::full(2, 1),
        dropped: 0,
        temperature_c: None,
    };
    // Interleaved input: (10,20,30), (40,50,60).
    let frame = Frame::new(meta, vec![10, 20, 30, 40, 50, 60]).unwrap();
    write_fits(&path, &frame, &FitsMetadata::default()).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    let cards = cards(&bytes);
    assert_eq!(card_value(&cards, "NAXIS").as_deref(), Some("3"));
    assert_eq!(card_value(&cards, "NAXIS3").as_deref(), Some("3"));
    // FITS wants plane-major: all red, then all green, then all blue.
    assert_eq!(&bytes[2880..2886], &[10, 40, 20, 50, 30, 60]);
    std::fs::remove_file(&path).ok();
}
