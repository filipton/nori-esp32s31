//! The round 240×240 GC9A01 display on SPI2: SCL (clock) GPIO20, SDA (MOSI) GPIO21, DC GPIO22, CS GPIO23,
//! RST GPIO24. The song's cover while one is heard, nori's wordmark on black otherwise, and for a
//! moment after the volume changes a thin ring of it round the edge. The display only listens (no
//! MISO), so the code cannot tell whether it is there.

use alloc::vec::Vec;

use embassy_futures::join::join;
use embassy_futures::select::select;
use embassy_time::{Duration, Instant, Timer};
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::pixelcolor::raw::RawU16;
use embedded_graphics::prelude::*;
use embedded_hal::delay::DelayNs;
use embedded_hal::spi::{Operation, SpiBus};
use esp_hal::delay::Delay;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::peripherals::{DMA_AXI_CH0, GPIO20, GPIO21, GPIO22, GPIO23, GPIO24, SPI2};
use esp_hal::spi::Mode;
use esp_hal::spi::master::{Config, Spi, SpiDma};
use esp_hal::{Async, Blocking};
use static_cell::StaticCell;
use esp_hal::time::Rate;
use mipidsi::Builder;
use mipidsi::interface::SpiInterface;
use mipidsi::models::GC9A01;
use mipidsi::options::{ColorInversion, ColorOrder};
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use crate::psram;
use crate::stream::Stream;

const SIZE: usize = 240;

/// The display's SPI clock. Lower it if the picture shows noise: long wires can't carry it.
const SPI_MHZ: u32 = 80;

/// nori's wordmark in its dark colors (`tools/logo.py`).
const LOGO: &[u8] = include_bytes!("../assets/logo.rgb565");
const LOGO_W: usize = 180;
const LOGO_H: usize = 75;

/// The largest cover side decoded; bigger ones are skipped.
const COVER_MAX: usize = 1024;

/// The volume ring: its radii, px from the center, and its sweep, from the bottom left round
/// through the top to the bottom right.
const RING_OUTER: f32 = 120.0;
const RING_WIDTH: f32 = 9.0;
const RING_SWEEP: f32 = 270.0;
/// The picture's size while paused, and how quickly it eases to a size (ms for most of the way a third).
const PAUSED_SCALE: f32 = 0.72;
const SCALE_MS: f32 = 110.0;
/// A new picture stays big this long even while paused: a song skipped to plays right after.
const PAUSE_AFTER_MS: u64 = 1500;

/// How long the ring takes to grow in, and its level to catch up with the volume, ms.
const GROW_MS: f32 = 220.0;
const GLIDE_MS: f32 = 90.0;
/// How long the ring stays after the volume last changed.
const RING_MS: u64 = 1500;
/// A frame while something moves.
const FRAME_MS: u64 = 16;

/// A new picture opens from the center over this long, out to the corners.
const OPEN_MS: u64 = 450;
const OPEN_RADIUS: f32 = 171.0;

/// The ring's filled part: nori's Autumn accent, kaki persimmon (#E8743B, crates/look's dark palette).
const KAKI: Rgb565 = Rgb565::new(0xe8 >> 3, 0x74 >> 2, 0x3b >> 3);

pub struct Pins {
    pub spi: SPI2<'static>,
    pub dma: DMA_AXI_CH0<'static>,
    pub sck: GPIO20<'static>,
    pub mosi: GPIO21<'static>,
    pub dc: GPIO22<'static>,
    pub cs: GPIO23<'static>,
    pub rst: GPIO24<'static>,
}

/// Bytes of pixels sent at a time: a block of rows across the screen.
const CHUNK_BYTES: usize = SIZE * BLOCK * 2;
/// A rectangle this small goes without DMA, which is slower to start than it is to send.
const SMALL_BYTES: usize = 1024;

/// The display's SPI while its setup runs: a device that only writes, CS low around each transaction.
struct Dev {
    spi: Spi<'static, Blocking>,
    cs: Output<'static>,
}

