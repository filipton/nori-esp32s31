#!/usr/bin/env python3
"""Plays test tones to a speaker over the deep sink protocol, as nori would, without nori:
PCM, FLAC and MP3 at their rates in one stream, each with its song's details and cover, then volume
steps, pause, a seek and a fade. Prints the rate frames
played at from each Status. Needs ffmpeg and flac. Usage: send.py <host> [seconds per format]"""
import select, socket, struct, subprocess, sys, tempfile, time, os

HOST, SECONDS = sys.argv[1], int(sys.argv[2]) if len(sys.argv) > 2 else 6
PCM, FLAC, MP3 = 0, 1, 2
HELLO, DEPTH, FORMAT, AUDIO, ENCODED, FLUSH, PAUSE, RESUME, RAMP, VOLUME, COVER, NOW = 1, 2, 3, 4, 5, 7, 8, 9, 10, 11, 12, 13
CAPS, WANT, STATUS, FLUSHED, PRESS, DEVICE_VOLUME = 0x81, 0x82, 0x83, 0x84, 0x85, 0x86


def tones(rate, bits, seconds, hz):
    """Quiet stereo tones with a little noise, so FLAC has work to do."""
    expr = f"0.08*sin(2*PI*{hz}*t)+0.02*(random(0)-0.5)|0.08*sin(2*PI*{hz * 1.5}*t)+0.02*(random(1)-0.5)"
    fmt = {16: "s16le", 24: "s24le"}[bits]
    return subprocess.run(["ffmpeg", "-loglevel", "error", "-f", "lavfi", "-i", f"aevalsrc={expr}:s={rate}:d={seconds}", "-f", fmt, "-"], check=True, capture_output=True).stdout


def cover(n):
    """A JPEG at the size Caps asks, different for each song."""
    src = f"testsrc2=s=240x240:d=1,hue=h={n * 60}"
    return subprocess.run(["ffmpeg", "-loglevel", "error", "-f", "lavfi", "-i", src, "-frames:v", "1", "-f", "mjpeg", "-q:v", "3", "-"], check=True, capture_output=True).stdout


def text(s):
    b = s.encode()
    return struct.pack("<H", len(b)) + b


def flac_packets(raw, rate, bits):
    with tempfile.TemporaryDirectory() as d:
        open(f"{d}/a.raw", "wb").write(raw)
        subprocess.run(["flac", "--silent", "--force-raw-format", "--endian=little", "--sign=signed", "--channels=2", f"--bps={bits}", f"--sample-rate={rate}", "-8", "--no-seektable", "--no-padding", "-o", f"{d}/a.flac", f"{d}/a.raw"], check=True)
        file = open(f"{d}/a.flac", "rb").read()
    at, heads = 4, []
    while True:
        last = file[at] & 0x80
        at += 4 + int.from_bytes(file[at + 1:at + 4], "big")
        if last:
            break
    packets = [(file[:at], 0)]
    starts = []
    i = file.find(b"\xff\xf8", at)
    while i >= 0:
        if i + 16 < len(file) and flac_block(file, i):
            starts.append(i)
        i = file.find(b"\xff\xf8", i + 1)
    for a, b in zip(starts, starts[1:] + [len(file)]):
        packets.append((file[a:b], flac_block(file, a)))
    return packets


def flac_block(f, i):
    """The block of the frame header at i, if its CRC-8 is right."""
    code = f[i + 2] >> 4
    b = f[i + 4]
    n = 1 if b < 0x80 else (bin(b)[2:].index("0") if b != 0xFF else 8)
    at = i + 4 + n
    block = {1: 192, 2: 576, 3: 1152, 4: 2304, 5: 4608}.get(code) or (256 << (code - 8) if code >= 8 else 0)
    if code == 6:
        block = f[at] + 1
        at += 1
    elif code == 7:
        block = int.from_bytes(f[at:at + 2], "big") + 1
        at += 2
    rc = f[i + 2] & 15
    at += {12: 1, 13: 2, 14: 2}.get(rc, 0)
    crc = 0
    for x in f[i:at]:
        crc ^= x
        for _ in range(8):
            crc = ((crc << 1) ^ 7) & 0xFF if crc & 0x80 else (crc << 1) & 0xFF
    return block if block and crc == f[at] else 0


def mp3_packets(raw, rate):
    p = subprocess.run(["ffmpeg", "-loglevel", "error", "-f", "s16le", "-ar", str(rate), "-ac", "2", "-i", "-", "-b:a", "192k", "-write_xing", "0", "-id3v2_version", "0", "-f", "mp3", "-"], input=raw, check=True, capture_output=True).stdout
    at, out = 0, []
    while at + 4 <= len(p):
        kbps = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320][p[at + 2] >> 4]
        n = 144000 * kbps // rate + ((p[at + 2] >> 1) & 1)
        out.append((p[at:at + n], 1152))
        at += n
    return out


