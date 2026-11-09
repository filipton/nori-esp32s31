//! What the speaker holds of nori's stream, in PSRAM: records in the order they came (audio, and the
//! frames placed in it: format, gain, now playing), each with the stream frame it starts at (frames
//! since `Hello`). The receiver puts them in and keeps the credit; the player takes them out.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use nori_sink_protocol::{Button, Codec, Device, Format};


/// The ring's bytes.
const DEPTH: usize = 12 * 1024 * 1024;

/// What the host may fill: the ring less its records' heads and the end a record does not fit in.
pub const BUFFER_BYTES: u32 = (DEPTH - 256 * 1024) as u32;

/// Covers kept for `Now` to name, oldest going first: as many as nori counts on a device keeping.
const COVERS: usize = 32;

/// The most bytes a cover takes.
const COVER_BYTES: usize = 64 * 1024;

/// A record's head: kind, payload length, frames, frames skipped, start.
const HEAD: usize = 24;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Pcm,
    Encoded,
    Format,
    Gain,
    Now,
    /// The rest of the ring is unused; the next record is at its start.
    Wrap,
}

impl Kind {
    fn of(b: u8) -> Kind {
        [Kind::Pcm, Kind::Encoded, Kind::Format, Kind::Gain, Kind::Now].get(b as usize).copied().unwrap_or(Kind::Wrap)
    }

    fn code(self) -> u8 {
        self as u8
    }