impl embedded_hal::spi::ErrorType for Dev {
    type Error = esp_hal::spi::Error;
}

impl embedded_hal::spi::SpiDevice for Dev {
    fn transaction(&mut self, ops: &mut [Operation<'_, u8>]) -> Result<(), Self::Error> {
        self.cs.set_low();
        let done = ops.iter_mut().try_for_each(|op| match op {
            Operation::Write(bytes) => SpiBus::write(&mut self.spi, bytes),
            Operation::DelayNs(ns) => {
                Delay::new().delay_ns(*ns);
                Ok(())
            }
            _ => Ok(()),
        });
        let flushed = SpiBus::flush(&mut self.spi);
        self.cs.set_high();
        done.and(flushed)
    }
}

/// The display after its setup: rectangles of pixels over SPI with DMA, each piece of them made while
/// the one before goes out.
struct Lcd {
    spi: SpiDma<'static, Async>,
    dc: Output<'static>,
    cs: Output<'static>,
    /// Held so it stays high: the display resets with it low.
    _rst: Option<Output<'static>>,
    bufs: [&'static mut [u8; CHUNK_BYTES]; 2],
}

impl Lcd {
    /// Sends the rectangle `x0..x1` × `y0..y1`, row by row, each pixel from `pixel`.
    async fn rect(&mut self, x0: usize, x1: usize, y0: usize, y1: usize, mut pixel: impl FnMut(usize, usize) -> Rgb565) -> Result<(), esp_hal::spi::Error> {
        if x1 <= x0 || y1 <= y0 {
            return Ok(());
        }
        let Lcd { spi, dc, cs, bufs, .. } = self;
        let be = |a: usize, b: usize| [(a >> 8) as u8, a as u8, (b >> 8) as u8, b as u8];
        cs.set_low();
        // A command and its arguments: a few bytes, sent as they are (DMA is slower to start).
        let mut command = |c: u8, args: &[u8]| -> Result<(), esp_hal::spi::Error> {
            dc.set_low();
            SpiBus::write(spi, &[c])?;
            SpiBus::flush(spi)?;
            dc.set_high();
            if args.is_empty() { Ok(()) } else { SpiBus::write(spi, args) }
        };
        let started = command(0x2A, &be(x0, x1 - 1)).and_then(|_| command(0x2B, &be(y0, y1 - 1))).and_then(|_| command(0x2C, &[]));
        if let Err(e) = started {
            log::warn!("screen: window: {e:?}");
        }
        if started.is_err() {
            cs.set_high();
            return started;
        }
        // Rows a piece holds, and a piece made into `buf` from row `from`: its bytes.
        let rows = (CHUNK_BYTES / 2 / (x1 - x0)).max(1);
        let mut make = |buf: &mut [u8], from: usize| {
            let mut i = 0;
            for y in from..(from + rows).min(y1) {
                for x in x0..x1 {
                    let [hi, lo] = pixel(x, y).into_storage().to_be_bytes();
                    buf[i] = hi;
                    buf[i + 1] = lo;
                    i += 2;
                }
            }
            i
        };
        let [a, b] = bufs;
        let (mut out, mut next): (&mut [u8], &mut [u8]) = (&mut a[..], &mut b[..]);
        let mut n = make(out, y0);
        let mut from = y0 + rows;
        // A small rectangle goes in one piece, as it is.
        if from >= y1 && n <= SMALL_BYTES {
            let sent = SpiBus::write(spi, &out[..n]).and_then(|_| SpiBus::flush(spi));
            if let Err(e) = sent {
                log::warn!("screen: small: {e:?}");
            }
            cs.set_high();
            return sent;
        }
        let mut sent = Ok(());
        while sent.is_ok() {
            if from >= y1 {
                sent = spi.write_async(&out[..n]).await;
                break;
            }
            let (s, m) = join(spi.write_async(&out[..n]), async { make(next, from) }).await;
            sent = s;
            n = m;
            from += rows;
            core::mem::swap(&mut out, &mut next);
        }
        cs.set_high();
        sent
    }
}

/// What the screen shows under the ring.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Showing {
    Logo,
    Cover(u32),
}

#[embassy_executor::task]
pub async fn run(p: Pins, stream: &'static Stream) {
    let spi = match Spi::new(p.spi, Config::default().with_frequency(Rate::from_mhz(SPI_MHZ)).with_mode(Mode::_0)) {
        Ok(spi) => spi.with_sck(p.sck).with_mosi(p.mosi),
        Err(e) => {
            log::error!("screen: spi: {e:?}; the screen stays off");
            return;
        }
    };
    let cs = Output::new(p.cs, Level::High, OutputConfig::default());
    let dc = Output::new(p.dc, Level::Low, OutputConfig::default());
    let rst = Output::new(p.rst, Level::High, OutputConfig::default());
    let mut buffer = [0u8; 512];
    let interface = SpiInterface::new(Dev { spi, cs }, dc, &mut buffer);

    // Most GC9A01 modules show colors inverted without this, and take blue first.
    let display = match Builder::new(GC9A01, interface).reset_pin(rst).invert_colors(ColorInversion::Inverted).color_order(ColorOrder::Bgr).init(&mut Delay::new()) {
        Ok(display) => display,
        Err(e) => {
            log::error!("screen: init: {e:?}; the screen stays off");
            return;
        }
    };
    // Set up: from here on the SPI goes with DMA.
    let (interface, _, rst) = display.release();
    let (Dev { spi, cs }, dc) = interface.release();
    static BUFS: StaticCell<[[u8; CHUNK_BYTES]; 2]> = StaticCell::new();
    let [a, b] = BUFS.init([[0; CHUNK_BYTES]; 2]);
    let mut lcd = Lcd { spi: spi.with_dma(p.dma).into_async(), dc, cs, _rst: rst, bufs: [a, b] };

    // The picture to show, and the one on the screen under the ring (the new one as it opens).
    let mut base = psram::vec(Rgb565::BLACK, SIZE * SIZE);
    let mut front = psram::vec(Rgb565::BLACK, SIZE * SIZE);
    let mut showing = None;
    // A new picture opening from the center: when it began and the radius drawn so far.
    let mut opening: Option<(Instant, f32)> = None;
    let mut ring = Ring { shown: 0.0, level: 0.0, target: 0.0, until: None, at: Instant::now() };
    let mut drawn_ring = ring.look();
    let mut volume_seen: Option<f32> = None;
    let mut frames = 0u32;
    let band = Band::new();
    // How big the picture is shown: smaller while paused. What was last drawn, and when it moved.
    let (mut scale, mut drawn_scale, mut scaled_at) = (1.0f32, 1.0f32, Instant::now());
    let mut pause_after = Instant::now();

    loop {
        let (want, cast, volume, paused) = stream.with(|s| {
            let want = match (s.cast, &s.now) {
                (true, Some(now)) => now.cover.map_or(Showing::Logo, Showing::Cover),
                _ => Showing::Logo,
            };
            (want, s.cast, s.volume, s.cast && s.now.is_some() && s.paused)
        });

        if showing != Some(want) {
            match want {
                Showing::Cover(id) if cover(stream, id, &mut base).is_some() => {}
                _ => logo(&mut base),
            }
            showing = Some(want);
            opening = Some((Instant::now(), 0.0));
            // A new song opens big: skipped to while paused, it plays a moment later.
            (scale, drawn_scale) = (1.0, 1.0);
            pause_after = Instant::now() + Duration::from_millis(PAUSE_AFTER_MS);
        }

        // The picture eases to its size: smaller while paused (not straight after a new one opens).
        let goal = if paused && Instant::now() >= pause_after { PAUSED_SCALE } else { 1.0 };
        let ms = scaled_at.elapsed().as_millis().min(100) as f32;
        scaled_at = Instant::now();
        scale = if (goal - scale).abs() < 0.002 { goal } else { goal + (scale - goal) * libm::expf(-ms / SCALE_MS) };

        // A change of the volume during a cast (from the knob or from nori) shows the ring.
        if cast && volume_seen.is_some_and(|seen| seen != volume) {
            if ring.shown == 0.0 {
                ring.level = volume;
            }
            ring.target = volume;
            ring.until = Some(Instant::now() + Duration::from_millis(RING_MS));
        }
        volume_seen = cast.then_some(volume);

        let moving = ring.step();
        let look = ring.look();
        let mut failed = false;
        if let Some((began, radius)) = opening {
            let t = (began.elapsed().as_millis() as f32 / OPEN_MS as f32).min(1.0);
            let to = ease_out(t) * OPEN_RADIUS;
            _ = runs::<()>(radius, to, |y, x0, x1| {
                for x in x0..x1 {
                    front[y * SIZE + x] = place(&base, drawn_scale, x, y);
                }
                Ok(())
            });
            failed |= blocks(&mut lcd, &front, radius, to, look).await.is_err();
            frames += 1;
            opening = (t < 1.0).then_some((began, to));
            if opening.is_none() {
                log::debug!("screen: opened in {} ms, {frames} frames", began.elapsed().as_millis());
                frames = 0;
            }
        }
        if opening.is_none() && scale != drawn_scale {
            failed |= whole(&mut lcd, &base, &mut front, scale, look).await.is_err();
            (drawn_scale, drawn_ring) = (scale, look);
        } else if look != drawn_ring {
            failed |= band.draw(&mut lcd, &front, look).await.is_err();
            drawn_ring = look;
        }
        if failed {
            log::warn!("screen: could not draw");
        }

        let holding = paused && scale == 1.0 && Instant::now() < pause_after;
        if opening.is_some() || moving || scale != goal || holding {
            _ = select(stream.screen.wait(), Timer::after_millis(FRAME_MS)).await;
        } else if let Some(until) = ring.until {
            _ = select(stream.screen.wait(), Timer::at(until)).await;
        } else {
            stream.screen.wait().await;
        }
    }
}

/// The volume ring as it moves.
struct Ring {
    /// How much of it is shown, 0 to 1: it grows in from the edge and thins away.
    shown: f32,
    /// The volume it shows, gliding to `target`.
    level: f32,
    target: f32,
    /// When it starts going.
    until: Option<Instant>,
    /// When it last moved.
    at: Instant,
}

/// The ring as drawn: its width, px, and the volume it shows.
#[derive(Clone, Copy, PartialEq)]
struct Look {
    width: f32,
    level: f32,
}

impl Ring {
    /// Moves it a frame on; whether it still moves.
    fn step(&mut self) -> bool {
        let now = Instant::now();
        let ms = (now - self.at).as_millis().min(100) as f32;
        self.at = now;
        if self.until.is_some_and(|u| now >= u) {
            self.until = None;
        }
        let goal = if self.until.is_some() { 1.0 } else { 0.0 };
        // In and away over GROW_MS, eased both ways (`look`).
        let by = ms / GROW_MS;
        self.shown = approach(self.shown, goal, by);
        // The level closes most of the way to the volume in GLIDE_MS.
        let left = libm::expf(-ms / GLIDE_MS * 3.0);
        self.level = if (self.target - self.level).abs() < 0.003 { self.target } else { self.target + (self.level - self.target) * left };
        self.shown != goal || self.level != self.target
    }

