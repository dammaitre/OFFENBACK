#!/usr/bin/env python3
"""Play a .mid file out of a virtual ALSA MIDI port, as if a real keyboard were plugged in.

Any program that reads MIDI input (DAW, your own app, aseqdump...) can connect to the
port "Fake Keyboard" and receive the notes in real time.

Uses mido only to parse the file; talks to ALSA's sequencer through ctypes, so no
compiled Python extension (python-rtmidi) or ALSA headers are needed.
"""
import argparse
import ctypes as C
import signal
import sys
import time

import mido

# --- minimal libasound sequencer binding ---------------------------------------------

asound = C.CDLL("libasound.so.2")

SND_SEQ_OPEN_OUTPUT = 1
SND_SEQ_PORT_CAP_READ = 1 << 0
SND_SEQ_PORT_CAP_SUBS_READ = 1 << 5
SND_SEQ_PORT_TYPE_MIDI_GENERIC = 1 << 1
SND_SEQ_PORT_TYPE_HARDWARE = 1 << 16  # advertise ourselves like a hardware keyboard
SND_SEQ_PORT_TYPE_APPLICATION = 1 << 20
SND_SEQ_QUEUE_DIRECT = 253
SND_SEQ_ADDRESS_SUBSCRIBERS = 254
SND_SEQ_ADDRESS_UNKNOWN = 253
SND_SEQ_EVENT_NONE = 255


class Addr(C.Structure):
    _fields_ = [("client", C.c_ubyte), ("port", C.c_ubyte)]


class SeqEvent(C.Structure):  # snd_seq_event_t, 28 bytes
    _fields_ = [
        ("type", C.c_ubyte),
        ("flags", C.c_ubyte),
        ("tag", C.c_ubyte),
        ("queue", C.c_ubyte),
        ("time", C.c_ubyte * 8),
        ("source", Addr),
        ("dest", Addr),
        ("data", C.c_ubyte * 12),
    ]


assert C.sizeof(SeqEvent) == 28

asound.snd_seq_open.argtypes = [C.POINTER(C.c_void_p), C.c_char_p, C.c_int, C.c_int]
asound.snd_seq_set_client_name.argtypes = [C.c_void_p, C.c_char_p]
asound.snd_seq_create_simple_port.argtypes = [C.c_void_p, C.c_char_p, C.c_uint, C.c_uint]
asound.snd_seq_client_id.argtypes = [C.c_void_p]
asound.snd_seq_event_output_direct.argtypes = [C.c_void_p, C.POINTER(SeqEvent)]
asound.snd_seq_close.argtypes = [C.c_void_p]
asound.snd_midi_event_new.argtypes = [C.c_size_t, C.POINTER(C.c_void_p)]
asound.snd_midi_event_encode.restype = C.c_long
asound.snd_midi_event_encode.argtypes = [C.c_void_p, C.c_char_p, C.c_long, C.POINTER(SeqEvent)]
asound.snd_midi_event_reset_encode.argtypes = [C.c_void_p]
asound.snd_strerror.restype = C.c_char_p


def check(rc, what):
    if rc < 0:
        sys.exit(f"{what} failed: {asound.snd_strerror(rc).decode()}")
    return rc


class VirtualPort:
    def __init__(self, name):
        self.seq = C.c_void_p()
        check(asound.snd_seq_open(C.byref(self.seq), b"default", SND_SEQ_OPEN_OUTPUT, 0), "snd_seq_open")
        asound.snd_seq_set_client_name(self.seq, name.encode())
        self.port = check(
            asound.snd_seq_create_simple_port(
                self.seq,
                name.encode(),
                SND_SEQ_PORT_CAP_READ | SND_SEQ_PORT_CAP_SUBS_READ,
                SND_SEQ_PORT_TYPE_MIDI_GENERIC | SND_SEQ_PORT_TYPE_HARDWARE | SND_SEQ_PORT_TYPE_APPLICATION,
            ),
            "snd_seq_create_simple_port",
        )
        self.client = asound.snd_seq_client_id(self.seq)
        self.encoder = C.c_void_p()
        check(asound.snd_midi_event_new(65536, C.byref(self.encoder)), "snd_midi_event_new")

    def send(self, raw: bytes):
        """Send raw MIDI bytes (one complete message) to every subscriber of the port."""
        asound.snd_midi_event_reset_encode(self.encoder)
        ev = SeqEvent()
        ev.type = SND_SEQ_EVENT_NONE
        n = asound.snd_midi_event_encode(self.encoder, raw, len(raw), C.byref(ev))
        if n < 0 or ev.type == SND_SEQ_EVENT_NONE:
            return
        ev.queue = SND_SEQ_QUEUE_DIRECT
        ev.source.port = self.port
        ev.dest.client = SND_SEQ_ADDRESS_SUBSCRIBERS
        ev.dest.port = SND_SEQ_ADDRESS_UNKNOWN
        asound.snd_seq_event_output_direct(self.seq, C.byref(ev))

    def all_notes_off(self):
        for ch in range(16):
            self.send(bytes([0xB0 | ch, 123, 0]))  # All Notes Off
            self.send(bytes([0xB0 | ch, 64, 0]))  # release sustain pedal

    def close(self):
        asound.snd_seq_close(self.seq)


# --- playback ------------------------------------------------------------------------


def play(port, midi, speed, verbose):
    start = time.monotonic()
    t = 0.0
    for msg in midi:  # iterating a MidiFile yields messages with delta times in seconds
        t += msg.time / speed
        if msg.is_meta:
            continue
        delay = start + t - time.monotonic()
        if delay > 0:
            time.sleep(delay)
        port.send(bytes(msg.bytes()))
        if verbose:
            print(f"{t:8.3f}  {msg}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("file", help="MIDI file to play")
    ap.add_argument("-n", "--name", default="Fake Keyboard", help="port/client name (default: %(default)s)")
    ap.add_argument("-l", "--loop", action="store_true", help="repeat forever")
    ap.add_argument("-s", "--speed", type=float, default=1.0, help="tempo multiplier, e.g. 0.5 or 2")
    ap.add_argument("-d", "--delay", type=float, default=0.0, help="seconds to wait before starting (time to connect)")
    ap.add_argument("-w", "--wait", action="store_true", help="wait for Enter before starting")
    ap.add_argument("-v", "--verbose", action="store_true", help="print each message as it is sent")
    args = ap.parse_args()

    midi = mido.MidiFile(args.file)
    port = VirtualPort(args.name)
    print(f"Virtual MIDI port ready: {port.client}:{port.port} '{args.name}'  ({midi.length / args.speed:.1f}s)")

    def stop(*_):
        port.all_notes_off()
        port.close()
        print("\nstopped")
        sys.exit(0)

    signal.signal(signal.SIGINT, stop)
    signal.signal(signal.SIGTERM, stop)

    if args.wait:
        input("Connect your program to the port, then press Enter to start...")
    elif args.delay:
        time.sleep(args.delay)

    while True:
        print("playing", args.file)
        play(port, midi, args.speed, args.verbose)
        port.all_notes_off()
        if not args.loop:
            break
    stop()


if __name__ == "__main__":
    main()
