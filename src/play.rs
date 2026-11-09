//! Plays what the stream holds through the codec: each record at its place (a format switches the
//! output where its audio starts, a gain or now playing applies from there), the audio decoded to
//! 32-bit stereo and scaled by fade × song gain, untouched when that is one; the volume is the codec's.

use alloc::vec::Vec;

use embassy_futures::select::select;
use embassy_time::{Duration, Timer};
use nori_sink_protocol::Format;

use crate::codec::{self, Output};
use crate::flac::{self, Flac};
use crate::psram;
use crate::stream::{self, Kind, Record, Stream};

const SILENCE: [u8; 1000] = [0; 1000];

/// Frames moved to the codec at once.
const CHUNK: usize = 1024;


/// Held before the codec starts, against the network's jitter.
const START_MS: u32 = 300;

/// With less than that held, how long nothing more coming waits before what is held plays.
const SETTLE: Duration = Duration::from_millis(200);

/// The fade nori's `Ramp`s move: its gain now, and a step per frame for the frames left.
struct Fade {
    gain: f32,
    step: f32,
    left: u32,
}

impl Fade {
    fn start(&mut self, target: f32, ms: u32, rate: u32) {
        let frames = (ms as u64 * rate as u64 / 1000) as u32;
        if frames == 0 {
            *self = Fade { gain: target, step: 0.0, left: 0 };
        } else {
            *self = Fade { gain: self.gain, step: (target - self.gain) / frames as f32, left: frames };
        }
    }
}

/// An encoded packet decoded, for the record it came from.
struct Decoded {
    epoch: u32,
    at: usize,
    frames: usize,
}

struct Player {
    output: Output,
    format: Option<Format>,
    fade: Fade,
    /// The song's ReplayGain, linear.
    song_gain: f32,
    flac: Flac,
    packet: Vec<u8>,
    /// Decoded samples, interleaved stereo.
    samples: Vec<i32>,
    decoded: Option<Decoded>,
    bytes: Vec<u8>,
}