    fn look(&self) -> Look {
        let s = self.shown;
        Look { width: RING_WIDTH * s * s * (3.0 - 2.0 * s), level: self.level }
    }
}

/// `from` moved toward `to` by at most `by`.
fn approach(from: f32, to: f32, by: f32) -> f32 {
    if (to - from).abs() <= by { to } else { from + by * (to - from).signum() }
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t)
}

/// The wordmark, centered on black.
fn logo(base: &mut [Rgb565]) {
    base.fill(Rgb565::BLACK);
    let (x0, y0) = ((SIZE - LOGO_W) / 2, (SIZE - LOGO_H) / 2);
    for y in 0..LOGO_H {
        for x in 0..LOGO_W {
            let at = (y * LOGO_W + x) * 2;
            base[(y0 + y) * SIZE + x0 + x] = RawU16::new(u16::from_le_bytes([LOGO[at], LOGO[at + 1]])).into();
        }
    }
}

/// Cover `id`, its middle square scaled to fill the screen, into `base`; none if it is not held or
/// does not decode.
fn cover(stream: &Stream, id: u32, base: &mut [Rgb565]) -> Option<()> {
    let jpeg = stream.with(|s| {
        let held = s.covers.get(id)?;
        let mut copy = psram::try_vec(0u8, held.len())?;
        copy.copy_from_slice(held);
        Some(copy)
    });
    let Some(jpeg) = jpeg else {
        log::warn!("screen: cover {id} is not held");
        return None;
    };
    let started = Instant::now();
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB).set_max_width(COVER_MAX).set_max_height(COVER_MAX);
    // On the heap (PSRAM): it is 29 KB, most of what this core's stack could hold.
    let mut decoder = alloc::boxed::Box::new(JpegDecoder::new_with_options(ZCursor::new(&jpeg[..]), options));
    if let Err(e) = decoder.decode_headers() {
        log::warn!("screen: cover {id}: {e:?}");
        return None;
    }
    let info = decoder.info()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let Some(mut rgb) = psram::try_vec(0u8, w * h * 3) else {
        log::warn!("screen: cover {id}: no room to decode {w}×{h}");
        return None;
    };
    if let Err(e) = decoder.decode_into(&mut rgb) {
        log::warn!("screen: cover {id}: {e:?}");
        return None;
    }
    fill(&rgb, w, h, base);
    log::info!("screen: cover {id}, {w}×{h}, shown in {} ms", started.elapsed().as_millis());
    Some(())
}

