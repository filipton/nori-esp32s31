//! nori's deep cast on [`PORT`], one connection at a time (docs/developers/plugins.md, "Deep sinks"):
//! the socket is read always, audio goes into the stream as it comes, and the speaker asks for more
//! with `Want` when it holds under half its depth. Control frames are answered at once.

use alloc::vec::Vec;

use embassy_futures::select::{Either, Either4, select, select4};
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embedded_io_async::Write;
use nori_sink_protocol::{Caps, Codec, Device, Frames, Host, VERSION};

use crate::codec;
use crate::flac;
use crate::psram;
use crate::stream::{self, Kind, Stream};

/// The port the cast comes to, as the announcement says.
pub const PORT: u16 = 47_801;

/// The covers asked for, px square.
const COVER_PX: u32 = 240;

const CAPS: Caps = Caps {
    version: VERSION,
    buffer_bytes: stream::BUFFER_BYTES,
    max_rate: codec::MAX_RATE,
    // 16- and 24-bit.
    bits: 0b11,
    decodes: 1 << 1,
    // For the round screen.
    cover: Some(COVER_PX),
};

#[embassy_executor::task]
pub async fn serve(stack: Stack<'static>, stream: &'static Stream) {
    let mut rx = psram::vec(0u8, 64 * 1024);
    let mut tx = psram::vec(0u8, 4 * 1024);
    let mut chunk = psram::vec(0u8, 8 * 1024);
    let mut out = Vec::new();
    // The format of the encoded audio coming in, to count its packets' frames.
    let mut codec = Codec::Pcm;
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
        if socket.accept(PORT).await.is_err() {
            continue;
        }
        log::info!("a cast from {:?}", socket.remote_endpoint());
        let mut frames = Frames::new();
        let mut hello = false;
        let mut told_volume = false;
        'connection: loop {
            match select4(socket.read(&mut chunk), stream.tell.wait(), stream.presses.receive(), select(stream.knob_volume.wait(), stream.seek.wait())).await {
                Either4::First(Ok(0) | Err(_)) => break,
                Either4::First(Ok(n)) => {
                    frames.push(&chunk[..n]);
                    while let Some(frame) = frames.next_host() {
                        let frame = match frame {
                            Ok(f) => f,
                            Err(e) => {
                                log::warn!("a bad frame: {e:?}");
                                break 'connection;
                            }
                        };
                        if !hello && !matches!(frame, Host::Hello { .. }) {
                            log::warn!("a frame before Hello");
                            break 'connection;
                        }
                        handle(stream, frame, &mut codec, &mut hello, &mut told_volume, &mut out).await;
                    }
                }
                Either4::Second(()) => {}
                Either4::Third(button) => {
                    if hello {
                        Device::Press(button).encode(&mut out);
                    }
                }
                Either4::Fourth(Either::First(gain)) => {
                    if hello {
                        Device::Volume { gain }.encode(&mut out);
                    }
                }
                Either4::Fourth(Either::Second(ms)) => {
                    if hello {
                        log::info!("seek to {}:{:02}", ms / 60_000, ms / 1000 % 60);
                        Device::Seek { ms }.encode(&mut out);
                    }
                }
            }
            if hello && let Some(frames) = stream.with(|s| s.want()) {
                stream.asked(frames);
                Device::Want { frames }.encode(&mut out);
                stream.with(|s| s.status()).encode(&mut out);
            }
            if !out.is_empty() {
                if socket.write_all(&out).await.is_err() {
                    break;
                }
                out.clear();
            }
        }
        log::info!("the cast ended");
        out.clear();
        stream.reset();
        socket.abort();
        let _ = socket.flush().await;
    }
}

/// Takes one frame from the host, putting what it answers in `out`.
async fn handle(stream: &Stream, frame: Host<'_>, codec: &mut Codec, hello: &mut bool, told_volume: &mut bool, out: &mut Vec<u8>) {
    match frame {
        Host::Hello { version } => {
            log::info!("nori's protocol {version}");
            stream.reset();
            stream.with(|s| s.cast = true);
            stream.changed();
            *hello = true;
            Device::Caps(CAPS).encode(out);
        }
        Host::Depth { ms } => {
            stream.set_depth(ms);
            stream.with(|s| s.status()).encode(out);
        }
        Host::Format(f) => {
            *codec = f.codec;
            // nori sends the volume keys here once it knows a volume, which it hears only while
            // the cast runs: the plugin starts that after `Hello`, and audio comes after it.
            if !core::mem::replace(told_volume, true) {
                Device::Volume { gain: stream.with(|s| s.volume) }.encode(out);
            }
            stream.put_marker(Kind::Format, &stream::encode_format(&f)).await;
        }
        Host::Audio(samples) => {
            let frame = stream.with(|s| s.incoming_frame_bytes());
            stream.put_audio(Kind::Pcm, (samples.len() / frame.max(1)) as u32, samples).await;
        }
        Host::Encoded(packet) => {
            let frames = flac::frames(packet);
            match frames {
                Some(n) => stream.put_audio(Kind::Encoded, n, packet).await,
                None => log::warn!("{codec:?}: a packet not understood"),
            }
        }
        Host::Gain { db } => stream.put_marker(Kind::Gain, &db.to_le_bytes()).await,
        Host::Flush { seq, keep } => {
            log::info!("flush {seq} (keep {keep:?}) at {} ms", embassy_time::Instant::now().as_millis());
            stream.flush(keep);
            let played = stream.with(|s| s.played);
            Device::Flushed { seq, played }.encode(out);
            stream.with(|s| s.status()).encode(out);
        }
        Host::Pause | Host::Resume => {
            stream.with(|s| s.paused = matches!(frame, Host::Pause));
            stream.woke.signal(());
            stream.changed();
            stream.with(|s| s.status()).encode(out);
        }
        Host::Ramp { target, ms } => {
            log::info!("fade to {:.0} % over {ms} ms at {} ms", target * 100.0, embassy_time::Instant::now().as_millis());
            stream.with(|s| s.ramp = Some((target.clamp(0.0, 1.0), ms)));
        }
        Host::Volume { gain } => {
            log::info!("volume {:.0} %", gain * 100.0);
            stream.with(|s| s.volume = gain.clamp(0.0, 1.0));
            stream.volume_set.signal(());
            stream.changed();
        }
        Host::Cover { id, jpeg } => {
            if stream.with(|s| s.covers.keep(id, jpeg)) {
                log::info!("cover {id}: {} bytes", jpeg.len());
            } else {
                log::warn!("cover {id}: {} bytes is too big to keep", jpeg.len());
            }
        }
        Host::Now { id, title, artist, album, duration_ms, cover, at_ms } => {
            let now = stream::encode_now(&stream::Now { id, title, artist, album, duration_ms, cover, at_ms });
            stream.put_marker(Kind::Now, &now).await;
        }
    }
}
