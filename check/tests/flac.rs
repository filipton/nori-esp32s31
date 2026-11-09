//! The FLAC decoder against the reference encoder (`flac` on the PATH): what it encodes decodes to the
//! same samples, frame by frame as nori sends them.

use std::process::Command;

use nori_esp32s31_check::flac::{self, Flac, MAX_BLOCK, PLANES};

/// Noise over a sweep, deterministic, at full scale for `bits`.
fn audio(frames: usize, channels: usize, bits: u32) -> Vec<i32> {
    let mut seed = 0x1234_5678u32;
    let top = (1i64 << (bits - 1)) - 1;
    (0..frames * channels)
        .map(|i| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let t = (i / channels) as f64;
            let tone = (t * t * 1e-6).sin() * 0.6 + (t * 0.05 * (1 + i % channels) as f64).sin() * 0.2;
            let noise = (seed >> 8) as f64 / (1u32 << 24) as f64 - 0.5;
            ((tone + noise * 0.3) * top as f64).clamp(-top as f64, top as f64) as i32
        })
        .collect()
}

fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in bytes {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x8005 } else { crc << 1 };
        }
    }
    crc
}

/// The file as nori's offload sends it: the head through STREAMINFO, then each frame, found by its
/// sync and the CRC-16 at its end.
fn packets(file: &[u8]) -> Vec<&[u8]> {
    // Past the metadata blocks.
    let mut at = 4;
    loop {
        let last = file[at] & 0x80 != 0;
        at += 4 + u32::from_be_bytes([0, file[at + 1], file[at + 2], file[at + 3]]) as usize;
        if last {
            break;
        }
    }
    let mut out = vec![&file[..4 + 4 + 34]];
    while at < file.len() {
        let mut end = at + 2;
        loop {
            end += 1;
            let whole = end == file.len() || (file[end] == 0xFF && file[end + 1] & 0xFE == 0xF8);
            if whole && crc16(&file[at..end - 2]) == u16::from_be_bytes([file[end - 2], file[end - 1]]) {
                break;
            }
        }
        out.push(&file[at..end]);
        at = end;
    }
    out
}

fn check(name: &str, channels: usize, bits: u32, rate: u32, args: &[&str]) {
    let dir = std::env::temp_dir().join(format!("nori-flac-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let frames = 100_000;
    let input = audio(frames, channels, bits);
    let bytes = (bits / 8) as usize;
    let raw: Vec<u8> = input.iter().flat_map(|s| s.to_le_bytes()[..bytes].to_vec()).collect();
    std::fs::write(dir.join("in.raw"), raw).unwrap();
    let status = Command::new("flac")
        .args(["--silent", "--force", "--force-raw-format", "--endian=little", "--sign=signed", "--lax"])
        .arg(format!("--channels={channels}"))
        .arg(format!("--bps={bits}"))
        .arg(format!("--sample-rate={rate}"))
        .args(args)
        .arg("-o")
        .arg(dir.join("out.flac"))
        .arg(dir.join("in.raw"))
        .status()
        .expect("flac on the PATH");
    assert!(status.success());
    let file = std::fs::read(dir.join("out.flac")).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();

    let mut decoder = Flac::new(vec![0; PLANES]);
    let mut out = vec![0i32; 2 * MAX_BLOCK];
    let mut got = Vec::new();
    for p in packets(&file) {
        let n = decoder.decode(p, &mut out).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(Some(n as u32), flac::frames(p), "{name}: the frames a packet says");
        got.extend_from_slice(&out[..2 * n]);
    }
    let want: Vec<i32> = (0..frames)
        .flat_map(|f| {
            let l = input[f * channels];
            let r = input[f * channels + (channels > 1) as usize];
            [l << (32 - bits), r << (32 - bits)]
        })
        .collect();
    assert_eq!(got.len(), want.len(), "{name}: frames");
    let first_wrong = got.iter().zip(&want).position(|(a, b)| a != b);
    assert_eq!(first_wrong, None, "{name}: a sample differs");
}

#[test]
fn every_kind_of_frame_decodes_to_the_samples_encoded() {
    // Fixed predictors; LPC with each stereo mode; mono; 24-bit; long and odd blocks; wasted bits.
    check("fixed", 2, 16, 44_100, &["-0"]);
    check("lpc", 2, 16, 44_100, &["-8"]);
    check("exhaustive", 2, 16, 48_000, &["-8", "-e", "-p", "-r", "0,8"]);
    check("independent", 2, 16, 44_100, &["-8", "--no-mid-side"]);
    check("mono", 1, 16, 22_050, &["-5"]);
    check("24-bit", 2, 24, 96_000, &["-8"]);
    check("long blocks", 2, 24, 96_000, &["-8", "--blocksize=16384"]);
    check("odd blocks", 2, 16, 44_100, &["-5", "--blocksize=1000"]);
    check("short blocks", 2, 16, 8_000, &["-5", "--blocksize=192"]);
}

#[test]
fn the_head_and_other_bytes_are_told_apart() {
    assert_eq!(flac::frames(b"fLaC\0\0\0\x22"), Some(0));
    assert_eq!(flac::frames(&[0xFF, 0xF8, 0xC9, 0x18, 0x00, 0xC2]), Some(4096));
    assert_eq!(flac::frames(b"ID3\x04"), None);
}
