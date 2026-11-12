# nori-esp32s31

A nori speaker on the ESP32-S31-Function-CoreBoard-1. It joins the network, announces itself over mDNS
as `_nori-speaker._tcp` and plays what a nori cast plugin sends it through the board's ES8311 codec
and speaker amplifier. nori does the playing (the queue, the sound chain, AutoMix); the speaker holds tens of
seconds, decodes FLAC and MP3 when nori sends a file as it is, and plays.

## Building and flashing

Nightly, `riscv32imafc-unknown-none-elf`:

```sh
cargo build --release
cargo run --release          # flashes over the board's USB and monitors it (espflash)
```

It patches two dependencies for now (`[patch.crates-io]` in Cargo.toml):

- The S31's I2S driver (esp-hal #6197) and its BLE controller are only on esp-hal's main branch, so the
  esp-hal crates come from one commit of it.
- esp-hal-wifimanager's `init_wm_with`, and BLE setup on the S31, come from its master branch until a
  release has them.

## Setting it up

On first boot it offers its setup two ways until a network is saved: an access point, `nori-<mac>`,
with a setup page, and a BLE setup service (esp-hal-wifimanager's). Both take the network and a `data`
field with the name nori's clients list it as:

```json
{"name":"Kitchen"}
```

## The stream

It speaks nori's deep cast sink protocol on port 47801, one connection at a time: the frames are
`nori-sink-protocol` (`crates/sink-protocol` on nori's master, pinned by Cargo.lock), the protocol is in
nori's `docs/developers/plugins.md`, "Deep sinks".

- It answers `Hello` with what it can do: a 12 MB buffer in PSRAM, up to 44.1 kHz, 16- and 24-bit PCM,
  FLAC and MP3 decoded here (so nori sends the file's own packets when nothing touches the samples),
  covers at 240 px.
- It reads its socket always and asks for audio with `Want` once it holds under half the depth nori set,
  then nothing comes until the next: the phone's radio sleeps between bursts.
- What comes is kept in order with the stream frame each record starts at, so a format, ReplayGain or
  now playing applies exactly where it was placed, and a flush cuts what is held at nori's frame.
- Everything plays as 32-bit stereo, scaled by fade × volume × ReplayGain, and bit for bit when that
  is one. Playing starts once 300 ms are in.
- The BOOT button is play-pause in nori. Now playing is logged and shown on the screens.

The DMA ring after the buffer (about 135 ms at 44.1 kHz) is lost on a pause; nori fades out before one.

nori resamples songs above 44.1 kHz. At 47 and 48 kHz this board sounds harsh, though the frame rate is exact.
MCLK from the audio PLL (which esp-hal doesn't model on the S31) was no better than from the crystal, nor was any
DAC oversampling tried, so the cause isn't known.

## Checks

The parts that need no chip (the stream, the FLAC and MP3 decoders) are tested on the host. The FLAC
check needs `flac` on the PATH, the MP3 one `ffmpeg`:

```sh
cd check && cargo test --release --target x86_64-unknown-linux-gnu
```

`check/send.py <host>` plays test tones to a running speaker over the real protocol, without nori: PCM,
FLAC and MP3, then pause, resume and a seek, printing the rate frames played at from each `Status`.

## Screens and knob

All on the J2 header:

| Part | Pin | GPIO |
|---|---|---|
| GC9A01 240×240 (SPI) | SCL / SDA / DC / CS / RST | 20 / 21 / 22 / 23 / 24 |
| SSD1306 128×32 OLED (I2C) | SDA / SCK | 43 / 44 |
| EC11 encoder | A / B / switch | 47 / 48 / 49 (common to GND) |

The round screen shows the song's cover (its middle square, scaled to fit), and nori's wordmark on
black when nothing plays; a new picture opens from the center. For a moment after the volume changes,
from the knob or from nori, a thin ring round the edge shows it. The OLED shows two short lines: the
setup's network name while it waits to be set up, the speaker's name once ready, then the song (a long
title scrolls). While a cast is connected the knob is the volume (nori hears it as the device's own), its
press is play-pause, turned with the button held ten clicks is the next song (or back), and turned
while paused it seeks 5 s a click (`Seek`, nori's sink protocol 3). The volume is set in the codec, so it is heard at once, and kept across
restarts (in the wifimanager's NVS, a moment after it last changed). While paused the cover shrinks
to a smaller circle.

The screens run on the second core, the round one's pixels going out by DMA while the next are made;
the knob runs in an interrupt executor so a quick turn's steps are not lost. The playing bar uses
`Now`'s `at_ms` (nori's sink protocol version 2), so it is right after a seek.

The wordmark is `assets/logo.rgb565`, made from nori's `docs/brand/nori-wordmark.svg` by
`tools/logo.py` (needs `rsvg-convert` and Pillow).

## The button

BOOT is play-pause; held for 600 ms it is the next song. It works during a cast: nori takes `Press` as a
media button, so no plugin code is involved.

## Covers

The speaker asks for 240 px covers and keeps the last 32, 64 KB each at most, in PSRAM (`Covers` in
`src/stream.rs`). The serial console prints each cover as it comes and, where each song starts, its title,
artist, album, length and cover. nori's cover is sent only once it is in nori's cache, so the first song
of an album played for the first time often has none (the round screen shows "nori" then), and Android
doesn't send covers yet.

## The plugin

`speaker` in [nori-plugins](https://github.com/norifm/nori-plugins) casts to the speaker: install it from nori's plugin list. It finds speakers over mDNS while nori's devices list is open, and adds a row there that starts and stops the cast, to the speaker found last.

## The board

| | GPIO |
|---|---|
| I2S MCLK, BCLK, WS, data to the codec | 52, 53, 55, 56 |
| ES8311 I2C SCL, SDA (address 0x18) | 50, 51 |
| NS4150B amplifier enable (high on) | 57 |
| BOOT button (low pressed) | 61 |

The codec runs as I2S slave with MCLK at 256 times the sample rate and 32-bit slots, and answers on I2C
only while MCLK runs. Its register sequence is Espressif's es8311 driver's.
