//! Session recording: the master output to WAV (written on a background thread) and the
//! incoming MIDI to a Standard MIDI File.
//!
//! The audio thread only copies fixed-size blocks into a preallocated channel; this thread
//! does the disk work.

use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};

use crate::midi::MidiEvent;

/// Frames per block; must cover the largest chunk the audio callback renders at once.
pub const BLOCK_FRAMES: usize = 256;
/// Blocks the channel can hold: ~2.7 s at 48 kHz before the writer has to catch up.
const QUEUE_BLOCKS: usize = 512;
/// MIDI file timing: 1000 ticks per quarter at 60 BPM, so one tick is one millisecond.
const TICKS_PER_QUARTER: u16 = 1000;
const MICROS_PER_QUARTER: u32 = 1_000_000;

pub struct AudioBlock {
    pub len: usize,
    pub frames: [[f32; 2]; BLOCK_FRAMES],
}

pub enum RecMsg {
    Audio(AudioBlock),
    Stop,
}

pub struct Recording {
    pub wav_path: PathBuf,
    pub mid_path: PathBuf,
    pub started: Instant,
    tx: Sender<RecMsg>,
    midi: Vec<(Duration, MidiEvent)>,
    writer: JoinHandle<Result<u64>>,
}

pub struct Saved {
    pub wav_path: PathBuf,
    pub seconds: f32,
    pub notes: usize,
}

impl Recording {
    /// Creates the files and the writer thread. `sender()` goes to the audio engine.
    pub fn start(dir: &Path, sample_rate: u32) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let stem = chrono::Local::now().format("offenback-%Y-%m-%d_%H-%M-%S").to_string();
        let wav_path = dir.join(format!("{stem}.wav"));
        let mid_path = dir.join(format!("{stem}.mid"));
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate,
            bits_per_sample: 24,
            sample_format: hound::SampleFormat::Int,
        };
        let wav = hound::WavWriter::create(&wav_path, spec)
            .with_context(|| format!("cannot create {}", wav_path.display()))?;
        let (tx, rx) = crossbeam_channel::bounded(QUEUE_BLOCKS);
        let writer = std::thread::spawn(move || write_wav(wav, rx));
        Ok(Self { wav_path, mid_path, started: Instant::now(), tx, midi: Vec::new(), writer })
    }

    pub fn sender(&self) -> Sender<RecMsg> {
        self.tx.clone()
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Records an incoming MIDI event; `at` is when it arrived.
    pub fn midi(&mut self, event: MidiEvent, at: Instant) {
        self.midi.push((at.saturating_duration_since(self.started), event));
    }

    /// Finishes both files. The engine must have been told to stop sending first.
    pub fn stop(self, sample_rate: u32) -> Result<Saved> {
        let _ = self.tx.send(RecMsg::Stop);
        drop(self.tx);
        let frames = self.writer.join().map_err(|_| anyhow::anyhow!("WAV writer crashed"))??;
        let notes = self.midi.iter().filter(|(_, e)| matches!(e, MidiEvent::NoteOn { .. })).count();
        std::fs::write(&self.mid_path, smf(&self.midi))
            .with_context(|| format!("cannot write {}", self.mid_path.display()))?;
        Ok(Saved { wav_path: self.wav_path, seconds: frames as f32 / sample_rate as f32, notes })
    }
}

fn write_wav(mut wav: hound::WavWriter<std::io::BufWriter<std::fs::File>>, rx: Receiver<RecMsg>) -> Result<u64> {
    const FULL_SCALE: f32 = 8_388_607.0; // 2^23 - 1
    let mut frames = 0u64;
    for msg in rx {
        match msg {
            RecMsg::Audio(block) => {
                for f in &block.frames[..block.len] {
                    wav.write_sample((f[0].clamp(-1.0, 1.0) * FULL_SCALE) as i32)?;
                    wav.write_sample((f[1].clamp(-1.0, 1.0) * FULL_SCALE) as i32)?;
                }
                frames += block.len as u64;
            }
            // Blocks the engine sends after this belong to no one: the file is complete.
            RecMsg::Stop => break,
        }
    }
    wav.finalize()?;
    Ok(frames)
}

