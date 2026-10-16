//! Real-time synth engine, run inside the audio callback.
//!
//! Rules: no allocation, no locks, no syscalls besides `Instant::now()`. Commands arrive
//! through a bounded lock-free channel and statistics leave through atomics.

use std::f32::consts::TAU;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use crossbeam_channel::Receiver;

use crate::midi::MidiEvent;

const MAX_VOICES: usize = 64;
const SUSTAIN_CC: u8 = 64;
const ALL_NOTES_OFF_CC: u8 = 123;
const BEND_RANGE_SEMITONES: f32 = 2.0;
const MASTER_GAIN: f32 = 0.5;

pub enum EngineCmd {
    /// `at` is when the event reached us, for latency measurement.
    Midi { event: MidiEvent, at: Instant },
}

/// Numbers the audio thread publishes for the UI. All values are "last seen" unless noted.
#[derive(Default)]
pub struct Stats {
    /// MIDI arrival → start of the audio callback that renders it.
    pub queue_us: AtomicU32,
    /// Start of the audio callback → sound leaving the speaker, as reported by the backend.
    pub output_us: AtomicU32,
    /// Sum of the two above for the most recent note-on, plus the worst seen.
    pub note_latency_us: AtomicU32,
    pub note_latency_max_us: AtomicU32,
    pub frames_per_callback: AtomicU32,
    pub active_voices: AtomicU32,
    /// f32 bits, decaying peak of the master output.
    pub peak: AtomicU32,
    pub callbacks: AtomicU64,
    pub errors: AtomicU64,
}

impl Stats {
    pub fn peak(&self) -> f32 {
        f32::from_bits(self.peak.load(Relaxed))
    }

    pub fn reset_latency(&self) {
        self.note_latency_us.store(0, Relaxed);
        self.note_latency_max_us.store(0, Relaxed);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Idle,
    Attack,
    Decay,
    Release,
}

#[derive(Clone, Copy)]
struct Voice {
    stage: Stage,
    note: u8,
    phase: f32,
    /// Phase increment per sample before pitch bend (cycles/sample).
    inc: f32,
    amp: f32,
    env: f32,
    decay_coef: f32,
    /// Key physically held down.
    key_down: bool,
    started: u64,
}

impl Voice {
    const IDLE: Self = Self {
        stage: Stage::Idle,
        note: 0,
        phase: 0.0,
        inc: 0.0,
        amp: 0.0,
        env: 0.0,
        decay_coef: 0.0,
        key_down: false,
        started: 0,
    };
}

pub struct Engine {
    sample_rate: f32,
    voices: [Voice; MAX_VOICES],
    sustain: bool,
    bend: f32,
    attack_step: f32,
    release_coef: f32,
    note_counter: u64,
    peak: f32,
    peak_fall: f32,
    cmds: Receiver<EngineCmd>,
    stats: Arc<Stats>,
}

impl Engine {
    pub fn new(sample_rate: u32, cmds: Receiver<EngineCmd>, stats: Arc<Stats>) -> Self {
        let sr = sample_rate as f32;
        Self {
            sample_rate: sr,
            voices: [Voice::IDLE; MAX_VOICES],
            sustain: false,
            bend: 1.0,
            attack_step: 1.0 / (0.002 * sr),
            release_coef: time_constant_coef(0.08, sr),
            note_counter: 0,
            peak: 0.0,
            peak_fall: time_constant_coef(0.3, sr),
            cmds,
            stats,
        }
    }

    /// Called once at the start of every audio callback, before rendering.
    pub fn begin_callback(&mut self, frames: usize, output_delay_us: u32) {
        let callback_start = Instant::now();
        while let Ok(EngineCmd::Midi { event, at }) = self.cmds.try_recv() {
            let queue_us = callback_start.saturating_duration_since(at).as_micros() as u32;
            self.stats.queue_us.store(queue_us, Relaxed);
            if matches!(event, MidiEvent::NoteOn { .. }) {
                let total = queue_us + output_delay_us;
                self.stats.note_latency_us.store(total, Relaxed);
                self.stats.note_latency_max_us.fetch_max(total, Relaxed);
            }
            self.handle(event);
        }
        self.stats.output_us.store(output_delay_us, Relaxed);
        self.stats.frames_per_callback.store(frames as u32, Relaxed);
        self.stats.callbacks.fetch_add(1, Relaxed);
    }

