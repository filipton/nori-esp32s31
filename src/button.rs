//! The board's BOOT button (GPIO61): a press is play-pause in nori, a long press the next song.

use embassy_futures::select::{Either, select};
use embassy_time::Timer;
use esp_hal::gpio::{Input, InputConfig, Pull};
use esp_hal::peripherals::GPIO61;
use nori_sink_protocol::Button;

use crate::stream::Stream;

const LONG_MS: u64 = 600;

#[embassy_executor::task]
pub async fn watch(pin: GPIO61<'static>, stream: &'static Stream) {
    let mut button = Input::new(pin, InputConfig::default().with_pull(Pull::Up));
    loop {
        button.wait_for_falling_edge().await;
        // Its contacts bounce.
        Timer::after_millis(30).await;
        if button.is_low() {
            let pressed = match select(button.wait_for_high(), Timer::after_millis(LONG_MS)).await {
                Either::First(()) => Button::PlayPause,
                Either::Second(()) => Button::Next,
            };
            log::info!("pressed: {pressed:?}");
            let _ = stream.presses.try_send(pressed);
            button.wait_for_high().await;
        }
    }
}
