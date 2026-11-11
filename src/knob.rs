//! The EC11 rotary encoder: A on GPIO47, B on GPIO48, its switch on GPIO49, each pulled up, common to
//! ground. While a cast is connected a turn is the volume (told to nori as the device's own knob) and
//! a press is play-pause; turned with the button held, ten clicks clockwise is the next song and
//! back the previous (nori's: the song's start first); turned while paused it seeks. Otherwise they
//! are only logged. It runs in an interrupt executor, ahead of
//! the audio and the network, so a quick turn's edges are not missed.

use embassy_futures::select::{Either, select, select3};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::gpio::{Input, InputConfig, Pull};
use esp_hal::peripherals::{GPIO47, GPIO48, GPIO49};
use nori_sink_protocol::Button;

use crate::stream::Stream;

/// Steps of the quadrature code per detent: 4 on most EC11s.
const STEPS: i8 = 4;

/// Clicks turned with the button held for a skip.
const SKIP_CLICKS: i32 = 10;
/// A click turned while paused moves the place in the song this far, and nori is told where it
/// rests once the knob has been still this long.
const SEEK_STEP_MS: i64 = 5000;
const SEEK_SETTLE_MS: u64 = 300;

/// The volume a detent moves, linear.
const VOLUME_STEP: f32 = 0.025;

/// The step a change from one A/B state to the next is (index `previous << 2 | now`); 0 for none or a bounce.
const STEP: [i8; 16] = [0, -1, 1, 0, 1, 0, 0, -1, -1, 0, 0, 1, 0, 1, -1, 0];

#[embassy_executor::task]
pub async fn watch(a: GPIO47<'static>, b: GPIO48<'static>, sw: GPIO49<'static>, stream: &'static Stream) {
    let config = InputConfig::default().with_pull(Pull::Up);
    let mut a = Input::new(a, config);
    let mut b = Input::new(b, config);
    let mut sw = Input::new(sw, config);
    // Read B before A: as wired, that makes clockwise up.
    let state = |a: &Input, b: &Input| (b.is_high() as u8) << 1 | a.is_high() as u8;

    let mut last = state(&a, &b);
    let mut steps = 0i8;
    let mut direction = 0i8;
    let mut pressed = sw.is_low();
    let mut pressed_at = Instant::now();
    // Clicks turned with the button held, toward a skip.
    let mut held_clicks = 0i32;
    log::info!("knob: ready");

    loop {
        select3(a.wait_for_any_edge(), b.wait_for_any_edge(), sw.wait_for_any_edge()).await;

        let now = state(&a, &b);
        if now != last {
            let step = STEP[(last << 2 | now) as usize];
            // Both changed since the last look: two steps the way it was turning.
            steps += if step == 0 { 2 * direction } else { step };
            if step != 0 {
                direction = step;
            }
            last = now;
            if steps.abs() >= STEPS {
                let turn = steps.signum() as f32;
                steps = 0;
                if pressed {
                    // A skip takes SKIP_CLICKS the same way; turning back starts again.
                    let turn = turn as i32;
                    if held_clicks.signum() != turn {
                        held_clicks = 0;
                    }
                    held_clicks += turn;
                    if held_clicks.abs() >= SKIP_CLICKS {
                        held_clicks = 0;
                        press(stream, if turn > 0 { Button::Next } else { Button::Previous });
                    }
                } else if !seek(stream, turn) {
                    turned(stream, turn);
                }
            }
        }

        // The switch bounces: a change within 20 ms of the last is ignored.
        let down = sw.is_low();
        if down != pressed && pressed_at.elapsed() > Duration::from_millis(20) {
            pressed = down;
            pressed_at = Instant::now();
            // Play-pause at once; turned while held, it skips as well (a skip plays).
            if down {
                held_clicks = 0;
                press(stream, Button::PlayPause);
            }
        }
    }
}

fn turned(stream: &Stream, turn: f32) {
    let volume = stream.with(|s| {
        s.cast.then(|| {
            s.volume = (s.volume + turn * VOLUME_STEP).clamp(0.0, 1.0);
            s.volume
        })
    });
    match volume {
        Some(volume) => {
            log::info!("knob: volume {:.0} %", volume * 100.0);
            stream.knob_volume.signal(volume);
            stream.volume_set.signal(());
            stream.changed();
        }
        None => log::info!("knob: {} (no cast)", if turn > 0.0 { "right" } else { "left" }),
    }
}

/// A click turned while a song is paused: its place moves, shown at once; false if nothing is paused,
/// and the click is the volume.
fn seek(stream: &Stream, turn: f32) -> bool {
    let to = stream.with(|s| {
        let played = s.played;
        if !(s.cast && s.paused) {
            return None;
        }
        let now = s.now.as_mut()?;
        let end = now.duration_ms.saturating_sub(1000) as i64;
        let to = (now.place_ms(played) as i64 + turn as i64 * SEEK_STEP_MS).clamp(0, end.max(0)) as u32;
        (now.at_ms, now.from) = (to, played);
        Some(to)
    });
    let Some(to) = to else { return false };
    log::info!("knob: to {}:{:02}", to / 60_000, to / 1000 % 60);
    stream.seek_moved.signal(to);
    stream.changed();
    true
}

/// Tells nori where the knob left the place in the song, once it has been still a moment: one seek
/// for a turn of many clicks.
#[embassy_executor::task]
pub async fn settle(stream: &'static Stream) {
    loop {
        let mut to = stream.seek_moved.wait().await;
        while let Either::First(moved) = select(stream.seek_moved.wait(), Timer::after_millis(SEEK_SETTLE_MS)).await {
            to = moved;
        }
        stream.seek.signal(to);
    }
}

/// The button `button` pressed in nori, if a cast is connected.
fn press(stream: &Stream, button: Button) {
    if !stream.with(|s| s.cast) {
        log::info!("knob: {button:?} (no cast)");
        return;
    }
    log::info!("knob: {button:?}");
    let _ = stream.presses.try_send(button);
}
