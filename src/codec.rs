//! The board's sound: I2S into the ES8311 codec (its MCLK from the chip at 256 times the sample rate)
//! and the NS4150B amplifier on the speaker header. Pins as the board's schematic has them. Every
//! source goes out as 32-bit stereo, so 16- and 24-bit songs play unchanged in one format.

use esp_hal::Async;
use esp_hal::Blocking;
use esp_hal::dma::DmaTxStreamBuf;
use esp_hal::gpio::{Level, OutputConfig};
use esp_hal::i2c::master::I2c;
use esp_hal::i2s::master::{Channels, DataFormat, I2s, I2sTx, I2sTxDmaTransfer, TdmConfig, UnitConfig};
use esp_hal::peripherals::{DMA_CH0, GPIO50, GPIO51, GPIO52, GPIO53, GPIO55, GPIO56, GPIO57, I2C0, I2S0};
use esp_hal::time::Rate;
use embassy_futures::select::{Either, select};
use embassy_time::Timer;

/// The ES8311's I2C address (its CE pin low).
const ES8311: u8 = 0x18;

/// The rate the output starts at; a song at another rate changes it. The codec needs no change.
const FIRST_RATE: u32 = 44_100;

/// The highest rate played: nori resamples what is above this. At 47 and 48 kHz this board sounds
/// harsh though the frame rate is exact, from the crystal or the audio PLL (tried, no better) and at
/// any DAC oversampling tried; the cause is not known.
pub const MAX_RATE: u32 = 44_100;

/// Bytes per frame sent: two 32-bit samples.
pub const FRAME: usize = 8;

/// The DMA ring the samples go out of: about 135 ms at 44.1 kHz, what the player may be late by. What it holds is lost on a pause.
const RING: usize = 48_000;

/// Longer than a descriptor of the ring takes to play at the lowest rate.
const RING_MS: u64 = 500;

/// Silence a transfer starts with, in pieces of this: enough for a FLAC block to decode before the
/// first samples are due. The DMA also reads a descriptor ahead, and ends at one it read as the last
/// even if more were queued after it. With none, a transfer plays what the ring last held.
const LEAD: [u8; 4_000] = [0; 4_000];
const LEAD_PIECES: usize = 8;

pub struct Pins {
    pub i2s: I2S0<'static>,
    pub dma: DMA_CH0<'static>,
    pub i2c: I2C0<'static>,
    pub mclk: GPIO52<'static>,
    pub bclk: GPIO53<'static>,
    pub ws: GPIO55<'static>,
    pub dout: GPIO56<'static>,
    pub scl: GPIO50<'static>,
    pub sda: GPIO51<'static>,
    /// The amplifier's enable, high on.
    pub amp: GPIO57<'static>,
}

type Transfer = I2sTxDmaTransfer<'static, Async, DmaTxStreamBuf>;

/// The I2S output: idle (the transmitter and its ring), or sending.
pub struct Output {
    idle: Option<(I2sTx<'static, Async>, DmaTxStreamBuf)>,
    sending: Option<Transfer>,
    rate: u32,
    _amp: esp_hal::gpio::Output<'static>,
    i2c: I2c<'static, Blocking>,
    /// The volume asked for, linear, and the DAC volume register as last written.
    volume: f32,
    written: Option<u8>,
}

fn unit(rate: u32) -> UnitConfig {
    UnitConfig::new_tdm_philips().with_sample_rate(Rate::from_hz(rate)).with_data_format(DataFormat::Data32Channel32).with_channels(Channels::STEREO)
}

impl Output {
    pub async fn new(p: Pins) -> Output {
        let config = TdmConfig::new_tdm_philips().with_sample_rate(Rate::from_hz(FIRST_RATE)).with_data_format(DataFormat::Data32Channel32).with_channels(Channels::STEREO);
        let i2s = I2s::new(p.i2s, p.dma, config).expect("i2s").into_async().with_mclk(p.mclk);
        let tx = i2s.i2s_tx.with_bclk(p.bclk).with_ws(p.ws).with_dout(p.dout).build();
        let ring = esp_hal::dma_tx_stream_buffer!(RING, 4000);
        let i2c = I2c::new(p.i2c, esp_hal::i2c::master::Config::default()).expect("i2c").with_sda(p.sda).with_scl(p.scl);
        let amp = esp_hal::gpio::Output::new(p.amp, Level::Low, OutputConfig::default());
        let mut out = Output { idle: Some((tx, ring)), sending: None, rate: FIRST_RATE, _amp: amp, i2c, volume: 1.0, written: Some(0xBF) };

        // The codec answers on I2C only with MCLK running, which runs while samples go out.
        out.start(FIRST_RATE);
        if let Err(e) = start_codec(&mut out.i2c).await {
            log::error!("es8311: {e:?}");
        }
        out.stop();
        out._amp.set_high();
        out
    }