    fn handle(&mut self, event: MidiEvent) {
        match event {
            MidiEvent::NoteOn { note, vel, .. } => self.note_on(note, vel),
            MidiEvent::NoteOff { note, .. } => self.note_off(note),
            MidiEvent::ControlChange { cc: SUSTAIN_CC, value, .. } => {
                self.sustain = value >= 64;
                if !self.sustain {
                    for v in self.voices.iter_mut().filter(|v| v.stage != Stage::Idle && !v.key_down) {
                        v.stage = Stage::Release;
                    }
                }
            }
            MidiEvent::ControlChange { cc: ALL_NOTES_OFF_CC, .. } => {
                for v in self.voices.iter_mut().filter(|v| v.stage != Stage::Idle) {
                    v.key_down = false;
                    v.stage = Stage::Release;
                }
            }
            MidiEvent::PitchBend { value, .. } => {
                self.bend = 2f32.powf(value as f32 / 8192.0 * BEND_RANGE_SEMITONES / 12.0);
            }
            _ => {}
        }
    }

    fn note_on(&mut self, note: u8, vel: u8) {
        // Re-striking a held/sustained note: let the old one ring out quickly.
        for v in self.voices.iter_mut().filter(|v| v.stage != Stage::Idle && v.note == note) {
            v.key_down = false;
            v.stage = Stage::Release;
        }
        let slot = self
            .voices
            .iter()
            .position(|v| v.stage == Stage::Idle)
            .unwrap_or_else(|| {
                // Steal the oldest voice.
                (0..MAX_VOICES).min_by_key(|&i| self.voices[i].started).unwrap_or(0)
            });
        let freq = 440.0 * 2f32.powf((note as f32 - 69.0) / 12.0);
        // Low notes ring longer, like a piano string.
        let decay_s = (8.0 * (-(note as f32 - 21.0) / 40.0).exp()).clamp(1.0, 8.0);
        let v = vel as f32 / 127.0;
        self.note_counter += 1;
        self.voices[slot] = Voice {
            stage: Stage::Attack,
            note,
            phase: 0.0,
            inc: freq / self.sample_rate,
            amp: v * v * 0.25,
            env: 0.0,
            decay_coef: time_constant_coef(decay_s, self.sample_rate),
            key_down: true,
            started: self.note_counter,
        };
    }

    fn note_off(&mut self, note: u8) {
        for v in self.voices.iter_mut().filter(|v| v.key_down && v.note == note) {
            v.key_down = false;
            if !self.sustain {
                v.stage = Stage::Release;
            }
        }
    }

