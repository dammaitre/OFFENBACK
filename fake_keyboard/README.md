# fake_keyboard

Plays a `.mid` file out of a virtual ALSA MIDI port, so any program that reads MIDI input
behaves as if a real keyboard were plugged in. Handy for developing OFFENBACK without the hardware.

## Setup

```bash
cd /mnt/DAMIEN/DEV/OFFENBACK/fake_keyboard
python3 -m venv --copies .venv        # --copies: this drive is NTFS (no symlinks)
.venv/bin/pip install -r requirements.txt
```

The only dependency is `mido`, which is used to parse the file. Port output goes straight to
the system `libasound.so.2` through `ctypes`, so you don't need `python-rtmidi` or the ALSA
dev headers.

## Usage

```bash
.venv/bin/python fake_keyboard.py "Parler sous la pluie.mid" -w
```

`Parler sous la pluie.mid` is a sample file in this folder (two piano tracks, about 76 s long).
You can pass any other `.mid` file instead.

| Option          | Effect                                                   |
|-----------------|----------------------------------------------------------|
| `-w`            | wait for Enter before playing (time to connect)          |
| `-d SECONDS`    | wait N seconds before playing                            |
| `-l`            | loop forever                                             |
| `-s FACTOR`     | tempo multiplier (`0.5` = half speed, `2` = double)      |
| `-n NAME`       | port/client name (default `Fake Keyboard`)               |
| `-v`            | print every message as it is sent                        |

Ctrl-C stops playback and sends All Notes Off + sustain release on all 16 channels.

## Receiving the notes

The port appears as an ALSA sequencer client named **Fake Keyboard**. PipeWire and JACK apps
see it too.

```bash
aconnect -l                          # find it (e.g. 129:0)
aseqdump -p "Fake Keyboard"          # quick check: print incoming events
aconnect "Fake Keyboard" <client>    # wire it to another program manually
```

From Python with mido (needs a backend such as python-rtmidi on the receiving side):

```python
with mido.open_input("Fake Keyboard:Fake Keyboard 129:0") as inp:   # see mido.get_input_names()
    for msg in inp:
        print(msg)
```

A real USB keyboard will appear the same way, as another ALSA MIDI port. A program that lets
you choose the input port works with either one unchanged.

## Notes

- Linux and ALSA only.
- Meta events (tempo, track names…) shape the timing but are not sent. Only channel and
  sysex messages go out the port.