    /// Sends at `rate` from now on; at the rate already sending, the music goes on gaplessly.
    pub fn start(&mut self, rate: u32) {
        if self.sending.is_some() && rate == self.rate {
            return;
        }
        self.stop();
        log::info!("output starts at {rate} Hz at {} ms", embassy_time::Instant::now().as_millis());
        let Some((mut tx, mut ring)) = self.idle.take() else { return };
        if rate != self.rate {
            match tx.apply_config(&unit(rate)) {
                Ok(()) => self.rate = rate,
                Err(e) => log::warn!("i2s: {rate} Hz: {e:?}"),
            }
        }
        for _ in 0..LEAD_PIECES {
            ring.push(&LEAD);
        }
        match tx.write(ring) {
            Ok(transfer) => {
                self.sending = Some(transfer);
                self.write_volume();
            }
            Err((e, tx, ring)) => {
                log::error!("i2s: {e:?}");
                self.idle = Some((tx, ring));
            }
        }
    }

    /// Sets the volume, linear 0 to 1, in the codec's DAC: heard at once, not after the ring. The
    /// codec answers only while samples go out, so a volume set while stopped is written at the
    /// next start.
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume;
        self.write_volume();
    }

    fn write_volume(&mut self) {
        let reg = dac_volume(self.volume);
        if self.sending.is_none() || self.written == Some(reg) {
            return;
        }
        match self.i2c.write(ES8311, &[0x32, reg]) {
            Ok(()) => self.written = Some(reg),
            Err(e) => log::warn!("es8311: volume: {e:?}"),
        }
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn sending(&self) -> bool {
        self.sending.is_some()
    }

    /// Sends `samples` (32-bit little-endian stereo), waiting while the ring is full. If the ring ran
    /// out the transfer has ended: it stops, and [`Self::sending`] says so.
    pub async fn push(&mut self, mut samples: &[u8]) {
        let Some(transfer) = self.sending.as_mut() else { return };
        while !samples.is_empty() {
            let n = transfer.push(samples);
            samples = &samples[n..];
            if n == 0 {
                // A transfer that ended while nothing waited on it wakes nobody: the ring takes
                // less than this to play.
                let woke = select(transfer.wait_for_available_async(), Timer::after_millis(RING_MS)).await;
                if !matches!(woke, Either::First(Ok(()))) {
                    log::warn!("the output ran out at {} ms", embassy_time::Instant::now().as_millis());
                    self.stop();
                    return;
                }
            }
        }
    }

    /// What the ring holds yet to be heard, µs.
    pub fn delay_us(&mut self) -> u32 {
        let Some(transfer) = self.sending.as_mut() else { return 0 };
        let held = RING.saturating_sub(transfer.available_bytes()) / FRAME;
        (held as u64 * 1_000_000 / self.rate as u64) as u32
    }

    /// Sends silence for as long as the ring holds, so the music before it is heard out.
    pub async fn drain(&mut self) {
        let silence = [0u8; 1000];
        for _ in 0..RING / silence.len() {
            self.push(&silence).await;
        }
    }

    pub fn stop(&mut self) {
        if let Some(transfer) = self.sending.take() {
            log::info!("output stops at {} ms", embassy_time::Instant::now().as_millis());
            self.idle = Some(transfer.stop());
        }
    }
}

/// The ES8311 as I2S slave, clocked from MCLK at 256 fs, 32-bit slots, the DAC on at 0 dB: Espressif's
/// es8311 driver's sequence.
async fn start_codec(i2c: &mut I2c<'static, Blocking>) -> Result<(), esp_hal::i2c::master::Error> {
    i2c.write(ES8311, &[0x00, 0x1F])?;
    Timer::after_millis(20).await;
    for (reg, value) in [
        (0x00, 0x00),
        // Powered, slave.
        (0x00, 0x80),
        // Every clock on, MCLK from its pin.
        (0x01, 0x3F),
        // Dividers for MCLK = 256 fs: pre-divider 1, ADC and DAC dividers 1, oversampling 0x10.
        (0x02, 0x00),
        (0x05, 0x00),
        (0x03, 0x10),
        (0x04, 0x10),
        (0x06, 0x03),
        (0x07, 0x00),
        (0x08, 0xFF),
        // Serial ports: I2S, 32-bit words.
        (0x09, 0x10),
        (0x0A, 0x10),
        (0x0B, 0x00),
        (0x0C, 0x00),
        (0x10, 0x1F),
        (0x11, 0x7F),
        // Analog on, DAC on, the output driver on.
        (0x0D, 0x01),
        (0x0E, 0x02),
        (0x12, 0x00),
        (0x13, 0x10),
        (0x1C, 0x6A),
        (0x37, 0x08),
        // DAC volume 0 dB, unmuted; the user's volume is set here later (`Output::set_volume`).
        (0x32, 0xBF),
        (0x31, 0x00),
    ] {
        i2c.write(ES8311, &[reg, value])?;
    }
    Ok(())
}

/// The ES8311's DAC volume register for a linear volume: 0xBF is 0 dB, each step 0.5 dB down, 0
/// silent.
fn dac_volume(volume: f32) -> u8 {
    if volume <= 0.0 {
        return 0;
    }
    let db = 20.0 * libm::log10f(volume.min(1.0));
    libm::roundf(191.0 + 2.0 * db).clamp(0.0, 191.0) as u8
}