/// Plays `stream` through `output`, for good.
pub async fn play(stream: &'static Stream, output: Output) -> ! {
    let mut p = Player {
        output,
        format: None,
        fade: Fade { gain: 1.0, step: 0.0, left: 0 },
        song_gain: 1.0,
        flac: Flac::new(psram::vec(0, flac::PLANES)),
        packet: Vec::new(),
        samples: psram::vec(0, 2 * flac::MAX_BLOCK),
        decoded: None,
        bytes: psram::vec(0, CHUNK * codec::FRAME),
    };
    let mut dry = false;
    loop {
        let (paused, first, epoch) = stream.with(|s| (s.paused, s.first(), s.epoch));
        // Silence keeps MCLK running while a cast has nothing to play: the codec pops when it stops.
        let casting = stream.with(|s| s.cast);
        // A format, gain or now playing at the front applies even while paused: the screens show the
        // song before it plays.
        let marker = first.is_some_and(|(_, r)| matches!(r.kind, Kind::Format | Kind::Gain | Kind::Now));
        let paused = paused && !marker;
        if (paused || first.is_none()) && casting && p.output.sending() {
            if first.is_none() && !paused && !dry {
                dry = true;
                log::info!("the stream ran dry at {} ms", embassy_time::Instant::now().as_millis());
            }
            p.output.push(&SILENCE).await;
            continue;
        }
        if paused {
            p.output.stop();
            stream.playing(false);
            stream.with(|s| s.delay_us = 0);
            stream.woke.wait().await;
            continue;
        }
        let Some((at, r)) = first else {
            if p.output.sending() {
                p.output.drain().await;
                p.output.stop();
                stream.playing(false);
                stream.with(|s| s.delay_us = 0);
            }
            stream.woke.wait().await;
            continue;
        };
        dry = false;
        match r.kind {
            Kind::Format => {
                p.format = stream.with(|s| stream::decode_format(s.payload(at, &r)));
                // Gain comes only before encoded songs: PCM has it applied.
                p.song_gain = 1.0;
                if let Some(f) = p.format {
                    log::info!("playing {} Hz, {} channels, {:?} {}", f.rate, f.channels, f.codec, f.bits);
                }
                pop(stream);
            }
            Kind::Gain => {
                let db = stream.with(|s| {
                    let b = s.payload(at, &r);
                    f32::from_le_bytes([b[0], b[1], b[2], b[3]])
                });
                p.song_gain = libm::powf(10.0, db / 20.0);
                pop(stream);
            }
            Kind::Now => {
                let rate = p.format.map_or(44_100, |f| f.rate);
                stream.with(|s| {
                    log_now(s, at, &r);
                    let Some(n) = stream::decode_now(s.payload(at, &r)) else { return };
                    // nori sends a cover only once it has it cached: a song without one keeps the
                    // album's cover from the song before, if it is of the same album.
                    let cover = n.cover.or_else(|| s.now.as_ref().filter(|was| !n.album.is_empty() && was.album == n.album).and_then(|was| was.cover));
                    let shown = stream::Shown { title: n.title.into(), artist: n.artist.into(), album: n.album.into(), cover, duration_ms: n.duration_ms, at_ms: n.at_ms, from: s.played, rate };
                    s.now = Some(shown);
                });
                // No flash for a new song: the LED's current on the board's 5 V is heard in the speaker.
                stream.changed();
                pop(stream);
            }
            Kind::Pcm | Kind::Encoded => {
                let Some(format) = p.format else {
                    log::warn!("audio before its format");
                    pop(stream);
                    continue;
                };
                let rate = format.rate;
                if !p.output.sending() {
                    // The stream may change while it waits: look at it again after.
                    if p.ready(stream, rate).await {
                        p.output.start(rate);
                        stream.playing(p.output.sending());
                    }
                    continue;
                }
                if rate != p.output.rate() {
                    // The last song is heard out before the rate changes.
                    p.output.drain().await;
                    p.output.stop();
                    p.output.start(rate);
                }
                if r.kind == Kind::Pcm {
                    p.pcm(stream, at, &r, format).await;
                } else {
                    p.encoded(stream, at, &r, epoch, format).await;
                }
            }
            Kind::Wrap => unreachable!("the ring steps over a wrap"),
        }
    }
}

fn pop(stream: &Stream) {
    stream.with(|s| s.pop());
    stream.took();
}

impl Player {
    /// Waits for enough audio to start on, or for nothing more coming; whether to play now (the
    /// stream may have changed meanwhile).
    async fn ready(&mut self, stream: &Stream, rate: u32) -> bool {
        loop {
            if stream.with(|s| s.holds(START_MS, rate)) {
                return true;
            }
            if let embassy_futures::select::Either::Second(()) = select(stream.woke.wait(), Timer::after(SETTLE)).await {
                return true;
            }
            if stream.with(|s| s.paused || s.first().is_none()) {
                return false;
            }
        }
    }

    /// Plays some of the PCM record at `at`.
    async fn pcm(&mut self, stream: &Stream, at: usize, r: &Record, f: Format) {
        let channels = f.channels.max(1) as usize;
        let width = if f.bits == 24 { 3 } else { 2 };
        let frame = channels * width;
        let n = stream.with(|s| {
            let n = (r.frames - s.consumed).min(CHUNK as u32) as usize;
            let from = s.consumed as usize * frame;
            let src = &s.payload(at, r)[from..from + n * frame];
            for (i, f) in src.chunks_exact(frame).enumerate() {
                let sample = |c: usize| {
                    let b = &f[c * width..];
                    if width == 3 { i32::from_le_bytes([0, b[0], b[1], b[2]]) } else { i32::from_le_bytes([0, 0, b[0], b[1]]) }
                };
                self.samples[2 * i] = sample(0);
                self.samples[2 * i + 1] = sample(1.min(channels - 1));
            }
            commit(s, r, n);
            n
        });
        stream.took();
        self.out(stream, 0, n).await;
    }