    fn is_audio(self) -> bool {
        matches!(self, Kind::Pcm | Kind::Encoded)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Record {
    pub kind: Kind,
    pub len: u32,
    /// The frames it plays (a flush may shorten it).
    pub frames: u32,
    /// Encoded: frames its decoding starts with that are not played.
    pub skip: u32,
    /// The stream frame its first played frame is.
    pub start: u64,
}

struct Ring {
    bytes: Vec<u8>,
    read: usize,
    write: usize,
    /// Bytes in use, the unused end before a wrap included.
    used: usize,
}

impl Ring {
    fn head_at(&self, at: usize) -> Record {
        let b = &self.bytes[at..at + HEAD];
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        Record { kind: Kind::of(b[0]), len: u32_at(4), frames: u32_at(8), skip: u32_at(12), start: u32_at(16) as u64 | (u32_at(20) as u64) << 32 }
    }

    fn set_head(&mut self, at: usize, r: &Record) {
        let b = &mut self.bytes[at..at + HEAD];
        b[0] = r.kind.code();
        b[4..8].copy_from_slice(&r.len.to_le_bytes());
        b[8..12].copy_from_slice(&r.frames.to_le_bytes());
        b[12..16].copy_from_slice(&r.skip.to_le_bytes());
        b[16..24].copy_from_slice(&r.start.to_le_bytes());
    }

    /// Where the record at `at` is: past an end too short for a head, or a wrap, the ring's start.
    fn settle(&self, at: usize) -> usize {
        if DEPTH - at < HEAD || self.head_at(at).kind == Kind::Wrap { 0 } else { at }
    }

    /// The record at the read place, if any.
    fn first(&self) -> Option<(usize, Record)> {
        if self.used == 0 {
            return None;
        }
        let at = self.settle(self.read);
        Some((at, self.head_at(at)))
    }

    /// Where the record after the one at `at` is, or none at the writer's place.
    fn after(&self, at: usize, r: &Record) -> Option<usize> {
        let next = at + HEAD + r.len as usize;
        if next == self.write {
            return None;
        }
        Some(self.settle(next))
    }

    /// Drops the first record.
    fn pop(&mut self) {
        let Some((at, r)) = self.first() else { return };
        let skipped = if at < self.read { DEPTH - self.read } else { 0 };
        self.used -= skipped + HEAD + r.len as usize;
        self.read = at + HEAD + r.len as usize;
        if self.used == 0 {
            self.read = 0;
            self.write = 0;
        }
    }

    /// Puts a record in, if it fits.
    fn put(&mut self, r: &Record, payload: &[u8]) -> bool {
        let n = HEAD + payload.len();
        if self.used == 0 {
            self.read = 0;
            self.write = 0;
        }
        let at = if self.used == 0 || self.write > self.read {
            if DEPTH - self.write >= n {
                self.write
            } else if self.read >= n {
                if DEPTH - self.write >= HEAD {
                    let wrap = Record { kind: Kind::Wrap, len: 0, frames: 0, skip: 0, start: 0 };
                    self.set_head(self.write, &wrap);
                }
                self.used += DEPTH - self.write;
                0
            } else {
                return false;
            }
        } else if self.read - self.write >= n {
            self.write
        } else {
            return false;
        };
        self.set_head(at, r);
        self.bytes[at + HEAD..at + n].copy_from_slice(payload);
        self.write = at + n;
        self.used += n;
        true
    }

    /// Ends the ring at `at`: what is there and after it is dropped.
    fn cut(&mut self, at: usize) {
        self.write = at;
        self.used = if at >= self.read { at - self.read } else { DEPTH - self.read + at };
        if self.used == 0 {
            self.read = 0;
            self.write = 0;
        }
    }

    fn free(&self) -> usize {
        DEPTH - self.used
    }
}

/// The covers nori sent, by id.
pub struct Covers {
    bytes: Vec<u8>,
    /// Each slot's id and length.
    slots: [Option<(u32, usize)>; COVERS],
    next: usize,
}

impl Covers {
    fn new() -> Covers {
        // Larger than the internal heap, so it is allocated in PSRAM.
        Covers { bytes: vec![0; COVERS * COVER_BYTES], slots: [None; COVERS], next: 0 }
    }

    /// Keeps `jpeg` under `id`; false if it is too big to.
    pub fn keep(&mut self, id: u32, jpeg: &[u8]) -> bool {
        if jpeg.len() > COVER_BYTES {
            return false;
        }
        self.bytes[self.next * COVER_BYTES..][..jpeg.len()].copy_from_slice(jpeg);
        self.slots[self.next] = Some((id, jpeg.len()));
        self.next = (self.next + 1) % COVERS;
        true
    }

    pub fn get(&self, id: u32) -> Option<&[u8]> {
        let slot = self.slots.iter().position(|s| s.is_some_and(|(held, _)| held == id))?;
        let (_, len) = self.slots[slot]?;
        Some(&self.bytes[slot * COVER_BYTES..][..len])
    }

    fn clear(&mut self) {
        self.slots = [None; COVERS];
        self.next = 0;
    }
}

pub struct State {
    ring: Ring,
    /// The stream frame the next audio in starts at.
    next: u64,
    /// Frames played since `Hello`.
    pub played: u64,
    /// Frames of the first record played.
    pub consumed: u32,
    /// Audio before this frame is dropped as it comes: a flush went back past what was played.
    drop_to: u64,
    /// How much to hold, ms.
    depth_ms: Option<u32>,
    /// The format audio comes in.
    incoming: Option<Format>,
    /// Frames the host was asked for and has not sent.
    credit: u64,
    /// A `Want` went since the hold was last over half the depth.
    asked: bool,
    /// Bumped when the first record is dropped by a flush or a new connection: the player drops
    /// what it decoded of it.
    pub epoch: u32,
    pub paused: bool,
    /// The user's volume, linear.
    pub volume: f32,
    /// A fade to start: its target and length, ms.
    pub ramp: Option<(f32, u32)>,
    /// What the output holds yet to be heard, µs.
    pub delay_us: u32,
    pub covers: Covers,
    /// A cast is connected.
    pub cast: bool,
    /// The output is sending.
    pub playing: bool,
    /// The song being heard, for the screens.
    pub now: Option<Shown>,
    pub net: Net,
}

/// The song being heard, as the screens show it.
pub struct Shown {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub cover: Option<u32>,
    pub duration_ms: u32,
    /// Where in it the audio from stream frame `from` is, ms, and its rate: how far in it is.
    pub at_ms: u32,
    pub from: u64,
    pub rate: u32,
}

impl Shown {
    /// Where in it it is, ms.
    pub fn place_ms(&self, played: u64) -> u32 {
        (self.at_ms as u64 + played.saturating_sub(self.from) * 1000 / self.rate.max(1) as u64).min(u32::MAX as u64) as u32
    }

    /// How far through it is, 0 to 1.
    pub fn progress(&self, played: u64) -> Option<f32> {
        (self.duration_ms > 0).then(|| (self.place_ms(played) as f32 / self.duration_ms as f32).min(1.0))
    }
}

/// Where the speaker is with its network, for the screens.
pub enum Net {
    /// Joining the saved network.
    Joining,
    /// Waiting to be set up, over its access point (this name) or BLE.
    Setup(&'static str),
    Ready(&'static str),
}

impl State {
    fn held(&self) -> u64 {
        self.next.saturating_sub(self.played)
    }

    /// Bytes per frame of the PCM coming in.
    pub fn incoming_frame_bytes(&self) -> usize {
        self.incoming.map_or(4, |f| f.channels as usize * f.bits as usize / 8)
    }

    pub fn first(&self) -> Option<(usize, Record)> {
        self.ring.first()
    }

    /// The first record's payload.
    pub fn payload(&self, at: usize, r: &Record) -> &[u8] {
        &self.ring.bytes[at + HEAD..at + HEAD + r.len as usize]
    }

    pub fn pop(&mut self) {
        self.ring.pop();
        self.consumed = 0;
    }

    /// Whether at least `ms` of audio is held, at `rate`.
    pub fn holds(&self, ms: u32, rate: u32) -> bool {
        self.held() * 1000 >= ms as u64 * rate as u64
    }

    fn depth_frames(&self) -> Option<u64> {
        let rate = self.incoming.map_or(48_000, |f| f.rate) as u64;
        Some(self.depth_ms? as u64 * rate / 1000)
    }

    /// The frames to ask for now, if it is time to.
    pub fn want(&self) -> Option<u32> {
        let depth = self.depth_frames()?;
        let held = self.held();
        if self.asked || held * 2 >= depth {
            return None;
        }
        // Encoded audio is smaller than this; the host fits the depth to the buffer anyway.
        let bytes_per_frame = match self.incoming {
            Some(f) if f.codec != Codec::Pcm => f.channels as u64 * 3,
            _ => self.incoming_frame_bytes() as u64,
        };
        let room = (self.ring.free().saturating_sub(64 * 1024) as u64) / bytes_per_frame.max(1);
        let frames = (depth - held).min(room).min(u32::MAX as u64) as u32;
        (frames > 0).then_some(frames)
    }

    pub fn status(&self) -> Device {
        Device::Status { played: self.played, held: self.held().min(u32::MAX as u64) as u32, delay_us: self.delay_us }
    }

    /// Drops what is held past stream frame `keep`, or everything not played.
    fn flush(&mut self, keep: Option<u64>) {
        let cut = keep.filter(|k| *k >= self.played).unwrap_or(self.played);
        let mut at = self.ring.first();
        let mut first = true;
        while let Some((pos, mut r)) = at {
            let plays = r.kind.is_audio() && r.frames > 0;
            if plays && r.start + r.frames as u64 > cut && r.start < cut {
                // It plays up to the cut.
                r.frames = (cut - r.start) as u32;
                self.ring.set_head(pos, &r);
                if let Some(next) = self.ring.after(pos, &r) {
                    self.ring.cut(next);
                }
                break;
            }
            if r.start >= cut {
                self.cut_at(pos, first);
                break;
            }
            at = self.ring.after(pos, &r).map(|p| (p, self.ring.head_at(p)));
            first = false;
        }
        self.next = self.next.min(keep.unwrap_or(self.played));
        self.drop_to = self.played;
        self.credit = 0;
        self.asked = false;
    }

    fn cut_at(&mut self, pos: usize, first: bool) {
        if first {
            self.ring.cut(self.ring.read);
            self.consumed = 0;
            self.epoch = self.epoch.wrapping_add(1);
        } else {
            self.ring.cut(pos);
        }
    }
}

pub struct Stream {
    state: Mutex<CriticalSectionRawMutex, RefCell<State>>,
    /// Audio came in, or the player is to look again (a flush, pause, resume or a new connection).
    pub woke: Signal<CriticalSectionRawMutex, ()>,
    /// The player made room.
    room: Signal<CriticalSectionRawMutex, ()>,
    /// The receiver has something to tell the host (it is time to ask for audio).
    pub tell: Signal<CriticalSectionRawMutex, ()>,
    pub presses: Channel<CriticalSectionRawMutex, Button, 4>,
    /// The OLED and the round screen have something new to show.
    pub oled: Signal<CriticalSectionRawMutex, ()>,
    pub screen: Signal<CriticalSectionRawMutex, ()>,
    /// The knob set this volume: the receiver tells nori.
    pub knob_volume: Signal<CriticalSectionRawMutex, f32>,
    /// The volume changed, from the knob or from nori: it is to be kept.
    pub volume_set: Signal<CriticalSectionRawMutex, ()>,
    /// The knob moved the place in the song (ms), and where it rests is to be told nori.
    pub seek_moved: Signal<CriticalSectionRawMutex, u32>,
    pub seek: Signal<CriticalSectionRawMutex, u32>,
}

impl Stream {
    pub fn new() -> &'static Stream {
        // Larger than the internal heap, so it is allocated in PSRAM.
        let ring = Ring { bytes: vec![0; DEPTH], read: 0, write: 0, used: 0 };
        let state = State {
            ring,
            next: 0,
            played: 0,
            consumed: 0,
            drop_to: 0,
            depth_ms: None,
            incoming: None,
            credit: 0,
            asked: false,
            epoch: 0,
            paused: false,
            // Until one is kept: not loud.
            volume: 0.3,
            ramp: None,
            delay_us: 0,
            covers: Covers::new(),
            cast: false,
            playing: false,
            now: None,
            net: Net::Joining,
        };
        alloc::boxed::Box::leak(alloc::boxed::Box::new(Stream {
            state: Mutex::new(RefCell::new(state)),
            woke: Signal::new(),
            room: Signal::new(),
            tell: Signal::new(),
            presses: Channel::new(),
            oled: Signal::new(),
            screen: Signal::new(),
            knob_volume: Signal::new(),
            volume_set: Signal::new(),
            seek_moved: Signal::new(),
            seek: Signal::new(),
        }))
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        self.state.lock(|s| f(&mut s.borrow_mut()))
    }

    /// A new connection: nothing held, the counts from zero.
    pub fn reset(&self) {
        self.with(|s| {
            s.ring.cut(s.ring.read);
            s.next = 0;
            s.played = 0;
            s.consumed = 0;
            s.drop_to = 0;
            s.depth_ms = None;
            s.incoming = None;
            s.credit = 0;
            s.asked = false;
            s.epoch = s.epoch.wrapping_add(1);
            s.paused = false;
            s.ramp = Some((1.0, 0));
            s.covers.clear();
            s.cast = false;
            s.now = None;
        });
        self.knob_volume.reset();
        self.woke.signal(());
        self.room.signal(());
        self.changed();
    }

    /// What the screens show changed.
    pub fn changed(&self) {
        self.oled.signal(());
        self.screen.signal(());
    }

    /// The output started or stopped sending.
    pub fn playing(&self, playing: bool) {
        if self.with(|s| core::mem::replace(&mut s.playing, playing)) != playing {
            self.changed();
        }
    }

    pub fn set_depth(&self, ms: u32) {
        self.with(|s| {
            s.depth_ms = Some(ms);
            s.asked = false;
        });
    }

    /// The player took a record or frames out.
    pub fn took(&self) {
        self.room.signal(());
        if self.with(|s| s.want().is_some()) {
            self.tell.signal(());
        }
    }

    /// Asks the host for `frames`, as [`State::want`] said.
    pub fn asked(&self, frames: u32) {
        self.with(|s| {
            s.credit = frames as u64;
            s.asked = true;
        });
    }

    /// The `Format`, `Gain` or `Now` placed here in the stream.
    pub async fn put_marker(&self, kind: Kind, payload: &[u8]) {
        if kind == Kind::Format {
            self.with(|s| s.incoming = decode_format(payload));
        }
        self.put(kind, 0, payload).await;
    }

    /// Audio that plays `frames` (encoded: a whole packet).
    pub async fn put_audio(&self, kind: Kind, frames: u32, payload: &[u8]) {
        self.put(kind, frames, payload).await;
    }

    async fn put(&self, kind: Kind, frames: u32, payload: &[u8]) {
        loop {
            let done = self.with(|s| {
                let mut r = Record { kind, len: payload.len() as u32, frames, skip: 0, start: s.next };
                let mut body = payload;
                if kind.is_audio() && r.start < s.drop_to {
                    let skip = (s.drop_to - r.start).min(frames as u64) as u32;
                    if kind == Kind::Pcm {
                        body = &body[(skip as usize * s.incoming_frame_bytes()).min(body.len())..];
                        r.len = body.len() as u32;
                    } else {
                        r.skip = skip;
                    }
                    r.frames -= skip;
                    r.start += skip as u64;
                }
                if !(kind.is_audio() && r.frames == 0 && frames > 0) && !s.ring.put(&r, body) {
                    return false;
                }
                if kind.is_audio() {
                    s.next += frames as u64;
                    s.credit = s.credit.saturating_sub(frames as u64);
                    if s.credit == 0 || s.depth_frames().is_some_and(|d| s.held() * 2 >= d) {
                        s.asked = false;
                    }
                }
                true
            });
            if done {
                self.woke.signal(());
                return;
            }
            self.room.wait().await;
        }
    }

    /// Drops what is held past `keep`, as [`nori_sink_protocol::Host::Flush`] says.
    pub fn flush(&self, keep: Option<u64>) {
        self.with(|s| s.flush(keep));
        self.woke.signal(());
        self.room.signal(());
    }
}

/// A `Format` as a record keeps it.
/// The song whose audio follows, as nori names it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Now<'a> {
    pub id: u32,
    pub title: &'a str,
    pub artist: &'a str,
    pub album: &'a str,
    pub duration_ms: u32,
    pub cover: Option<u32>,
    /// Where in the song the audio after it starts, ms.
    pub at_ms: u32,
}

const NOW_HEAD: usize = 17;

pub fn encode_now(now: &Now) -> Vec<u8> {
    let mut out = Vec::with_capacity(NOW_HEAD + now.title.len() + now.artist.len() + now.album.len() + 2);
    out.extend_from_slice(&now.id.to_le_bytes());
    out.extend_from_slice(&now.duration_ms.to_le_bytes());
    out.push(now.cover.is_some() as u8);
    out.extend_from_slice(&now.cover.unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&now.at_ms.to_le_bytes());
    for (i, text) in [now.title, now.artist, now.album].iter().enumerate() {
        if i > 0 {
            out.push(0);
        }
        out.extend_from_slice(text.as_bytes());
    }
    out
}

pub fn decode_now(b: &[u8]) -> Option<Now<'_>> {
    let (head, text) = b.split_at_checked(NOW_HEAD)?;
    let u32_at = |i: usize| u32::from_le_bytes([head[i], head[i + 1], head[i + 2], head[i + 3]]);
    let mut text = core::str::from_utf8(text).ok()?.split('\0');
    Some(Now { id: u32_at(0), duration_ms: u32_at(4), cover: (head[8] != 0).then(|| u32_at(9)), at_ms: u32_at(13), title: text.next()?, artist: text.next()?, album: text.next()? })
}

pub fn encode_format(f: &Format) -> [u8; 7] {
    let r = f.rate.to_le_bytes();
    let codec = match f.codec {
        Codec::Pcm => 0,
        Codec::Flac => 1,
        Codec::Mp3 => 2,
    };
    [r[0], r[1], r[2], r[3], f.channels, f.bits, codec]
}

pub fn decode_format(b: &[u8]) -> Option<Format> {
    let codec = match b.get(6)? {
        0 => Codec::Pcm,
        1 => Codec::Flac,
        2 => Codec::Mp3,
        _ => return None,
    };
    Some(Format { rate: u32::from_le_bytes([b[0], b[1], b[2], b[3]]), channels: b[4], bits: b[5], codec })
}