/// The middle square of a `w`×`h` RGB image, scaled to the screen: each pixel the average of the
/// pixels it covers when smaller, the nearest when larger.
fn fill(rgb: &[u8], w: usize, h: usize, base: &mut [Rgb565]) {
    let side = w.min(h);
    let (ox, oy) = ((w - side) / 2, (h - side) / 2);
    let span = |i: usize| {
        let start = i * side / SIZE;
        (start, ((i + 1) * side / SIZE).max(start + 1))
    };
    for y in 0..SIZE {
        let (y0, y1) = span(y);
        for x in 0..SIZE {
            let (x0, x1) = span(x);
            let mut sum = [0u32; 3];
            for sy in y0..y1 {
                let row = &rgb[((oy + sy) * w + ox + x0) * 3..((oy + sy) * w + ox + x1) * 3];
                for px in row.chunks_exact(3) {
                    sum[0] += px[0] as u32;
                    sum[1] += px[1] as u32;
                    sum[2] += px[2] as u32;
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            base[y * SIZE + x] = Rgb565::new((sum[0] / n >> 3) as u8, (sum[1] / n >> 2) as u8, (sum[2] / n >> 3) as u8);
        }
    }
}

/// Calls `run` with each row's runs of pixels from `inner` to `outer` px from the center: left and
/// right of the hole, or one where the row misses it.
fn runs<E>(inner: f32, outer: f32, mut run: impl FnMut(usize, usize, usize) -> Result<(), E>) -> Result<(), E> {
    let c = SIZE as f32 / 2.0;
    let inner = inner.max(0.0);
    for y in 0..SIZE {
        let dy = y as f32 + 0.5 - c;
        if dy.abs() >= outer {
            continue;
        }
        let o = sqrt(outer * outer - dy * dy);
        let i = if dy.abs() < inner { sqrt(inner * inner - dy * dy) } else { 0.0 };
        let clamp = |x: f32| (x.max(0.0) as usize).min(SIZE);
        let (l0, l1, r0, r1) = (clamp(c - o), clamp(c - i), clamp(c + i), clamp(c + o + 1.0));
        if i == 0.0 {
            run(y, l0, r1)?;
        } else {
            run(y, l0, l1)?;
            run(y, r0, r1)?;
        }
    }
    Ok(())
}

const BAND_INNER: f32 = RING_OUTER - RING_WIDTH - 2.0;
const BAND_OUTER: f32 = RING_OUTER + 2.0;

/// Rows sent together: each block of them goes as one or two rectangles, the display being slow to
/// start one; a whole screen goes a block at a time.
const BLOCK: usize = 8;

/// The picture at `x`, `y` when it is shown at `scale` of its size: a circle in the middle, black
/// round it, its edge smoothed.
fn place(base: &[Rgb565], scale: f32, x: usize, y: usize) -> Rgb565 {
    if scale >= 1.0 {
        return base[y * SIZE + x];
    }
    let c = SIZE as f32 / 2.0;
    let (dx, dy) = (x as f32 + 0.5 - c, y as f32 + 0.5 - c);
    let edge = scale * c - sqrt(dx * dx + dy * dy) + 0.5;
    if edge <= 0.0 {
        return Rgb565::BLACK;
    }
    let (sx, sy) = ((c + dx / scale) as usize, (c + dy / scale) as usize);
    let px = base[sy.min(SIZE - 1) * SIZE + sx.min(SIZE - 1)];
    if edge >= 1.0 { px } else { mix(Rgb565::BLACK, px, edge) }
}

/// Sends the whole screen: `base` at `scale` (kept in `front`), the ring over it.
async fn whole(lcd: &mut Lcd, base: &[Rgb565], front: &mut [Rgb565], scale: f32, ring: Look) -> Result<(), esp_hal::spi::Error> {
    let ends = Ends::of(ring);
    let c = SIZE as f32 / 2.0;
    let scale = scale.min(1.0);
    let radius = scale * c;
    // The picture's row or column each of the screen's shows, worked out once.
    let mut from = [0u16; SIZE];
    for (i, f) in from.iter_mut().enumerate() {
        *f = ((c + (i as f32 + 0.5 - c) / scale) as usize).min(SIZE - 1) as u16;
    }
    // In quarter pixels squared (offsets doubled, so whole numbers): inside the picture, outside it,
    // and where the ring may be.
    let sq = |r: f32| (4.0 * r * r) as i32;
    let (inside, outside) = (sq((radius - 1.0).max(0.0)), sq(radius + 1.0));
    let (near, far) = (sq((RING_OUTER - ring.width - 1.0).max(0.0)), sq(RING_OUTER + 1.0));
    let ringed = ring.width > 0.0;
    lcd.rect(0, SIZE, 0, SIZE, |x, y| {
        let (dx, dy) = (2 * x as i32 + 1 - SIZE as i32, 2 * y as i32 + 1 - SIZE as i32);
        let r2 = dx * dx + dy * dy;
        let under = if r2 >= outside {
            Rgb565::BLACK
        } else {
            let px = base[from[y] as usize * SIZE + from[x] as usize];
            if r2 <= inside { px } else { mix(Rgb565::BLACK, px, (radius - sqrt(r2 as f32) / 2.0 + 0.5).clamp(0.0, 1.0)) }
        };
        front[y * SIZE + x] = under;
        if ringed && r2 >= near && r2 <= far { compose(Polar::of(x, y), under, ring, &ends) } else { under }
    })
    .await
}

/// Calls `rect` with rectangles (x and y ranges) of up to [`BLOCK`] rows that hold everything from
/// `inner` to `outer` px from the center, with some pixels either side.
fn block_rects<E>(inner: f32, outer: f32, mut rect: impl FnMut(usize, usize, usize, usize) -> Result<(), E>) -> Result<(), E> {
    // Each row's extent left of the middle and right of it.
    let mut rows = [(SIZE, 0, SIZE, 0); SIZE];
    _ = runs::<()>(inner, outer, |y, a, b| {
        if b > a {
            let r = &mut rows[y];
            if a < SIZE / 2 {
                (r.0, r.1) = (r.0.min(a), r.1.max(b));
            }
            if b > SIZE / 2 {
                (r.2, r.3) = (r.2.min(a.max(SIZE / 2)), r.3.max(b));
            }
        }
        Ok(())
    });
    for top in (0..SIZE).step_by(BLOCK) {
        let (mut l0, mut l1, mut r0, mut r1) = (SIZE, 0, SIZE, 0);
        let (mut y0, mut y1) = (SIZE, 0);
        for (y, r) in rows.iter().enumerate().skip(top).take(BLOCK) {
            if r.1 > r.0 || r.3 > r.2 {
                (y0, y1) = (y0.min(y), y1.max(y + 1));
                (l0, l1, r0, r1) = (l0.min(r.0), l1.max(r.1), r0.min(r.2), r1.max(r.3));
            }
        }
        if y1 <= y0 {
            continue;
        }
        if l1 >= r0 || l1 <= l0 || r1 <= r0 {
            rect(l0.min(r0), r1.max(l1), y0, y1)?;
        } else {
            rect(l0, l1, y0, y1)?;
            rect(r0, r1, y0, y1)?;
        }
    }
    Ok(())
}

/// Sends everything from `inner` to `outer` px from the center, in [`block_rects`]: `front` with the
/// ring over it.
async fn blocks(lcd: &mut Lcd, front: &[Rgb565], inner: f32, outer: f32, ring: Look) -> Result<(), esp_hal::spi::Error> {
    let ends = Ends::of(ring);
    let mut rects = heapless_rects();
    _ = block_rects::<()>(inner, outer, |x0, x1, y0, y1| {
        rects.push((x0, x1, y0, y1));
        Ok(())
    });
    for (x0, x1, y0, y1) in rects {
        lcd.rect(x0, x1, y0, y1, |x, y| compose(Polar::of(x, y), front[y * SIZE + x], ring, &ends)).await?;
    }
    Ok(())
}

/// Room for a band's rectangles: two a block of rows.
fn heapless_rects() -> Vec<(usize, usize, usize, usize)> {
    Vec::with_capacity(2 * SIZE / BLOCK)
}

/// The hardware's square root (libm's is done in software).
fn sqrt(x: f32) -> f32 {
    let r: f32;
    unsafe { core::arch::asm!("fsqrt.s {0}, {1}", out(freg) r, in(freg) x, options(pure, nomem, nostack)) };
    r
}

/// A pixel's place from the center: its offset, its radius and its degrees along the ring's sweep
/// (from the bottom left round through the top; none away from the ring, where it is not needed).
#[derive(Clone, Copy)]
struct Polar {
    dx: f32,
    dy: f32,
    r: f32,
    along: f32,
}

impl Polar {
    fn of(x: usize, y: usize) -> Polar {
        let c = SIZE as f32 / 2.0;
        let (dx, dy) = (x as f32 + 0.5 - c, y as f32 + 0.5 - c);
        let r = sqrt(dx * dx + dy * dy);
        let along = if r >= BAND_INNER { libm::atan2f(dx, -dy).to_degrees() + RING_SWEEP / 2.0 } else { 0.0 };
        Polar { dx, dy, r, along }
    }
}

/// The ring's band worked out once: the rectangles it goes in and where each of their pixels is.
struct Band {
    rects: Vec<(u8, u8, u8, u8)>,
    polar: Vec<Polar>,
}

impl Band {
    fn new() -> Band {
        let mut rects = Vec::new();
        let mut n = 0;
        let mut collect = |x0: usize, x1: usize, y0: usize, y1: usize| -> Result<(), ()> {
            rects.push((x0 as u8, (x1 - 1) as u8, y0 as u8, (y1 - 1) as u8));
            n += (x1 - x0) * (y1 - y0);
            Ok(())
        };
        _ = block_rects(BAND_INNER, BAND_OUTER, &mut collect);
        let mut polar = psram::vec(Polar { dx: 0.0, dy: 0.0, r: 0.0, along: 0.0 }, n);
        let mut at = 0;
        for &(x0, x1, y0, y1) in &rects {
            for y in y0 as usize..=y1 as usize {
                for x in x0 as usize..=x1 as usize {
                    polar[at] = Polar::of(x, y);
                    at += 1;
                }
            }
        }
        Band { rects, polar }
    }

    /// Sends the band: `front` with the ring over it.
    async fn draw(&self, lcd: &mut Lcd, front: &[Rgb565], ring: Look) -> Result<(), esp_hal::spi::Error> {
        let ends = Ends::of(ring);
        let mut at = 0;
        for &(x0, x1, y0, y1) in &self.rects {
            let (x0, x1, y0, y1) = (x0 as usize, x1 as usize + 1, y0 as usize, y1 as usize + 1);
            lcd.rect(x0, x1, y0, y1, |x, y| {
                let p = compose(self.polar[at], front[y * SIZE + x], ring, &ends);
                at += 1;
                p
            })
            .await?;
        }
        Ok(())
    }
}

/// The ring's round ends this frame: the middle of its band, and where the sweep starts, ends and
/// the level reaches.
struct Ends {
    half: f32,
    middle: f32,
    start: (f32, f32),
    end: (f32, f32),
    level: (f32, f32),
    level_along: f32,
}

impl Ends {
    fn of(ring: Look) -> Ends {
        let half = ring.width / 2.0;
        let middle = RING_OUTER - half;
        let at = |along: f32| {
            let t = (along - RING_SWEEP / 2.0).to_radians();
            (middle * libm::sinf(t), -middle * libm::cosf(t))
        };
        let level_along = ring.level * RING_SWEEP;
        Ends { half, middle, start: at(0.0), end: at(RING_SWEEP), level: at(level_along), level_along }
    }
}

/// The pixel at `p`: `under`, with the ring over it where it is. The ring is a band with round ends
/// against the edge: a dark track (the picture darkened, dark grey at least, to be seen on black) the
/// whole sweep, and persimmon over it up to its level. Its edges are smoothed.
fn compose(p: Polar, under: Rgb565, ring: Look, e: &Ends) -> Rgb565 {
    if ring.width <= 0.0 || p.r < RING_OUTER - ring.width - 1.0 || p.r > RING_OUTER + 1.0 {
        return under;
    }
    let reach = (e.half + 0.5) * (e.half + 0.5);
    let d2 = |(x, y): (f32, f32)| (p.dx - x) * (p.dx - x) + (p.dy - y) * (p.dy - y);
    // How much of the pixel a round-ended arc from the start to `upto` degrees, ending at `end`, covers.
    let cover = |upto: f32, end: (f32, f32)| {
        let d = if (0.0..=upto).contains(&p.along) {
            (p.r - e.middle).abs()
        } else {
            let near = d2(e.start).min(d2(end));
            if near >= reach {
                return 0.0;
            }
            sqrt(near)
        };
        (e.half - d + 0.5).clamp(0.0, 1.0)
    };
    let track = cover(RING_SWEEP, e.end);
    if track <= 0.0 {
        return under;
    }
    let dark = Rgb565::new((under.r() / 4).max(5), (under.g() / 4).max(10), (under.b() / 4).max(5));
    let fill = cover(e.level_along, e.level);
    let shown = mix(under, dark, track);
    if fill > 0.0 { mix(shown, KAKI, fill) } else { shown }
}

/// `a` toward `b` by `t`, 0 to 1.
fn mix(a: Rgb565, b: Rgb565, t: f32) -> Rgb565 {
    let t = (t * 256.0) as i32;
    let m = |a: u8, b: u8| (a as i32 + (((b as i32 - a as i32) * t + 128) >> 8)) as u8;
    Rgb565::new(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}
