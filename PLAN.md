# OFFENBACK — build plan

## Decisions
- **Language**: Rust. Crates: `midir` (ALSA MIDI in), `cpal` (audio out via PipeWire's ALSA layer),
  `ratatui` + `crossterm` (TUI), `ogg` + `opus` (sample decoding), `serde` + `toml` (profiles), `hound` (WAV recording).
- **Engine**: our own sampler playing **SFZ** instruments (open format). Libraries live in `~/Sounds/SFZ/`:
  Splendid Grand Piano (Akai Steinway, public domain) and Salamander Grand Piano V3
  (Yamaha C5 by Alexander Holm, CC-BY 3.0). Muse Sounds `.sts` could become a second importer later.
- **Outputs**: JBL Flip 5 (Bluetooth, SBC only) and OneOdio A70 (Bluetooth SBC, or wired jack).
- **v1 extra feature**: recording.

## Muse Sounds `.sts` format (as observed)
```
u32 len | "StaffPad Sample File" | u32 count | count × { u32 len | name | u64 start | u64 end } | Ogg Opus blobs
```
Piano names: `Steinway_Mixed_<dyn>_up_<note><oct>.opus`, dyn ∈ ppp pp p mp mf f ff (7 layers × 88 keys).
`.spx` (instrument mapping) looks encrypted, so we build the key/velocity map from the sample names.
Each instrument may use a different naming scheme, so we'll need a small mapper per instrument family.

## Architecture
```
MIDI in ──► transform ──► sampler voices ──► FX chain ──► output ──► (recorder tap)
(chosen     vel curve     RAM PCM, 48 kHz    filter        cpal
 port)      auto-sustain  ~64 voices         reverb        small buffer
            transpose     layer pick/xfade   limiter
            detune/pitch  release envelope
                 ▲
TUI thread ── lock-free parameter queue (no alloc/locks in audio callback)
```

## Milestones
1. ✅ **Skeleton**: cargo project, choose a MIDI port in the TUI, live event log (test with `fake_keyboard`).
2. ✅ **Audio out**: pick output device, sine-synth voices, measure latency on wired output, buffer size setting.
3. ✅ **SFZ sampler**: SFZ parser (defines/includes/inheritance/curves), CC-modulated params, FLAC/WAV decoded to
   normalized i16 in RAM, Hermite interpolation, DAHDSR, filters, polyphony groups, release triggers, keyswitches,
   CC-triggered regions. Splendid: 245 MB, 0.6 s load. Salamander: 1.25 GB, 3.3 s load.
3b. ✅ **UI redesign**: top bar (held/sustain lights, port, output), effects matrix (arrows select,
   touchpad scroll adjusts), mini keyboard, popups for Port / Output (buffer, latency) / Instrument.
   Parameters are not yet wired to sound.
4. **Transform stage**: velocity curve (editable points), auto-sustain, CC64 passthrough, transpose, detune.
5. **FX**: low-pass/cutoff, reverb, master gain + limiter.
6. **Profiles**: TOML files in `soundsprofiles/`, load/save/switch while playing.
7. **Recording**: tap the master bus and write a WAV in a non-real-time thread (also write MIDI).
8. **Bluetooth tuning**: PipeWire bluez5 latency settings, SBC-XQ, latency offset measurement.

## Latency budget
| Stage | Target |
|---|---|
| MIDI in → engine | < 1 ms |
| Audio buffer (64–128 frames @ 48 kHz) | 1.3–2.7 ms (×2 for double buffering) |
| **Bluetooth SBC (both devices)** | **~150–250 ms, outside our control** |
| A70 on the wired jack | ≈ 0 ms extra |

### Measured (2026-10-16, native PipeWire backend)
Estimated key → sound, as reported by PipeWire (no acoustic measurement yet):

| Output | Buffer | Output delay | App total (last note) |
|---|---|---|---|
| Null test sink | 32 / 64 / 128 / 256 | 0.7 / 1.3 / 2.7 / 5.3 ms | 1.3 / 2.2 / 3.8 / 8.1 ms |
| Laptop speakers | 128 / 1024 | 5.3 / 42.7 ms | |
| OneOdio A70, Bluetooth SBC (public or `.1` direct node) | 128 | ~207 ms | |

The same thing through ALSA's pipewire plugin at 128 frames: 6.4 ms (vs 3.8 native).
The A70 sink caps the graph at 256 frames when asked for more.