/// Single-track (format 0) Standard MIDI File.
fn smf(events: &[(Duration, MidiEvent)]) -> Vec<u8> {
    let mut track = Vec::new();
    // Tempo so that ticks are milliseconds.
    track.extend([0x00, 0xFF, 0x51, 0x03]);
    track.extend(&MICROS_PER_QUARTER.to_be_bytes()[1..]);
    let mut last_ms = 0u64;
    for (t, e) in events {
        let Some((bytes, len)) = e.to_bytes() else { continue };
        let ms = t.as_millis() as u64;
        push_vlq(&mut track, ms.saturating_sub(last_ms) as u32);
        track.extend(&bytes[..len]);
        last_ms = ms.max(last_ms);
    }
    track.extend([0x00, 0xFF, 0x2F, 0x00]); // end of track

    let mut out = Vec::with_capacity(track.len() + 22);
    out.extend(b"MThd");
    out.extend(6u32.to_be_bytes());
    out.extend(0u16.to_be_bytes()); // format 0
    out.extend(1u16.to_be_bytes()); // one track
    out.extend(TICKS_PER_QUARTER.to_be_bytes());
    out.extend(b"MTrk");
    out.extend((track.len() as u32).to_be_bytes());
    out.extend(track);
    out
}

fn push_vlq(out: &mut Vec<u8>, mut v: u32) {
    let mut stack = [0u8; 5];
    let mut n = 0;
    loop {
        stack[n] = (v & 0x7F) as u8;
        n += 1;
        v >>= 7;
        if v == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        out.push(stack[i] | if i > 0 { 0x80 } else { 0 });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vlq_encoding() {
        let enc = |v| {
            let mut o = Vec::new();
            push_vlq(&mut o, v);
            o
        };
        assert_eq!(enc(0), [0x00]);
        assert_eq!(enc(0x7F), [0x7F]);
        assert_eq!(enc(0x80), [0x81, 0x00]);
        assert_eq!(enc(0x3FFF), [0xFF, 0x7F]);
        assert_eq!(enc(0x0FFF_FFFF), [0xFF, 0xFF, 0xFF, 0x7F]);
    }

    #[test]
    fn records_wav_and_midi_that_read_back() {
        let dir = std::env::temp_dir().join(format!("offenback-rec-{}", std::process::id()));
        let mut rec = Recording::start(&dir, 48_000).unwrap();
        let tx = rec.sender();
        let start = rec.started;
        rec.midi(MidiEvent::NoteOn { ch: 0, note: 60, vel: 100 }, start + Duration::from_millis(10));
        rec.midi(MidiEvent::NoteOff { ch: 0, note: 60, vel: 0 }, start + Duration::from_millis(510));
        for i in 0..10 {
            let mut b = AudioBlock { len: BLOCK_FRAMES, frames: [[0.0; 2]; BLOCK_FRAMES] };
            b.frames[0] = [0.5, -0.5 * i as f32 / 10.0];
            tx.send(RecMsg::Audio(b)).unwrap();
        }
        drop(tx);
        let (wav_path, mid_path) = (rec.wav_path.clone(), rec.mid_path.clone());
        let saved = rec.stop(48_000).unwrap();
        assert_eq!(saved.notes, 1);
        assert!((saved.seconds - 2560.0 / 48_000.0).abs() < 1e-6);

        let reader = hound::WavReader::open(&wav_path).unwrap();
        assert_eq!(reader.spec().bits_per_sample, 24);
        assert_eq!(reader.len(), 2560 * 2);
        let first: Vec<i32> = reader.into_samples::<i32>().take(2).map(Result::unwrap).collect();
        assert_eq!(first[0], (0.5 * 8_388_607.0) as i32);

        let smf = std::fs::read(&mid_path).unwrap();
        assert_eq!(&smf[..4], b"MThd");
        // Tempo meta, then note-on 10 ms in, note-off 500 ms later (500 = 0x83 0x74).
        let track = &smf[22..];
        assert_eq!(&track[7..11], &[0x0A, 0x90, 60, 100]);
        assert_eq!(&track[11..16], &[0x83, 0x74, 0x80, 60, 0]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
