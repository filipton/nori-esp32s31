//! The volume, kept in NVS (the wifimanager's, in its namespace) across restarts: written a moment
//! after it last changed, from the knob or from nori, so a turn of the knob is one write.

use embassy_futures::select::{Either, select};
use embassy_time::Timer;
use esp_hal_wifimanager::{Key, Nvs};

use crate::stream::Stream;

const VOLUME: Key = Key::from_str("volume");

/// How long the volume rests before it is written.
const SETTLE_MS: u64 = 2000;

/// The volume kept, linear 0 to 1, if one is.
pub async fn volume(nvs: &Nvs) -> Option<f32> {
    let kept: u16 = nvs.get(&VOLUME).await.ok()?;
    Some((kept as f32 / 1000.0).clamp(0.0, 1.0))
}

#[embassy_executor::task]
pub async fn keep(nvs: &'static Nvs, stream: &'static Stream) {
    loop {
        stream.volume_set.wait().await;
        while let Either::First(()) = select(stream.volume_set.wait(), Timer::after_millis(SETTLE_MS)).await {}
        let volume = stream.with(|s| s.volume);
        match nvs.set(&VOLUME, (volume * 1000.0) as u16).await {
            Ok(()) => log::info!("volume {:.0} % kept", volume * 100.0),
            Err(e) => log::warn!("keeping the volume: {e:?}"),
        }
    }
}
