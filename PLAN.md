# OFFENBACK — build plan

## Decisions
- **Language**: Rust. Crates: `midir` (ALSA MIDI in), `cpal` (audio out via PipeWire's ALSA layer),
  `ratatui` + `crossterm` (TUI), `ogg` + `opus` (sample decoding), `serde` + `toml` (profiles), `hound` (WAV recording).
- **Engine**: our own sampler, fed by Opus samples read from Muse Sounds `.sts` containers
  (personal use; the samples are read in place from `~/Muse Sounds`, never redistributed).
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
3. **.sts reader + sampler**: parse the container, decode Opus to f32 in RAM, map the Piano, velocity layers, release envelope.
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