def pcm_items(raw, bits):
    frame = 2 * bits // 8
    return [(raw[i:i + 4096 * frame], len(raw[i:i + 4096 * frame]) // frame) for i in range(0, len(raw), 4096 * frame)]


def section(name, rate, bits, codec, seconds, hz):
    raw = tones(rate, bits, seconds, hz)
    if codec == FLAC:
        return name, rate, 0, codec, ENCODED, flac_packets(raw, rate, bits)
    if codec == MP3:
        return name, rate, 0, codec, ENCODED, mp3_packets(raw, rate)
    return name, rate, bits, codec, AUDIO, pcm_items(raw, bits)


class Link:
    def __init__(self):
        self.s = socket.create_connection((HOST, 47801), timeout=5)
        self.s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = b""
        self.credit = 0
        self.last = None  # (time, played)
        self.played = 0
        self.seen = []

    def send(self, kind, body=b""):
        self.s.sendall(struct.pack("<IB", len(body) + 1, kind) + body)

    def pump(self, wait):
        """Reads what the device said for up to `wait` seconds."""
        end = time.time() + wait
        while True:
            left = end - time.time()
            if not select.select([self.s], [], [], max(0, left))[0]:
                return
            d = self.s.recv(65536)
            if not d:
                raise SystemExit("the device hung up")
            self.buf += d
            while len(self.buf) >= 4 and len(self.buf) >= 4 + struct.unpack("<I", self.buf[:4])[0]:
                n = struct.unpack("<I", self.buf[:4])[0]
                kind, body = self.buf[4], self.buf[5:4 + n]
                self.buf = self.buf[4 + n:]
                self.frame(kind, body)
            if left <= 0:
                return

    def frame(self, kind, body):
        self.seen.append(kind)
        if kind == WANT:
            self.credit = struct.unpack("<I", body)[0]
            print(f"  want {self.credit}")
        elif kind == STATUS:
            played, held, delay = struct.unpack("<QII", body)
            now = time.time()
            rate = ""
            if self.last and now - self.last[0] > 0.2:
                rate = f"  {(played - self.last[1]) / (now - self.last[0]):8.0f} frames/s"
            if not self.last or now - self.last[0] > 0.2:
                self.last = (now, played)
            self.played = played
            print(f"  status played {played} held {held} delay {delay} us{rate}")
        elif kind == FLUSHED:
            print("  flushed", struct.unpack("<IQ", body))
        elif kind == PRESS:
            print("  button pressed", body[0])
        elif kind == DEVICE_VOLUME:
            print("  volume", struct.unpack("<f", body)[0])
        elif kind == CAPS:
            print("  caps", body.hex())


def main():
    print("preparing audio")
    sections = [
        section("PCM 16-bit 44.1 kHz", 44100, 16, PCM, SECONDS, 440),
        section("PCM 24-bit 48 kHz", 48000, 24, PCM, SECONDS, 523),
        section("FLAC 16-bit 44.1 kHz", 44100, 16, FLAC, SECONDS, 587),
        section("FLAC 24-bit 48 kHz", 48000, 24, FLAC, SECONDS, 659),
        section("MP3 44.1 kHz", 44100, 16, MP3, SECONDS, 784),
    ]
    l = Link()
    l.send(HELLO, struct.pack("<H", 1))
    l.pump(1)
    l.send(DEPTH, struct.pack("<I", 2000))
    for n, (name, rate, bits, codec, kind, items) in enumerate(sections):
        print(name, f"({len(items)} items, {sum(len(p) for p, _ in items) / SECONDS / 1000:.0f} kB/s)")
        l.send(FORMAT, struct.pack("<IBBB", rate, 2, bits, codec))
        jpeg = cover(n)
        print(f"  cover {n}: {len(jpeg)} bytes")
        l.send(COVER, struct.pack("<I", n) + jpeg)
        l.send(NOW, struct.pack("<I", n) + text(name) + text("send.py") + text("Tones") + struct.pack("<I", SECONDS * 1000) + b"\x01" + struct.pack("<I", n) + struct.pack("<I", 0))
        for payload, frames in items:
            while l.credit <= 0:
                l.pump(0.2)
            l.send(kind, payload)
            l.credit -= frames
            l.pump(0)
    print("volume steps")
    for gain in (0.1, 0.5, 1.0):
        l.send(VOLUME, struct.pack("<f", gain))
        l.pump(0.5)
    print("pausing with audio held")
    l.pump(0.5)
    l.send(RAMP, struct.pack("<fI", 0.2, 300))
    l.pump(0.4)
    l.send(PAUSE)
    l.pump(0.2)
    before = l.played
    l.pump(1.5)
    l.send(RESUME)
    l.pump(0.2)
    print(f"  played {before} before the pause, {l.played} at resume")
    l.pump(0.5)
    print("seeking: everything held is dropped")
    l.send(FLUSH, struct.pack("<IB", 7, 0))
    l.pump(0.5)
    l.send(RAMP, struct.pack("<fI", 1.0, 0))
    print("after the seek")
    name, rate, bits, codec, kind, items = sections[0]
    l.send(FORMAT, struct.pack("<IBBB", rate, 2, bits, codec))
    for payload, frames in items:
        while l.credit <= 0:
            l.pump(0.2)
        l.send(kind, payload)
        l.credit -= frames
        l.pump(0)
    l.pump(3)
    print("device said:", sorted({hex(k) for k in l.seen}))


main()
