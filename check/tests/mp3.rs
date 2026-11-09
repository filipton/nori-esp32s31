//! MP3 frame by frame, as nori sends it, against the decoder reading the whole file (`ffmpeg` on the
//! PATH makes it).

use std::process::Command;

use nanomp3_core::{Decoder, MAX_SAMPLES_PER_FRAME};
use nori_esp32s31_check::mp3::{self, Mp3};

/// The bytes of the MPEG frame at the start of `p`.
fn frame_len(p: &[u8]) -> usize {
    const KBPS: [u32; 16] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0];
    let kbps = KBPS[(p[2] >> 4) as usize];
    let rate = [44_100, 48_000, 32_000][((p[2] >> 2) & 3) as usize];
    (144_000 * kbps / rate) as usize + ((p[2] >> 1) & 1) as usize
}

fn encode(channels: u32) -> Vec<u8> {
    let path = std::env::temp_dir().join(format!("nori-mp3-{}-{channels}.mp3", std::process::id()));
    let status = Command::new("ffmpeg")
        .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=3", "-ar", "44100"])
        .arg("-ac")
        .arg(channels.to_string())
        .args(["-b:a", "192k", "-write_xing", "0", "-id3v2_version", "0"])
        .arg(&path)
        .status()
        .expect("ffmpeg on the PATH");
    assert!(status.success());
    let file = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    file
}

#[test]
fn frames_decode_one_by_one_as_the_whole_file_does() {
    for channels in [1, 2] {
        let file = encode(channels);
        let mut whole = Vec::new();
        let mut decoder = Decoder::new();
        let mut pcm = [0i32; MAX_SAMPLES_PER_FRAME];
        let mut rest = &file[..];
        while !rest.is_empty() {
            let (n, result) = decoder.decode(rest, &mut pcm);
            rest = &rest[n..];
            if let Ok(info) = result {
                for f in pcm[..info.samples_produced * channels as usize].chunks(channels as usize) {
                    whole.extend_from_slice(&[f[0], f[channels as usize - 1]]);
                }
            }
        }

        let mut mp3 = Mp3::new();
        let mut out = vec![0i32; 2 * 1152];
        let mut got = Vec::new();
        let mut at = 0;
        while at + 4 <= file.len() {
            let p = &file[at..(at + frame_len(&file[at..])).min(file.len())];
            assert_eq!(mp3::frames(p), Some(1152));
            let n = mp3.decode(p, &mut out).unwrap();
            got.extend_from_slice(&out[..2 * n]);
            at += p.len();
        }
        // The whole file's decoder holds its last frame back until it sees another.
        assert!(got.len() >= whole.len() && whole.len() > 100_000, "{channels}: {} against {}", got.len(), whole.len());
        assert_eq!(got[..whole.len()], whole[..], "{channels} channels");
    }
}

#[test]
fn a_header_tells_its_frames() {
    // MPEG-1 layer III, MPEG-2 layer III, MPEG-1 layer II, and no frame.
    assert_eq!(mp3::frames(&[0xFF, 0xFB, 0x90, 0x00]), Some(1152));
    assert_eq!(mp3::frames(&[0xFF, 0xF3, 0x90, 0x00]), Some(576));
    assert_eq!(mp3::frames(&[0xFF, 0xFD, 0x90, 0x00]), Some(1152));
    assert_eq!(mp3::frames(b"ID3\x04"), None);
}
