//! MP3 as nori's offload sends it: the file's frames one by one, decoded to 32-bit stereo.

use alloc::vec::Vec;
use nanomp3_core::{Decoder, MAX_SAMPLES_PER_FRAME};

/// The frames an MPEG audio frame plays, from its header; none if `p` is not one.
pub fn frames(p: &[u8]) -> Option<u32> {
    if p.len() < 4 || p[0] != 0xFF || p[1] & 0xE0 != 0xE0 {
        return None;
    }
    // Version: 3 MPEG-1, 2 MPEG-2, 0 MPEG-2.5. Layer: 3 I, 2 II, 1 III.
    let (version, layer) = ((p[1] >> 3) & 3, (p[1] >> 1) & 3);
    match (version, layer) {
        (1, _) | (_, 0) => None,
        (_, 3) => Some(384),
        (3, _) | (_, 2) => Some(1152),
        _ => Some(576),
    }
}

pub struct Mp3 {
    decoder: Decoder,
    /// The packet with its own header after it: the decoder takes a frame only once it sees the
    /// next one start.
    input: Vec<u8>,
    pcm: [i32; MAX_SAMPLES_PER_FRAME],
}

impl Mp3 {
    pub fn new() -> Mp3 {
        Mp3 { decoder: Decoder::new(), input: Vec::new(), pcm: [0; MAX_SAMPLES_PER_FRAME] }
    }

    /// A new stream: what the last left in the decoder is not heard.
    pub fn reset(&mut self) {
        self.decoder = Decoder::new();
    }

    /// Decodes packet `p` into `out` (interleaved stereo, 32-bit): always the frames its header says,
    /// silence where the decoder made none (the frames after a seek need the ones before).
    pub fn decode(&mut self, p: &[u8], out: &mut [i32]) -> Result<usize, &'static str> {
        let frames = frames(p).ok_or("not an MPEG audio frame")? as usize;
        if out.len() < 2 * frames {
            return Err("a frame too long");
        }
        self.input.clear();
        self.input.extend_from_slice(p);
        self.input.extend_from_slice(&p[..4]);
        let (_, result) = self.decoder.decode(&self.input, &mut self.pcm);
        let made = match result {
            Ok(info) if info.channels.num() == 1 => {
                let n = info.samples_produced.min(frames);
                for (i, s) in self.pcm[..n].iter().enumerate() {
                    out[2 * i] = *s;
                    out[2 * i + 1] = *s;
                }
                n
            }
            Ok(info) => {
                let n = info.samples_produced.min(frames);
                out[..2 * n].copy_from_slice(&self.pcm[..2 * n]);
                n
            }
            Err(_) => 0,
        };
        out[2 * made..2 * frames].fill(0);
        Ok(frames)
    }
}
