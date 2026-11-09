//! The stream the speaker holds: records in, played out in order, flushed as nori asks, and the
//! credit asked for.

use embassy_futures::block_on;
use nori_esp32s31_check::light::Mood;
use nori_esp32s31_check::stream::{self, Kind, Stream};
use nori_sink_protocol::{Codec, Format};

const PCM: Format = Format { rate: 48_000, channels: 2, bits: 16, codec: Codec::Pcm };
const FLAC: Format = Format { rate: 48_000, channels: 2, bits: 0, codec: Codec::Flac };

/// 16-bit stereo frames `from..from + n`, each holding its stream frame number.
fn pcm(from: u32, n: u32) -> Vec<u8> {
    (from..from + n).flat_map(u32::to_le_bytes).collect()
}

fn format(s: &Stream, f: Format) {
    block_on(s.put_marker(Kind::Format, &stream::encode_format(&f)));
}

fn put(s: &Stream, from: u32, n: u32) {
    block_on(s.put_audio(Kind::Pcm, n, &pcm(from, n)));
}

/// What a player hears of up to `frames`: PCM frames' numbers, an encoded record as its start and
/// skip, and the markers.
#[derive(Debug, PartialEq)]
enum Heard {
    Pcm(u32),
    Encoded { start: u64, skip: u32 },
    Marker(Kind),
}

fn play(s: &Stream, mut frames: u32) -> Vec<Heard> {
    let mut heard = Vec::new();
    s.with(|st| {
        while let Some((at, r)) = st.first() {
            if !matches!(r.kind, Kind::Pcm | Kind::Encoded) {
                heard.push(Heard::Marker(r.kind));
                st.pop();
                continue;
            }
            if frames == 0 {
                break;
            }
            let n = (r.frames - st.consumed).min(frames);
            if r.kind == Kind::Pcm {
                let from = st.consumed as usize * 4;
                for f in st.payload(at, &r)[from..from + n as usize * 4].chunks(4) {
                    heard.push(Heard::Pcm(u32::from_le_bytes([f[0], f[1], f[2], f[3]])));
                }
            } else if st.consumed == 0 {
                heard.push(Heard::Encoded { start: r.start, skip: r.skip });
            }
            st.played += n as u64;
            st.consumed += n;
            frames -= n;
            if st.consumed == r.frames {
                st.pop();
            }
        }
    });
    heard
}

fn pcm_heard(from: u32, to: u32) -> Vec<Heard> {
    (from..to).map(Heard::Pcm).collect()
}

#[test]
fn audio_plays_in_order_through_the_ring_wrapping_round() {
    let s = Stream::new();
    format(s, PCM);
    assert_eq!(play(s, 0), vec![Heard::Marker(Kind::Format)]);
    // 40 MB through a 12 MB ring, in records of odd sizes.
    let mut next = 0;
    let mut heard = 0;
    while next < 10_000_000 {
        let n = 7_919 + next % 50_000;
        put(s, next, n);
        next += n;
        if next - heard > 2_000_000 {
            let got = play(s, 1_000_000);
            assert_eq!(got, pcm_heard(heard, heard + 1_000_000));
            heard += 1_000_000;
        }
    }
    assert_eq!(play(s, u32::MAX), pcm_heard(heard, next));
    assert!(s.with(|st| st.first().is_none()));
}

#[test]
fn a_flush_keeps_what_is_held_before_its_frame() {
    let s = Stream::new();
    format(s, PCM);
    for i in 0..10 {
        put(s, i * 1000, 1000);
    }
    play(s, 1500);
    // The music made again from 4500 on.
    s.flush(Some(4500));
    format(s, PCM);
    put(s, 4500, 1000);
    let mut want = pcm_heard(1500, 4500);
    want.push(Heard::Marker(Kind::Format));
    want.extend(pcm_heard(4500, 5500));
    assert_eq!(play(s, u32::MAX), want);
}

#[test]
fn a_flush_at_a_record_boundary_drops_the_markers_there() {
    let s = Stream::new();
    format(s, PCM);
    put(s, 0, 1000);
    format(s, PCM);
    put(s, 1000, 1000);
    s.flush(Some(1000));
    let mut want = vec![Heard::Marker(Kind::Format)];
    want.extend(pcm_heard(0, 1000));
    assert_eq!(play(s, u32::MAX), want);
}

#[test]
fn a_seek_drops_everything_not_played() {
    let s = Stream::new();
    format(s, PCM);
    put(s, 0, 5000);
    play(s, 1200);
    s.flush(None);
    assert_eq!(play(s, u32::MAX), vec![]);
    // The new audio counts on from what played.
    format(s, PCM);
    put(s, 1200, 100);
    let mut want = vec![Heard::Marker(Kind::Format)];
    want.extend(pcm_heard(1200, 1300));
    assert_eq!(play(s, u32::MAX), want);
    assert_eq!(s.with(|st| st.played), 1300);
}

