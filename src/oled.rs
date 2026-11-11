//! The 0.91" 128×32 SSD1306 OLED on I2C1 (SDA GPIO43, SCK GPIO44): two short lines, centered: what
//! the speaker is doing, or the song. A new line slides in; one too long to fit scrolls. If the OLED does not answer it
//! says so and tries again, and nothing else stops.

use alloc::string::String;

use embassy_futures::select::select;
use embassy_time::{Instant, Timer};
use embedded_graphics::mono_font::iso_8859_2::{FONT_6X10, FONT_7X13};
use embedded_graphics::mono_font::{MonoFont, MonoTextStyle};
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::draw_target::DrawTargetExt;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use embedded_graphics::text::{Baseline, Text};
use esp_hal::i2c::master::{Config, I2c};
use esp_hal::peripherals::{GPIO43, GPIO44, I2C1};
use esp_hal::time::Rate;
use ssd1306::prelude::*;
use ssd1306::{I2CDisplayInterface, Ssd1306Async};

use crate::stream::{Net, State, Stream};

const RETRY_S: u64 = 5;
const WIDTH: i32 = 128;

/// A frame while something moves.
const FRAME_MS: u64 = 30;

/// A new text slides up this far into its line (or in from the right, a new song's), over this long.
const SLIDE_PX: i32 = 16;
const SLIDE_MS: u64 = 320;

/// A line too long to fit waits this long, then scrolls a pixel each this many ms, with this many
/// spaces before it comes round.
const SCROLL_WAIT_MS: u64 = 1200;
const SCROLL_MS_PER_PX: u32 = 30;
const GAP: i32 = 4;

/// How often the progress bar is redrawn while nothing else moves.
const PROGRESS_MS: u64 = 500;

/// The top line, large, and the bottom one, small: font, y, and the band it is kept in.
const TOP: (&MonoFont, i32, Rectangle) = (&FONT_7X13, 2, Rectangle::new(Point::new(0, 0), Size::new(128, 17)));
const BOTTOM: (&MonoFont, i32, Rectangle) = (&FONT_6X10, 20, Rectangle::new(Point::new(0, 17), Size::new(128, 15)));

#[embassy_executor::task]
pub async fn run(i2c: I2C1<'static>, sda: GPIO43<'static>, scl: GPIO44<'static>, stream: &'static Stream) {
    let i2c = match I2c::new(i2c, Config::default().with_frequency(Rate::from_khz(400))) {
        Ok(i2c) => i2c.with_sda(sda).with_scl(scl).into_async(),
        Err(e) => {
            log::error!("oled: i2c: {e:?}; the OLED stays off");
            return;
        }
    };
    let mut display = Ssd1306Async::new(I2CDisplayInterface::new(i2c), DisplaySize128x32, DisplayRotation::Rotate0).into_buffered_graphics_mode();

    loop {
        if let Err(e) = display.init().await {
            log::warn!("oled: no answer at 0x3C ({e:?}); check SDA→GPIO43, SCK→GPIO44, 3V3, GND. Trying again in {RETRY_S} s");
            Timer::after_secs(RETRY_S).await;
            continue;
        }
        log::info!("oled: up");

        let mut shown = [Line::new(), Line::new()];
        let mut was_song = false;
        loop {
            // A new song slides in from the right, the last going out left; anything else rises.
            let (texts, song) = stream.with(|s| (lines(s), s.cast && s.now.is_some()));
            let sideways = song && was_song && texts[0] != shown[0].text;
            was_song = song;
            for (line, text) in shown.iter_mut().zip(texts) {
                line.set(text, sideways);
            }
            display.clear_buffer();
            let mut moving = false;
            for (line, (font, y, band)) in shown.iter().zip([TOP, BOTTOM]) {
                moving |= line.draw(&mut display.clipped(&band), font, y);
            }
            // How far through the song, along the bottom edge.
            let progress = stream.with(|s| s.cast.then(|| s.now.as_ref().and_then(|n| n.progress(s.played))).flatten());
            if let Some(p) = progress {
                let w = (p * WIDTH as f32) as u32;
                // The rest of it dotted, the part played solid.
                _ = display.draw_iter((0..WIDTH).step_by(4).map(|x| Pixel(Point::new(x, 31), BinaryColor::On)));
                _ = Rectangle::new(Point::new(0, 30), Size::new(w, 2)).into_styled(PrimitiveStyle::with_fill(BinaryColor::On)).draw(&mut display);
            }
            if let Err(e) = display.flush().await {
                log::warn!("oled: lost ({e:?}); trying again in {RETRY_S} s");
                Timer::after_secs(RETRY_S).await;
                break;
            }
            if moving {
                _ = select(stream.oled.wait(), Timer::after_millis(FRAME_MS)).await;
            } else if progress.is_some() {
                _ = select(stream.oled.wait(), Timer::after_millis(PROGRESS_MS)).await;
            } else {
                stream.oled.wait().await;
            }
        }
    }
}