    /// Plays some of the encoded packet at `at`, decoding it first.
    async fn encoded(&mut self, stream: &Stream, at: usize, r: &Record, epoch: u32, f: Format) {
        if !self.decoded.as_ref().is_some_and(|d| d.epoch == epoch && d.at == at) {
            stream.with(|s| {
                self.packet.clear();
                self.packet.extend_from_slice(s.payload(at, r));
            });
            let made = self.flac.decode(&self.packet, &mut self.samples);
            let frames = made.unwrap_or_else(|e| {
                log::warn!("{:?}: {e}", f.codec);
                0
            });
            self.decoded = Some(Decoded { epoch, at, frames });
        }
        let decoded = self.decoded.as_ref().map_or(0, |d| d.frames);
        let (from, n) = stream.with(|s| {
            let from = (r.skip + s.consumed) as usize;
            // A packet that made less than it says plays out as silence.
            let n = (r.frames - s.consumed).min(CHUNK as u32) as usize;
            commit(s, r, n);
            (from, n)
        });
        if from + n > decoded {
            self.samples[2 * decoded.min(from)..2 * (from + n)].fill(0);
        }
        stream.took();
        if stream.with(|s| s.consumed == 0) {
            self.decoded = None;
        }
        self.out(stream, from, n).await;
    }

    /// Scales frames `from..from + n` of the samples and sends them.
    async fn out(&mut self, stream: &Stream, from: usize, n: usize) {
        let rate = self.output.rate();
        let (volume, ramp) = stream.with(|s| (s.volume, s.ramp.take()));
        if let Some((target, ms)) = ramp {
            self.fade.start(target, ms, rate);
        }
        // The volume is the codec's, heard at once; the samples carry the fade and the song's gain.
        self.output.set_volume(volume);
        let samples = &mut self.samples[2 * from..2 * (from + n)];
        let still = self.fade.left == 0;
        let gain = self.fade.gain * self.song_gain;
        if !(still && gain == 1.0) {
            for frame in samples.as_chunks_mut::<2>().0 {
                let g = self.fade.gain * self.song_gain;
                for s in frame {
                    *s = (*s as f32 * g).clamp(i32::MIN as f32, i32::MAX as f32) as i32;
                }
                if self.fade.left > 0 {
                    self.fade.gain += self.fade.step;
                    self.fade.left -= 1;
                }
            }
        }
        for (b, s) in self.bytes.as_chunks_mut::<4>().0.iter_mut().zip(samples.iter()) {
            b.copy_from_slice(&s.to_le_bytes());
        }

        self.output.push(&self.bytes[..n * codec::FRAME]).await;
        if !self.output.sending() {
            stream.playing(false);
        }
        let delay_us = self.output.delay_us();
        stream.with(|s| s.delay_us = delay_us);
    }

}

/// What a screen will show of the song at `at`.
fn log_now(s: &stream::State, at: usize, r: &Record) {
    let Some(now) = stream::decode_now(s.payload(at, r)) else { return };
    let seconds = now.duration_ms / 1000;
    let at = now.at_ms / 1000;
    log::info!("now playing: {} - {} - {} ({}:{:02}, from {}:{:02}) at {} ms", now.title, now.artist, now.album, seconds / 60, seconds % 60, at / 60, at % 60, embassy_time::Instant::now().as_millis());
    match now.cover {
        Some(id) => match s.covers.get(id) {
            Some(jpeg) => log::info!("its cover: {id}, {} bytes", jpeg.len()),
            None => log::warn!("its cover {id} is not held"),
        },
        None => log::info!("no cover"),
    }
}

/// Counts `n` frames of the first record played, dropping it once all are.
fn commit(s: &mut stream::State, r: &Record, n: usize) {
    s.played += n as u64;
    s.consumed += n as u32;
    if s.consumed >= r.frames {
        s.pop();
    }
}