#[test]
fn a_flush_back_past_what_played_drops_the_audio_again_up_to_it() {
    let s = Stream::new();
    format(s, PCM);
    put(s, 0, 10_000);
    play(s, 6000);
    // The host makes the music again from 4000, which already played.
    s.flush(Some(4000));
    format(s, PCM);
    put(s, 4000, 1000);
    put(s, 5000, 1500);
    let mut want = vec![Heard::Marker(Kind::Format)];
    want.extend(pcm_heard(6000, 6500));
    assert_eq!(play(s, u32::MAX), want);

    // An encoded packet across the frame played skips what played of it.
    play(s, 0);
    s.flush(Some(6000));
    s.flush(Some(5000));
    format(s, FLAC);
    block_on(s.put_audio(Kind::Encoded, 0, b"fLaC"));
    block_on(s.put_audio(Kind::Encoded, 1024, b"frame"));
    block_on(s.put_audio(Kind::Encoded, 1024, b"frame"));
    let heard = play(s, u32::MAX);
    assert_eq!(
        heard,
        vec![Heard::Marker(Kind::Format), Heard::Encoded { start: 5000, skip: 0 }, Heard::Encoded { start: 6500, skip: 476 }],
        "the head (no frames) stays for the decoder; the packet that ended before is dropped"
    );
}

#[test]
fn it_asks_for_what_fills_its_depth_once_under_half() {
    let s = Stream::new();
    format(s, PCM);
    assert_eq!(s.with(|st| st.want()), None, "nothing before a depth");
    s.set_depth(1000);
    assert_eq!(s.with(|st| st.want()), Some(48_000));
    s.asked(48_000);
    assert_eq!(s.with(|st| st.want()), None, "one at a time");
    put(s, 0, 30_000);
    assert_eq!(s.with(|st| st.want()), None);
    put(s, 30_000, 18_000);
    play(s, 20_000);
    assert_eq!(s.with(|st| st.want()), None, "over half");
    play(s, 5_000);
    assert_eq!(s.with(|st| st.want()), Some(25_000));
    s.asked(25_000);
    // The host had only some: once over half and under again, it asks again.
    put(s, 48_000, 10_000);
    play(s, 20_000);
    assert_eq!(s.with(|st| st.want()), Some(35_000));
    // A flush forgets the credit.
    s.asked(35_000);
    s.flush(None);
    assert_eq!(s.with(|st| st.want()), Some(48_000));
}

#[test]
fn a_song_keeps_its_details_through_the_stream() {
    let s = Stream::new();
    for cover in [None, Some(7)] {
        let now = stream::Now { id: 3, title: "Blue", artist: "Ä", album: "", duration_ms: 205_000, cover, at_ms: 70_000 };
        block_on(s.put_marker(Kind::Now, &stream::encode_now(&now)));
        s.with(|st| {
            let (at, r) = st.first().unwrap();
            assert_eq!(stream::decode_now(st.payload(at, &r)), Some(now));
            st.pop();
        });
    }
}

#[test]
fn covers_stay_until_32_newer_came_or_the_cast_ended() {
    let s = Stream::new();
    let jpeg = |id: u32| vec![id as u8; 1000 + id as usize];
    s.with(|st| {
        for id in 0..33 {
            assert!(st.covers.keep(id, &jpeg(id)));
        }
        assert_eq!(st.covers.get(0), None, "the oldest went");
        assert_eq!(st.covers.get(1), Some(&jpeg(1)[..]));
        assert_eq!(st.covers.get(32), Some(&jpeg(32)[..]));
        assert!(!st.covers.keep(99, &vec![0; 64 * 1024 + 1]));
        assert_eq!(st.covers.get(99), None);
    });
    s.reset();
    assert_eq!(s.with(|st| st.covers.get(32).map(<[u8]>::len)), None);
}

#[test]
fn the_mood_follows_the_cast_and_the_light_the_volume() {
    let s = Stream::new();
    assert_eq!(s.with(|st| st.mood()), Mood::Idle);
    s.with(|st| st.cast = true);
    assert_eq!(s.with(|st| st.mood()), Mood::Waiting);
    s.playing(true);
    assert_eq!(s.with(|st| st.mood()), Mood::Playing);
    s.with(|st| st.paused = true);
    assert_eq!(s.with(|st| st.mood()), Mood::Paused);
    s.reset();
    assert_eq!(s.with(|st| st.mood()), Mood::Idle);

    let green = |v| Mood::Playing.color(v)[1];
    assert!(green(0.0) > 0 && green(0.0) < green(0.5) && green(0.5) < green(1.0));
    assert_eq!(Mood::Idle.color(0.0), Mood::Idle.color(1.0));
}