/// A line as it moves: a new text slides up in its place, and one too long to fit scrolls.
struct Line {
    text: String,
    old: String,
    since: Instant,
    /// It changes sideways (a new song), else rising.
    sideways: bool,
}

impl Line {
    fn new() -> Line {
        Line { text: String::new(), old: String::new(), since: Instant::now(), sideways: false }
    }

    fn set(&mut self, text: String, sideways: bool) {
        if text != self.text || sideways {
            self.old = core::mem::replace(&mut self.text, text);
            self.since = Instant::now();
            self.sideways = sideways;
        }
    }

    /// Draws it at `y`; whether it still moves.
    fn draw<D: DrawTarget<Color = BinaryColor>>(&self, display: &mut D, font: &MonoFont, y: i32) -> bool {
        let ms = self.since.elapsed().as_millis();
        let t = (ms as f32 / SLIDE_MS as f32).min(1.0);
        let eased = 1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t);
        let mut moving = t < 1.0;
        let scroll = ms.saturating_sub(SCROLL_WAIT_MS) as u32 / SCROLL_MS_PER_PX;
        if self.sideways {
            let shift = (eased * WIDTH as f32) as i32;
            if moving {
                text(display, &self.old, font, -shift, y, 0);
            }
            moving |= text(display, &self.text, font, WIDTH - shift, y, scroll);
        } else {
            let lift = (eased * SLIDE_PX as f32) as i32;
            if moving {
                text(display, &self.old, font, 0, y - lift, 0);
            }
            moving |= text(display, &self.text, font, 0, y + SLIDE_PX - lift, scroll);
        }
        moving
    }
}

/// Draws `s` centered at `y` (moved `dx` px right), or moved left by `scroll` px and coming round again if it is too
/// long; whether it scrolls.
fn text<D: DrawTarget<Color = BinaryColor>>(display: &mut D, s: &str, font: &MonoFont, dx: i32, y: i32, scroll: u32) -> bool {
    let style = MonoTextStyle::new(font, BinaryColor::On);
    let advance = (font.character_size.width + font.character_spacing) as i32;
    let n = s.chars().count() as i32;
    if n * advance <= WIDTH {
        _ = Text::with_baseline(s, Point::new(dx + (WIDTH - n * advance) / 2, y), style, Baseline::Top).draw(display);
        return false;
    }
    let period = (n + GAP) * advance;
    let x = dx - (scroll as i32 % period);
    _ = Text::with_baseline(s, Point::new(x, y), style, Baseline::Top).draw(display);
    _ = Text::with_baseline(s, Point::new(x + period, y), style, Baseline::Top).draw(display);
    true
}

/// The two lines to show.
fn lines(s: &mut State) -> [String; 2] {
    match (&s.net, s.cast, &s.now) {
        (Net::Joining, ..) => ["nori".into(), "connecting to wifi".into()],
        (Net::Setup(name), ..) => ["set me up on wifi".into(), (*name).into()],
        (Net::Ready(name), false, _) => [(*name).into(), "ready to play".into()],
        (Net::Ready(name), true, None) => [(*name).into(), "connected".into()],
        (Net::Ready(_), true, Some(now)) => {
            let at = now.place_ms(s.played) / 1000;
            let length = now.duration_ms / 1000;
            let under = if s.paused { alloc::format!("paused {}:{:02} / {}:{:02}", at / 60, at % 60, length / 60, length % 60) } else { now.artist.clone() };
            [now.title.clone(), under]
        }
    }
}
