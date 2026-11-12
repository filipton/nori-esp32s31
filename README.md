# nori-esp32s31

A [nori](https://github.com/norifm/nori) speaker on the ESP32-S31-Function-CoreBoard-1. It plays what
nori casts to it (PCM, FLAC, MP3) through the board's ES8311 codec, and shows the cover on a round screen.

## Build

```sh
cargo run --release   # builds, flashes and monitors
```

Nightly. esp-hal and esp-hal-wifimanager come from git for now (`[patch.crates-io]`).

## Set up

On first boot it opens an access point, `nori-<mac>`, and a BLE setup. Give it your network and a name:

```json
{"name":"NoriHW"}
```

Then cast to it with the `speaker` plugin from [nori-plugins](https://github.com/norifm/nori-plugins).

## Wiring

| Part | GPIO |
|---|---|
| GC9A01 240×240: SCL, SDA, DC, CS, RST | 20, 21, 22, 23, 24 |
| SSD1306 128×32: SDA, SCK | 43, 44 |
| EC11 knob: A, B, switch | 47, 48, 49 |

The knob is the volume; press for play-pause, hold and turn to skip, turn while paused to seek.
BOOT is play-pause, held for next.

## Checks

```sh
cd check && cargo test --release --target x86_64-unknown-linux-gnu   # needs flac and ffmpeg
python3 check/send.py <host>                                         # test tones to a running speaker
```