    /// Fills `out` with stereo frames.
    pub fn render(&mut self, out: &mut [[f32; 2]]) {
        out.fill([0.0; 2]);
        let mut active = 0;
        for v in self.voices.iter_mut().filter(|v| v.stage != Stage::Idle) {
            active += 1;
            let inc = v.inc * self.bend;
            for frame in out.iter_mut() {
                match v.stage {
                    Stage::Attack => {
                        v.env += self.attack_step;
                        if v.env >= 1.0 {
                            v.env = 1.0;
                            v.stage = Stage::Decay;
                        }
                    }
                    Stage::Decay => v.env *= v.decay_coef,
                    Stage::Release => v.env *= self.release_coef,
                    Stage::Idle => break,
                }
                if v.env < 1e-4 && v.stage != Stage::Attack {
                    v.stage = Stage::Idle;
                    break;
                }
                let p = v.phase * TAU;
                // A touch of 2nd/3rd harmonic so it is not a pure test tone.
                let s = p.sin() + 0.3 * (2.0 * p).sin() + 0.1 * (3.0 * p).sin();
                let x = s * v.amp * v.env;
                frame[0] += x;
                frame[1] += x;
                v.phase += inc;
                if v.phase >= 1.0 {
                    v.phase -= 1.0;
                }
            }
        }

        for frame in out.iter_mut() {
            for s in frame.iter_mut() {
                *s = soft_clip(*s * MASTER_GAIN);
            }
            let level = frame[0].abs().max(frame[1].abs());
            self.peak = if level > self.peak { level } else { self.peak * self.peak_fall };
        }
        self.stats.active_voices.store(active, Relaxed);
        self.stats.peak.store(self.peak.to_bits(), Relaxed);
    }
}

/// Per-sample multiplier giving an exponential decay with time constant `seconds`.
fn time_constant_coef(seconds: f32, sample_rate: f32) -> f32 {
    (-1.0 / (seconds * sample_rate)).exp()
}

/// Smooth limiter: rational tanh approximation, nearly linear below ~0.3, saturates at ±1.
fn soft_clip(x: f32) -> f32 {
    let x = x.clamp(-3.0, 3.0);
    x * (27.0 + x * x) / (27.0 + 9.0 * x * x)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> (Engine, crossbeam_channel::Sender<EngineCmd>) {
        let (tx, rx) = crossbeam_channel::bounded(256);
        (Engine::new(48_000, rx, Arc::new(Stats::default())), tx)
    }

    fn send(tx: &crossbeam_channel::Sender<EngineCmd>, event: MidiEvent) {
        tx.send(EngineCmd::Midi { event, at: Instant::now() }).unwrap();
    }

    fn active(e: &Engine) -> usize {
        e.voices.iter().filter(|v| v.stage != Stage::Idle).count()
    }

    #[test]
    fn note_sounds_then_releases() {
        let (mut e, tx) = engine();
        let mut buf = [[0.0; 2]; 256];
        send(&tx, MidiEvent::NoteOn { ch: 0, note: 69, vel: 100 });
        e.begin_callback(buf.len(), 0);
        e.render(&mut buf);
        assert_eq!(active(&e), 1);
        assert!(buf.iter().any(|f| f[0].abs() > 0.01));

        send(&tx, MidiEvent::NoteOff { ch: 0, note: 69, vel: 0 });
        e.begin_callback(buf.len(), 0);
        for _ in 0..200 {
            e.render(&mut buf);
        }
        assert_eq!(active(&e), 0);
    }

    #[test]
    fn sustain_pedal_holds_notes() {
        let (mut e, tx) = engine();
        let mut buf = [[0.0; 2]; 256];
        send(&tx, MidiEvent::ControlChange { ch: 0, cc: SUSTAIN_CC, value: 127 });
        send(&tx, MidiEvent::NoteOn { ch: 0, note: 60, vel: 100 });
        send(&tx, MidiEvent::NoteOff { ch: 0, note: 60, vel: 0 });
        e.begin_callback(buf.len(), 0);
        for _ in 0..50 {
            e.render(&mut buf);
        }
        assert_eq!(active(&e), 1, "pedal down keeps the note ringing");

        send(&tx, MidiEvent::ControlChange { ch: 0, cc: SUSTAIN_CC, value: 0 });
        e.begin_callback(buf.len(), 0);
        for _ in 0..200 {
            e.render(&mut buf);
        }
        assert_eq!(active(&e), 0);
    }

    #[test]
    fn output_stays_bounded_with_many_voices() {
        let (mut e, tx) = engine();
        let mut buf = [[0.0; 2]; 512];
        for note in 30..100 {
            send(&tx, MidiEvent::NoteOn { ch: 0, note, vel: 127 });
        }
        e.begin_callback(buf.len(), 0);
        e.render(&mut buf);
        assert_eq!(active(&e), MAX_VOICES);
        assert!(buf.iter().all(|f| f[0].abs() <= 1.0 && f[1].abs() <= 1.0));
    }
}
