//! Real-time engine (sampler and synth voices), run inside the audio callback.
//!
//! Rules: no allocation, no locks, no syscalls besides `Instant::now()`. Commands arrive
//! through a bounded lock-free channel and statistics leave through atomics. Replaced
//! instruments are handed back through `garbage` so their memory is freed off this thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender};

use crate::instrument::{CC_COUNT, Ctx, FilterKind, Instrument, Region, Trigger};
use crate::midi::MidiEvent;
use crate::params::{ParamId, Params, velocity_gamma};
use crate::recorder::{AudioBlock, BLOCK_FRAMES, RecMsg};
use crate::reverb::Reverb;
use crate::synth::{MAX_UNISON, oscillator};

const MAX_VOICES: usize = 128;
const BEND_RANGE_CENTS: f32 = 200.0;
const MASTER_GAIN: f32 = 1.0;
/// -80 dB: below this a voice is considered finished.
const SILENCE: f32 = 1e-4;
/// ln(1000): exponential segments fall by 60 dB over their nominal time.
const LN_1000: f32 = 6.907_755;
const ALL_SOUND_OFF_CC: u8 = 120;
const ALL_NOTES_OFF_CC: u8 = 123;
/// Samples fade out over their last this-many seconds (at most 1/8 of their length), so a
/// note held past the end of a short recording fades instead of cutting off.
const TAIL_FADE_S: f32 = 0.25;
/// Master filters at these cutoffs are bypassed.
const LOW_PASS_OFF_HZ: f32 = 19_900.0;
const HIGH_PASS_OFF_HZ: f32 = 20.5;
/// Synth voice filters are recomputed this often (frames) while their envelope moves.
const FILTER_UPDATE_FRAMES: usize = 32;
/// Notes starting this close together are a chord: they don't glide from one another.
const GLIDE_CHORD_S: f32 = 0.03;

pub enum EngineCmd {
    /// `at` is when the event reached us, for latency measurement.
    Midi { event: MidiEvent, at: Instant },
    SetInstrument(Arc<Instrument>),
    Params(Params),
    /// Start (`Some`) or stop (`None`) copying the master output to a recorder.
    Record(Option<Sender<RecMsg>>),
}

/// The effects matrix, converted to what the audio thread needs.
#[derive(Clone, Copy)]
struct User {
    gain: f32,
    transpose: i32,
    detune: f32,
    /// Synth envelope: seconds, seconds, level 0..1, seconds. Sampled instruments ignore it.
    attack: f32,
    decay: f32,
    sustain: f32,
    release: f32,
    /// Synth vibrato depth and speed.
    lfo_cents: f32,
    lfo_rate: f32,
    /// Octaves the synth filter opens at each note's attack.
    filter_env_oct: f32,
    /// Synth portamento time (0 = off).
    glide: f32,
    low_pass_hz: f32,
    resonance_db: f32,
    high_pass_hz: f32,
    vel_gamma: f32,
    /// Reverb send level, 0..1 (1 ≈ as loud as the dry signal).
    reverb: f32,
}

impl User {
    fn new(p: &Params) -> Self {
        Self {
            gain: db_to_gain(p.get(ParamId::Volume)),
            transpose: p.get(ParamId::Transpose).round() as i32,
            detune: 2f32.powf(p.get(ParamId::Detune) / 1200.0),
            attack: p.get(ParamId::Attack),
            decay: p.get(ParamId::Decay),
            sustain: p.get(ParamId::Sustain) / 100.0,
            release: p.get(ParamId::Release),
            lfo_cents: p.get(ParamId::Lfo),
            lfo_rate: p.get(ParamId::LfoRate),
            filter_env_oct: p.get(ParamId::FilterEnv),
            glide: p.get(ParamId::Glide),
            low_pass_hz: p.get(ParamId::LowPass),
            resonance_db: p.get(ParamId::Resonance),
            high_pass_hz: p.get(ParamId::HighPass),
            vel_gamma: velocity_gamma(p.get(ParamId::Velocity)),
            reverb: p.get(ParamId::Reverb) / 100.0,
        }
    }

    fn velocity(&self, vel: u8) -> u8 {
        if vel == 0 || self.vel_gamma == 1.0 {
            return vel;
        }
        ((vel as f32 / 127.0).powf(self.vel_gamma) * 127.0).round().clamp(1.0, 127.0) as u8
    }
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
    /// Audio blocks the recorder could not take (disk too slow).
    pub rec_dropped: AtomicU64,
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

/// Per-sample multiplier for an exponential fall of 60 dB over `seconds`.
fn exp_coef(seconds: f32, sr: f32) -> f32 {
    if seconds <= 0.0 { 0.0 } else { (-LN_1000 / (seconds * sr)).exp() }
}

fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[derive(Clone, Copy, PartialEq, PartialOrd, Debug)]
enum Stage {
    Delay,
    Attack,
    Hold,
    Decay,
    Sustain,
    Release,
    Done,
}

#[derive(Clone, Copy)]
struct Env {
    stage: Stage,
    level: f32,
    delay_left: u32,
    hold_left: u32,
    attack_step: f32,
    decay_coef: f32,
    sustain: f32,
    release_coef: f32,
    /// Released during the attack: finish the attack first, so very short notes still sound.
    release_after_attack: bool,
}

impl Env {
    const IDLE: Self = Self {
        stage: Stage::Done,
        level: 0.0,
        delay_left: 0,
        hold_left: 0,
        attack_step: 0.0,
        decay_coef: 0.0,
        sustain: 0.0,
        release_coef: 0.0,
        release_after_attack: false,
    };

    fn new(delay: f32, attack: f32, hold: f32, decay: f32, sustain_pct: f32, sr: f32) -> Self {
        let delay_left = (delay.max(0.0) * sr) as u32;
        Self {
            // Skipping an empty delay matters: a note released before its first rendered
            // sample must still fade out rather than be treated as never started.
            stage: if delay_left > 0 { Stage::Delay } else { Stage::Attack },
            level: 0.0,
            delay_left,
            hold_left: (hold.max(0.0) * sr) as u32,
            attack_step: if attack > 0.0 { 1.0 / (attack * sr) } else { 1.0 },
            decay_coef: exp_coef(decay, sr),
            sustain: (sustain_pct / 100.0).clamp(0.0, 1.0),
            release_coef: 0.0,
            release_after_attack: false,
        }
    }

    #[inline]
    fn next(&mut self) -> f32 {
        match self.stage {
            Stage::Delay => {
                if self.delay_left > 0 {
                    self.delay_left -= 1;
                } else {
                    self.stage = Stage::Attack;
                }
            }
            Stage::Attack => {
                self.level += self.attack_step;
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.stage = if self.release_after_attack { Stage::Release } else { Stage::Hold };
                }
            }
            Stage::Hold => {
                if self.hold_left > 0 {
                    self.hold_left -= 1;
                } else {
                    self.stage = Stage::Decay;
                }
            }
            Stage::Decay => {
                self.level = self.sustain + (self.level - self.sustain) * self.decay_coef;
                if self.level - self.sustain < SILENCE {
                    self.level = self.sustain;
                    self.stage = if self.sustain < SILENCE { Stage::Done } else { Stage::Sustain };
                }
            }
            Stage::Sustain => {}
            Stage::Release => {
                self.level *= self.release_coef;
                if self.level < SILENCE {
                    self.level = 0.0;
                    self.stage = Stage::Done;
                }
            }
            Stage::Done => {}
        }
        self.level
    }

    fn release(&mut self, seconds: f32, sr: f32) {
        self.release_coef = exp_coef(seconds, sr);
        match self.stage {
            // Still silent: nothing to fade.
            Stage::Delay => self.stage = Stage::Done,
            Stage::Attack => self.release_after_attack = true,
            Stage::Hold | Stage::Decay | Stage::Sustain => self.stage = Stage::Release,
            Stage::Release | Stage::Done => {}
        }
    }
}

/// Transposed direct form II biquad, stereo.
#[derive(Clone, Copy)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z: [[f32; 2]; 2],
}

impl Biquad {
    fn new(kind: FilterKind, cutoff: f32, resonance_db: f32, sr: f32) -> Self {
        let mut f = Self { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0, z: [[0.0; 2]; 2] };
        f.set(kind, cutoff, resonance_db, sr);
        f
    }

    /// Updates the coefficients, keeping the filter state (no click when sweeping).
    fn set(&mut self, kind: FilterKind, cutoff: f32, resonance_db: f32, sr: f32) {
        let fc = cutoff.clamp(10.0, 0.45 * sr);
        let w0 = std::f32::consts::TAU * fc / sr;
        let (b0, b1, b2, a1, a2) = match kind {
            FilterKind::Lpf1p | FilterKind::Hpf1p => {
                let k = (w0 / 2.0).tan();
                let a1 = (k - 1.0) / (k + 1.0);
                if kind == FilterKind::Lpf1p {
                    let b0 = k / (1.0 + k);
                    (b0, b0, 0.0, a1, 0.0)
                } else {
                    let b0 = 1.0 / (1.0 + k);
                    (b0, -b0, 0.0, a1, 0.0)
                }
            }
            FilterKind::Lpf2p | FilterKind::Hpf2p | FilterKind::Bpf2p => {
                let q = std::f32::consts::FRAC_1_SQRT_2 * db_to_gain(resonance_db);
                let (sin, cos) = w0.sin_cos();
                let alpha = sin / (2.0 * q);
                let a0 = 1.0 + alpha;
                let (b0, b1, b2) = match kind {
                    FilterKind::Lpf2p => ((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0),
                    FilterKind::Hpf2p => ((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0),
                    _ => (alpha, 0.0, -alpha),
                };
                (b0 / a0, b1 / a0, b2 / a0, -2.0 * cos / a0, (1.0 - alpha) / a0)
            }
        };
        (self.b0, self.b1, self.b2, self.a1, self.a2) = (b0, b1, b2, a1, a2);
    }

    #[inline]
    fn process(&mut self, ch: usize, x: f32) -> f32 {
        let z = &mut self.z[ch];
        let y = self.b0 * x + z[0];
        z[0] = self.b1 * x - self.a1 * y + z[1];
        z[1] = self.b2 * x - self.a2 * y;
        y
    }
}

#[inline]
fn hermite(y0: f32, y1: f32, y2: f32, y3: f32, t: f32) -> f32 {
    let c1 = 0.5 * (y2 - y0);
    let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
    ((c3 * t + c2) * t + c1) * t + y1
}

#[derive(Clone, Copy)]
struct Voice {
    active: bool,
    region: u32,
    note: u8,
    vel: u8,
    group: i64,
    trigger: Trigger,
    /// Fractional frame position in the sample (excluding padding).
    pos: f64,
    /// Frames per output frame, before pitch bend.
    step: f64,
    gain: [f32; 2],
    env: Env,
    /// Synth voice: `step` is the base phase increment and `phases` the oscillators.
    synth: bool,
    phases: [f32; MAX_UNISON],
    /// Synth filter envelope (same attack/decay as the amplitude, falls back to 0).
    fenv: Env,
    /// Glide: current pitch / target pitch, easing to 1.
    glide_ratio: f32,
    glide_coef: f32,
    /// Position where the end-of-sample fade starts, and 1 / its length.
    tail_start: f64,
    tail_inv: f32,
    filter: Option<Biquad>,
    /// Key physically held down.
    key_down: bool,
    /// Key released while the sustain pedal was down.
    sustained: bool,
    /// Silenced by polyphony or `off_by`; already fading.
    offed: bool,
    started: u64,
}

impl Voice {
    const IDLE: Self = Self {
        active: false,
        region: 0,
        note: 0,
        vel: 0,
        group: 0,
        trigger: Trigger::Attack,
        pos: 0.0,
        step: 0.0,
        gain: [0.0; 2],
        env: Env::IDLE,
        synth: false,
        phases: [0.0; MAX_UNISON],
        fenv: Env::IDLE,
        glide_ratio: 1.0,
        glide_coef: 0.0,
        tail_start: 0.0,
        tail_inv: 0.0,
        filter: None,
        key_down: false,
        sustained: false,
        offed: false,
        started: 0,
    };
}

pub struct Engine {
    sr: f32,
    inst: Arc<Instrument>,
    voices: Vec<Voice>,
    ccs: [f32; CC_COUNT],
    bend: f32,
    user: User,
    /// Free-running vibrato phase (0..1).
    lfo_phase: f32,
    /// Pitch (phase increment) and start time of the last synth note, for glide.
    last_synth_note: Option<(f64, u64)>,
    /// Smoothed master gain.
    gain: f32,
    gain_step: f32,
    low_pass: Biquad,
    high_pass: Biquad,
    low_pass_hz: f32,
    low_pass_res: f32,
    high_pass_hz: f32,
    reverb: Reverb,
    /// Smoothed reverb send; the reverb is bypassed (and cleared) once it reaches 0.
    reverb_send: f32,
    reverb_running: bool,
    recorder: Option<Sender<RecMsg>>,
    sw_last: Option<u8>,
    /// Sounding note for each held input key (input key + transpose at note-on time).
    key_map: [Option<u8>; 128],
    key_vel: [u8; 128],
    key_time: [u64; 128],
    key_held: [bool; 128],
    /// Released while the sustain pedal was down: release samples wait for pedal up.
    release_pending: [bool; 128],
    /// Frames rendered so far.
    clock: u64,
    voice_counter: u64,
    rng: u32,
    peak: f32,
    peak_fall: f32,
    cmds: Receiver<EngineCmd>,
    garbage: Sender<Arc<Instrument>>,
    stats: Arc<Stats>,
}

impl Engine {
    pub fn new(
        sample_rate: u32,
        inst: Arc<Instrument>,
        cmds: Receiver<EngineCmd>,
        garbage: Sender<Arc<Instrument>>,
        stats: Arc<Stats>,
    ) -> Self {
        let sr = sample_rate as f32;
        Self {
            sr,
            ccs: inst.cc_defaults,
            sw_last: inst.sw_default,
            inst,
            voices: vec![Voice::IDLE; MAX_VOICES],
            bend: 1.0,
            user: User::new(&Params::default()),
            lfo_phase: 0.0,
            last_synth_note: None,
            gain: 1.0,
            gain_step: 1.0 - (-1.0 / (0.01 * sr)).exp(),
            low_pass: Biquad::new(FilterKind::Lpf2p, 20_000.0, 0.0, sr),
            high_pass: Biquad::new(FilterKind::Hpf2p, 20.0, 0.0, sr),
            low_pass_hz: 20_000.0,
            low_pass_res: 0.0,
            high_pass_hz: 20.0,
            reverb: Reverb::new(sr),
            reverb_send: 0.0,
            reverb_running: false,
            recorder: None,
            key_map: [None; 128],
            key_vel: [0; 128],
            key_time: [0; 128],
            key_held: [false; 128],
            release_pending: [false; 128],
            clock: 0,
            voice_counter: 0,
            rng: 0x9E37_79B9,
            peak: 0.0,
            peak_fall: exp_coef(2.0, sr),
            cmds,
            garbage,
            stats,
        }
    }

    /// Called once at the start of every audio callback, before rendering.
    pub fn begin_callback(&mut self, frames: usize, output_delay_us: u32) {
        let callback_start = Instant::now();
        while let Ok(cmd) = self.cmds.try_recv() {
            match cmd {
                EngineCmd::Midi { event, at } => {
                    let queue_us = callback_start.saturating_duration_since(at).as_micros() as u32;
                    self.stats.queue_us.store(queue_us, Relaxed);
                    if matches!(event, MidiEvent::NoteOn { .. }) {
                        let total = queue_us + output_delay_us;
                        self.stats.note_latency_us.store(total, Relaxed);
                        self.stats.note_latency_max_us.fetch_max(total, Relaxed);
                    }
                    self.handle(event);
                }
                EngineCmd::SetInstrument(inst) => self.set_instrument(inst),
                EngineCmd::Params(p) => self.user = User::new(&p),
                // The UI keeps its own sender, so dropping ours never wakes a thread from here.
                EngineCmd::Record(tx) => self.recorder = tx,
            }
        }
        self.stats.output_us.store(output_delay_us, Relaxed);
        self.stats.frames_per_callback.store(frames as u32, Relaxed);
        self.stats.callbacks.fetch_add(1, Relaxed);
    }

    fn set_instrument(&mut self, inst: Arc<Instrument>) {
        for v in &mut self.voices {
            v.active = false;
        }
        self.ccs = inst.cc_defaults;
        self.sw_last = inst.sw_default;
        self.release_pending = [false; 128];
        let old = std::mem::replace(&mut self.inst, inst);
        // If the UI side is gone or the queue is full we drop it here; rare and harmless.
        let _ = self.garbage.try_send(old);
    }

    fn handle(&mut self, event: MidiEvent) {
        match event {
            MidiEvent::NoteOn { note, vel, .. } => self.note_on(note, vel),
            MidiEvent::NoteOff { note, .. } => self.note_off(note),
            MidiEvent::ControlChange { cc, value, .. } => self.control(cc, value),
            MidiEvent::PitchBend { value, .. } => {
                self.bend = 2f32.powf(value as f32 / 8192.0 * BEND_RANGE_CENTS / 1200.0);
            }
            _ => {}
        }
    }

    fn random(&mut self) -> f32 {
        // xorshift32
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        (self.rng >> 8) as f32 / (1u32 << 24) as f32
    }

    fn is_keyswitch(&self, note: u8) -> bool {
        self.inst.sw_range.is_some_and(|(lo, hi)| (lo..=hi).contains(&note))
    }

    fn region_matches(&self, r: &Region, vel: u8, rand: f32) -> bool {
        (r.lovel..=r.hivel).contains(&vel)
            && rand >= r.lorand
            && (rand < r.hirand || r.hirand >= 1.0)
            && r.sw_last.is_none_or(|sw| self.sw_last == Some(sw))
            && r.cc_ranges.iter().all(|&(cc, lo, hi)| {
                let v = self.ccs.get(cc as usize).copied().unwrap_or(0.0);
                v >= lo - 1e-4 && v <= hi + 1e-4
            })
    }

    fn note_on(&mut self, key: u8, vel: u8) {
        if self.is_keyswitch(key) {
            self.sw_last = Some(key);
            return;
        }
        let Ok(note) = u8::try_from(key as i32 + self.user.transpose) else { return };
        if note > 127 {
            return;
        }
        let vel = self.user.velocity(vel);
        self.key_map[key as usize] = Some(note);
        let n = note as usize;
        self.key_vel[n] = vel;
        self.key_time[n] = self.clock;
        self.key_held[n] = true;
        self.release_pending[n] = false;

        let inst = self.inst.clone();
        if inst.synth.is_some() {
            self.start_synth_voice(&inst, note, vel);
            return;
        }
        let rand = self.random();
        for &ri in &inst.by_key[n] {
            let r = &inst.regions[ri as usize];
            if r.trigger == Trigger::Attack && self.region_matches(r, vel, rand) {
                self.start_voice(&inst, ri, note, vel, 1.0);
            }
        }
    }

    fn note_off(&mut self, key: u8) {
        if self.is_keyswitch(key) {
            return;
        }
        // Use the transpose of the note-on, even if it changed while the key was down.
        let Some(note) = self.key_map[key as usize].take() else { return };
        let n = note as usize;
        self.key_held[n] = false;
        let sustain = self.sustain_down();
        let inst = self.inst.clone();
        for i in 0..self.voices.len() {
            let v = &mut self.voices[i];
            if !v.active || !v.key_down || v.note != note {
                continue;
            }
            v.key_down = false;
            if !v.synth && inst.regions[v.region as usize].one_shot {
                continue;
            }
            if sustain {
                v.sustained = true;
            } else {
                self.release_voice(&inst, i);
            }
        }
        if sustain {
            self.release_pending[n] = true;
        } else {
            self.fire_release_samples(&inst, note);
        }
    }

    fn sustain_down(&self) -> bool {
        self.ccs.get(self.inst.sustain_cc as usize).is_some_and(|&v| v >= 0.5)
    }

    fn control(&mut self, cc: u8, value: u8) {
        self.apply_cc(cc as usize, value as f32 / 127.0);
    }

    /// Sets controller `c` to `new` (0..1) and runs everything that depends on it.
    fn apply_cc(&mut self, c: usize, new: f32) {
        let old = self.ccs[c];
        if old == new {
            return;
        }
        self.ccs[c] = new;
        let inst = self.inst.clone();

        if c == ALL_NOTES_OFF_CC as usize || c == ALL_SOUND_OFF_CC as usize {
            for i in 0..self.voices.len() {
                if self.voices[i].active {
                    self.voices[i].key_down = false;
                    self.release_voice(&inst, i);
                }
            }
            return;
        }

        if c == inst.sustain_cc as usize && old >= 0.5 && new < 0.5 {
            for i in 0..self.voices.len() {
                let v = &self.voices[i];
                if v.active && v.sustained && !v.key_down {
                    self.release_voice(&inst, i);
                }
            }
            for note in 0..128u8 {
                if self.release_pending[note as usize] && !self.key_held[note as usize] {
                    self.release_pending[note as usize] = false;
                    self.fire_release_samples(&inst, note);
                }
            }
        }

        // Regions triggered by this controller entering their range (pedal noises).
        let rand = self.random();
        for &ri in &inst.cc_triggered {
            let r = &inst.regions[ri as usize];
            let Some((on_cc, lo, hi)) = r.on_cc else { continue };
            let inside = |v: f32| v >= lo - 1e-4 && v <= hi + 1e-4;
            if on_cc as usize == c && inside(new) && !inside(old) && self.region_matches(r, 127, rand) {
                let note = r.pitch_keycenter.clamp(0.0, 127.0) as u8;
                self.start_voice(&inst, ri, note, 127, 1.0);
            }
        }

        // Half-pedalling: release times follow the controllers while fading.
        for i in 0..self.voices.len() {
            let v = &self.voices[i];
            // Check `active` first: idle slots may point at regions a synth doesn't have.
            if v.active
                && !v.synth
                && !v.offed
                && v.env.stage == Stage::Release
                && inst.regions[v.region as usize].ampeg.dynamic
            {
                self.release_voice(&inst, i);
            }
        }
    }

    fn ctx<'a>(&'a self, inst: &'a Instrument, note: u8, vel: u8) -> Ctx<'a> {
        Ctx { ccs: &self.ccs, curves: &inst.curves, note, vel }
    }

    fn release_voice(&mut self, inst: &Instrument, i: usize) {
        let v = self.voices[i];
        let seconds = if v.synth {
            self.user.release
        } else {
            inst.regions[v.region as usize].ampeg.release.eval(&self.ctx(inst, v.note, v.vel))
        };
        self.voices[i].env.release(seconds.max(0.0005), self.sr);
    }

    /// Fades a voice out because something else took its place.
    fn off_voice(&mut self, inst: &Instrument, i: usize) {
        let off_time = if self.voices[i].synth { None } else { inst.regions[self.voices[i].region as usize].off_time };
        match off_time {
            Some(t) => {
                self.voices[i].env.release(t.max(0.0005), self.sr);
            }
            None => self.release_voice(inst, i),
        }
        self.voices[i].offed = true;
    }

    fn fire_release_samples(&mut self, inst: &Arc<Instrument>, note: u8) {
        let n = note as usize;
        let vel = self.key_vel[n];
        let held_s = (self.clock - self.key_time[n]) as f32 / self.sr;
        let rand = self.random();
        for &ri in &inst.by_key[n] {
            let r = &inst.regions[ri as usize];
            if r.trigger == Trigger::Release && self.region_matches(r, vel, rand) {
                let attenuation = db_to_gain(-r.rt_decay * held_s);
                self.start_voice(inst, ri, note, vel, attenuation);
            }
        }
    }

    /// Keeps at most `keep` live voices matching `pred`, fading out the oldest.
    fn limit_voices(&mut self, inst: &Instrument, keep: u32, pred: impl Fn(&Voice) -> bool) {
        loop {
            let live = |v: &&Voice| v.active && !v.offed && pred(v);
            if self.voices.iter().filter(live).count() as u32 <= keep {
                return;
            }
            let oldest = (0..self.voices.len())
                .filter(|&i| live(&&self.voices[i]))
                .min_by_key(|&i| self.voices[i].started);
            match oldest {
                Some(i) => self.off_voice(inst, i),
                None => return,
            }
        }
    }

    fn start_voice(&mut self, inst: &Arc<Instrument>, ri: u32, note: u8, vel: u8, extra_gain: f32) {
        let r = &inst.regions[ri as usize];
        if let Some(np) = r.note_polyphony {
            let (group, trigger) = (r.group, r.trigger);
            self.limit_voices(inst, np - 1, |v| v.note == note && v.group == group && v.trigger == trigger);
        }
        if let Some(p) = r.polyphony {
            let group = r.group;
            self.limit_voices(inst, p - 1, |v| v.group == group);
        }
        if r.group != 0 {
            for i in 0..self.voices.len() {
                let v = &self.voices[i];
                if v.active && !v.offed && !v.synth && inst.regions[v.region as usize].off_by == Some(r.group) {
                    self.off_voice(inst, i);
                }
            }
        }

        let ctx = self.ctx(inst, note, vel);
        let sample = &inst.samples[r.sample];
        let veltrack = (r.amp_veltrack.eval(&ctx) / 100.0).clamp(-1.0, 1.0);
        let vel_curve = (vel as f32 / 127.0).powi(2);
        let vel_gain = if veltrack >= 0.0 { 1.0 - veltrack * (1.0 - vel_curve) } else { 1.0 + veltrack * vel_curve };
        let pan = (r.pan.eval(&ctx) / 100.0).clamp(-1.0, 1.0);
        let gain = r.amplitude.eval_product(&ctx)
            * db_to_gain(r.volume.eval(&ctx))
            * vel_gain
            * extra_gain
            * sample.gain;
        let cents = (note as f32 - r.pitch_keycenter) * r.pitch_keytrack + r.tune.eval(&ctx);
        let step = 2f64.powf(cents as f64 / 1200.0) * sample.rate as f64 / self.sr as f64;
        let e = &r.ampeg;
        let env = Env::new(
            e.delay.eval(&ctx),
            e.attack.eval(&ctx),
            e.hold.eval(&ctx),
            e.decay.eval(&ctx),
            e.sustain.eval(&ctx),
            self.sr,
        );
        let filter = r.filter.as_ref().map(|f| {
            let cents = f.cutoff_cents.eval(&ctx) + f.veltrack_cents * vel as f32 / 127.0;
            Biquad::new(f.kind, f.cutoff_hz * 2f32.powf(cents / 1200.0), f.resonance_db, self.sr)
        });
        let offset = r.offset.eval(&ctx).max(0.0) as f64;
        let tail_len = ((TAIL_FADE_S * sample.rate as f32) as usize).min(sample.frames / 8).max(1);

        self.voice_counter += 1;
        let voice = Voice {
            active: true,
            region: ri,
            note,
            vel,
            group: r.group,
            trigger: r.trigger,
            pos: offset.min(sample.frames.saturating_sub(1) as f64),
            step,
            gain: [gain * (1.0 - pan).min(1.0), gain * (1.0 + pan).min(1.0)],
            env,
            synth: false,
            phases: [0.0; MAX_UNISON],
            fenv: Env::IDLE,
            glide_ratio: 1.0,
            glide_coef: 0.0,
            tail_start: (sample.frames - tail_len) as f64,
            tail_inv: 1.0 / tail_len as f32,
            filter,
            key_down: r.trigger == Trigger::Attack,
            sustained: false,
            offed: false,
            started: self.voice_counter,
        };
        self.place_voice(voice);
    }

    /// Puts a new voice in a free slot, or in place of the quietest one.
    fn place_voice(&mut self, voice: Voice) {
        let slot = self.voices.iter().position(|v| !v.active).unwrap_or_else(|| {
            (0..self.voices.len())
                .min_by(|&a, &b| {
                    let level = |v: &Voice| v.env.level * (v.gain[0] + v.gain[1]);
                    level(&self.voices[a]).total_cmp(&level(&self.voices[b]))
                })
                .unwrap_or(0)
        });
        self.voices[slot] = voice;
    }

    fn start_synth_voice(&mut self, inst: &Instrument, note: u8, vel: u8) {
        let Some(patch) = &inst.synth else { return };
        let freq = 440.0 * 2f64.powf((note as f64 - 69.0) / 12.0);
        let mut phases = [0.0; MAX_UNISON];
        if patch.unison > 1 {
            // Free-running feel: unison oscillators start out of phase.
            for p in phases.iter_mut().take(patch.unison) {
                *p = self.random();
            }
        }
        let u = self.user;
        let step = freq / self.sr as f64;
        // Legato glide: slide from the previous note if a key is still down and it wasn't
        // struck together with this one (a chord).
        let chord_frames = (GLIDE_CHORD_S * self.sr) as u64;
        let legato = self.voices.iter().any(|v| v.active && v.synth && v.key_down);
        let glide_from = match self.last_synth_note {
            Some((prev, at)) if u.glide > 0.0 && legato && self.clock.saturating_sub(at) > chord_frames => {
                (prev / step) as f32
            }
            _ => 1.0,
        };
        self.last_synth_note = Some((step, self.clock));
        let gain = patch.unison_gain() * (vel as f32 / 127.0).powi(2);
        self.voice_counter += 1;
        self.place_voice(Voice {
            active: true,
            note,
            vel,
            step,
            gain: [gain; 2],
            env: Env::new(0.0, u.attack, 0.0, u.decay, u.sustain * 100.0, self.sr),
            synth: true,
            phases,
            fenv: Env::new(0.0, u.attack, 0.0, u.decay, 0.0, self.sr),
            glide_ratio: glide_from,
            glide_coef: exp_coef(u.glide, self.sr),
            filter: Some(Biquad::new(FilterKind::Lpf2p, 20_000.0, 0.0, self.sr)),
            key_down: true,
            started: self.voice_counter,
            ..Voice::IDLE
        });
    }

    /// Fills `out` with stereo frames.
    pub fn render(&mut self, out: &mut [[f32; 2]]) {
        out.fill([0.0; 2]);
        self.update_master_filters(out.len());
        let inst = &*self.inst;
        let bend = (self.bend * self.user.detune) as f64;
        // Vibrato for synth voices, updated once per block.
        self.lfo_phase = (self.lfo_phase + out.len() as f32 * self.user.lfo_rate / self.sr).fract();
        let vibrato = 2f64.powf((self.user.lfo_cents * (self.lfo_phase * std::f32::consts::TAU).sin()) as f64 / 1200.0);
        // Unison layout of the current synth patch: pitch ratio and L/R gains per oscillator.
        let mut unison = [(1.0f64, 1.0f32, 1.0f32); MAX_UNISON];
        let (waveform, oscillators) = match &inst.synth {
            Some(p) => {
                for (k, u) in unison.iter_mut().enumerate().take(p.unison) {
                    let (ratio, pan) = p.unison_voice(k);
                    *u = (ratio as f64, (1.0 - pan).min(1.0), (1.0 + pan).min(1.0));
                }
                (p.waveform, p.unison)
            }
            None => (crate::synth::Waveform::Sine, 0),
        };
        // Synths filter each voice (so the filter envelope can sweep it); pianos use the
        // master low-pass below.
        let filter_on = inst.synth.is_some() && self.low_pass_hz < LOW_PASS_OFF_HZ;
        let mut active = 0;
        for v in self.voices.iter_mut().filter(|v| v.active) {
            if v.synth {
                let step = v.step * bend * vibrato;
                for (n, frame) in out.iter_mut().enumerate() {
                    let e = v.env.next();
                    if v.env.stage == Stage::Done {
                        v.active = false;
                        break;
                    }
                    let fe = v.fenv.next();
                    if filter_on && n % FILTER_UPDATE_FRAMES == 0 {
                        let cutoff = self.low_pass_hz * 2f32.powf(self.user.filter_env_oct * fe);
                        if let Some(f) = v.filter.as_mut() {
                            f.set(FilterKind::Lpf2p, cutoff, self.low_pass_res, self.sr);
                        }
                    }
                    v.glide_ratio = 1.0 + (v.glide_ratio - 1.0) * v.glide_coef;
                    let step = step * v.glide_ratio as f64;
                    let (mut l, mut r) = (0.0, 0.0);
                    for k in 0..oscillators {
                        let (ratio, gl, gr) = unison[k];
                        let dt = (step * ratio) as f32;
                        let x = oscillator(waveform, v.phases[k], dt);
                        v.phases[k] += dt;
                        if v.phases[k] >= 1.0 {
                            v.phases[k] -= 1.0;
                        }
                        l += x * gl;
                        r += x * gr;
                    }
                    if filter_on {
                        if let Some(f) = v.filter.as_mut() {
                            l = f.process(0, l);
                            r = f.process(1, r);
                        }
                    }
                    frame[0] += l * v.gain[0] * e;
                    frame[1] += r * v.gain[1] * e;
                }
                if v.active {
                    active += 1;
                }
                continue;
            }
            let s = &inst.samples[inst.regions[v.region as usize].sample];
            let d = &s.data;
            let step = v.step * bend;
            for frame in out.iter_mut() {
                let e = v.env.next();
                let i = v.pos as usize;
                if v.env.stage == Stage::Done || i >= s.frames {
                    v.active = false;
                    break;
                }
                let t = (v.pos - i as f64) as f32;
                // Frame i lives at (i + PAD_START) * channels, so i - 1 starts at i * channels.
                let (mut l, mut r) = if s.channels == 2 {
                    let b = i * 2;
                    (
                        hermite(d[b] as f32, d[b + 2] as f32, d[b + 4] as f32, d[b + 6] as f32, t),
                        hermite(d[b + 1] as f32, d[b + 3] as f32, d[b + 5] as f32, d[b + 7] as f32, t),
                    )
                } else {
                    let m = hermite(d[i] as f32, d[i + 1] as f32, d[i + 2] as f32, d[i + 3] as f32, t);
                    (m, m)
                };
                if let Some(f) = v.filter.as_mut() {
                    l = f.process(0, l);
                    r = f.process(1, r);
                }
                let mut e = e;
                if v.pos > v.tail_start {
                    e *= ((s.frames as f64 - v.pos) as f32 * v.tail_inv).clamp(0.0, 1.0);
                }
                frame[0] += l * v.gain[0] * e;
                frame[1] += r * v.gain[1] * e;
                v.pos += step;
            }
            if v.active {
                active += 1;
            }
        }

        let lp_on = self.inst.synth.is_none() && self.low_pass_hz < LOW_PASS_OFF_HZ;
        let hp_on = self.high_pass_hz > HIGH_PASS_OFF_HZ;
        let reverb_wanted = self.user.reverb > 0.0;
        if reverb_wanted {
            self.reverb_running = true;
        } else if self.reverb_running && self.reverb_send < 1e-4 {
            // Fully faded out: stop processing, and drop the old tail so it can't come back.
            self.reverb_running = false;
            self.reverb_send = 0.0;
            self.reverb.clear();
        }
        for frame in out.iter_mut() {
            self.gain += (self.user.gain - self.gain) * self.gain_step;
            for (c, s) in frame.iter_mut().enumerate() {
                if lp_on {
                    *s = self.low_pass.process(c, *s);
                }
                if hp_on {
                    *s = self.high_pass.process(c, *s);
                }
            }
            if self.reverb_running {
                self.reverb_send += (self.user.reverb - self.reverb_send) * self.gain_step;
                let [wl, wr] = self.reverb.process((frame[0] + frame[1]) * 0.5);
                frame[0] += wl * self.reverb_send;
                frame[1] += wr * self.reverb_send;
            }
            for s in frame.iter_mut() {
                *s = soft_clip(*s * self.gain * MASTER_GAIN);
            }
            let level = frame[0].abs().max(frame[1].abs());
            self.peak = if level > self.peak { level } else { self.peak * self.peak_fall };
        }
        if let Some(tx) = &self.recorder {
            let mut block = AudioBlock { len: out.len().min(BLOCK_FRAMES), frames: [[0.0; 2]; BLOCK_FRAMES] };
            block.frames[..block.len].copy_from_slice(&out[..block.len]);
            if tx.try_send(RecMsg::Audio(block)).is_err() {
                self.stats.rec_dropped.fetch_add(1, Relaxed);
            }
        }
        self.clock += out.len() as u64;
        self.stats.active_voices.store(active, Relaxed);
        self.stats.peak.store(self.peak.to_bits(), Relaxed);
    }
}

impl Engine {
    /// Glides the master filter cutoffs towards their targets (~30 ms, log domain).
    fn update_master_filters(&mut self, frames: usize) {
        let k = 1.0 - (-(frames as f32) / (0.03 * self.sr)).exp();
        let glide = |current: &mut f32, target: f32| -> bool {
            if (*current / target - 1.0).abs() < 1e-3 {
                let changed = *current != target;
                *current = target;
                return changed;
            }
            *current = (current.ln() + (target.ln() - current.ln()) * k).exp();
            true
        };
        let res_changed = self.low_pass_res != self.user.resonance_db;
        if glide(&mut self.low_pass_hz, self.user.low_pass_hz) || res_changed {
            self.low_pass_res = self.user.resonance_db;
            self.low_pass.set(FilterKind::Lpf2p, self.low_pass_hz, self.low_pass_res, self.sr);
        }
        if glide(&mut self.high_pass_hz, self.user.high_pass_hz) {
            self.high_pass.set(FilterKind::Hpf2p, self.high_pass_hz, 0.0, self.sr);
        }
    }
}

/// Untouched below this level (-2 dBFS); above it the signal bends smoothly towards ±1.
const CLIP_KNEE: f32 = 0.8;

/// Safety limiter: exactly linear up to `CLIP_KNEE`, then a tanh-shaped knee that never
/// exceeds ±1. Normal playing never reaches the knee, so it adds no colouring.
fn soft_clip(x: f32) -> f32 {
    let a = x.abs();
    if a <= CLIP_KNEE {
        return x;
    }
    let room = 1.0 - CLIP_KNEE;
    let over = ((a - CLIP_KNEE) / room).min(3.0);
    // Rational tanh approximation, reaches exactly 1 at 3.
    let bent = over * (27.0 + over * over) / (27.0 + 9.0 * over * over);
    (CLIP_KNEE + room * bent).copysign(x)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::{CcMod, Param};

    const SR: u32 = 48_000;

    struct Rig {
        engine: Engine,
        tx: Sender<EngineCmd>,
        buf: Vec<[f32; 2]>,
    }

    impl Rig {
        fn new(inst: Instrument) -> Self {
            let (tx, rx) = crossbeam_channel::bounded(256);
            let (gtx, _grx) = crossbeam_channel::bounded(4);
            let engine = Engine::new(SR, Arc::new(inst), rx, gtx, Arc::new(Stats::default()));
            Self { engine, tx, buf: vec![[0.0; 2]; 256] }
        }

        fn send(&mut self, event: MidiEvent) {
            self.tx.send(EngineCmd::Midi { event, at: Instant::now() }).unwrap();
            self.engine.begin_callback(self.buf.len(), 0);
        }

        fn run(&mut self, seconds: f32) -> f32 {
            let blocks = (seconds * SR as f32 / self.buf.len() as f32).ceil() as usize;
            let mut peak = 0.0f32;
            for _ in 0..blocks {
                self.engine.render(&mut self.buf);
                peak = self.buf.iter().fold(peak, |p, f| p.max(f[0].abs()));
            }
            peak
        }

        fn active(&self) -> usize {
            self.engine.voices.iter().filter(|v| v.active).count()
        }

        fn set(&mut self, edits: &[(ParamId, i32)]) {
            let mut p = Params::default();
            for &(id, steps) in edits {
                p.nudge(id, steps);
            }
            self.tx.send(EngineCmd::Params(p)).unwrap();
            self.engine.begin_callback(self.buf.len(), 0);
        }

        /// RMS of the left channel over `seconds`.
        fn rms(&mut self, seconds: f32) -> f32 {
            let blocks = (seconds * SR as f32 / self.buf.len() as f32).ceil() as usize;
            let mut sum = 0.0f64;
            for _ in 0..blocks {
                self.engine.render(&mut self.buf);
                sum += self.buf.iter().map(|f| (f[0] as f64).powi(2)).sum::<f64>();
            }
            (sum / (blocks * self.buf.len()) as f64).sqrt() as f32
        }
    }

    fn on(note: u8, vel: u8) -> MidiEvent {
        MidiEvent::NoteOn { ch: 0, note, vel }
    }
    fn off(note: u8) -> MidiEvent {
        MidiEvent::NoteOff { ch: 0, note, vel: 0 }
    }
    fn cc(cc: u8, value: u8) -> MidiEvent {
        MidiEvent::ControlChange { ch: 0, cc, value }
    }

    #[test]
    fn note_sounds_then_releases() {
        let mut rig = Rig::new(Instrument::builtin_sine(SR));
        rig.send(on(69, 100));
        assert!(rig.run(0.05) > 0.01);
        assert_eq!(rig.active(), 1);
        rig.send(off(69));
        rig.run(0.5);
        assert_eq!(rig.active(), 0);
    }

    #[test]
    fn sustain_pedal_holds_notes() {
        let mut rig = Rig::new(Instrument::builtin_sine(SR));
        rig.send(cc(64, 127));
        rig.send(on(60, 100));
        rig.send(off(60));
        rig.run(0.3);
        assert_eq!(rig.active(), 1, "pedal down keeps the note ringing");
        rig.send(cc(64, 0));
        rig.run(0.5);
        assert_eq!(rig.active(), 0);
    }

    #[test]
    fn pitch_follows_key() {
        // A4 sample on its own key plays at step 1; an octave up plays twice as fast.
        let mut rig = Rig::new(Instrument::builtin_sine(SR));
        rig.send(on(69, 100));
        rig.send(on(81, 100));
        let steps: Vec<f64> = rig.engine.voices.iter().filter(|v| v.active).map(|v| v.step).collect();
        assert!((steps[0] - 1.0).abs() < 1e-9 && (steps[1] - 2.0).abs() < 1e-9, "{steps:?}");
    }

    #[test]
    fn velocity_scales_gain() {
        let mut rig = Rig::new(Instrument::builtin_sine(SR));
        rig.send(on(60, 127));
        rig.send(on(62, 64));
        let g: Vec<f32> = rig.engine.voices.iter().filter(|v| v.active).map(|v| v.gain[0]).collect();
        let ratio = g[1] / g[0];
        assert!((ratio - (64.0f32 / 127.0).powi(2)).abs() < 1e-3, "{ratio}");
    }

    #[test]
    fn note_polyphony_fades_restruck_note() {
        let mut inst = Instrument::builtin_sine(SR);
        inst.regions[0].note_polyphony = Some(1);
        inst.regions[0].off_time = Some(0.05);
        let mut rig = Rig::new(inst);
        rig.send(on(60, 100));
        rig.send(on(60, 100));
        assert_eq!(rig.active(), 2);
        assert_eq!(rig.engine.voices.iter().filter(|v| v.offed).count(), 1);
        rig.run(0.3);
        assert_eq!(rig.active(), 1);
    }

    #[test]
    fn release_samples_fire_and_decay_with_hold_time() {
        let mut inst = Instrument::builtin_sine(SR);
        let mut rel = inst.regions[0].clone();
        rel.trigger = Trigger::Release;
        rel.rt_decay = 20.0;
        inst.regions.push(rel);
        let inst = {
            // Rebuild key lookup with the extra region.
            let mut i = inst;
            i.by_key = vec![vec![0, 1]; 128];
            i
        };
        let mut rig = Rig::new(inst);
        rig.send(on(60, 100));
        rig.run(0.5);
        rig.send(off(60));
        let rel_voice = rig.engine.voices.iter().find(|v| v.active && v.trigger == Trigger::Release).unwrap();
        let attack_gain = (100.0f32 / 127.0).powi(2) * db_to_gain(-6.0) / 32767.0;
        // Held ~0.5 s at 20 dB/s → about -10 dB.
        let db = 20.0 * (rel_voice.gain[0] / attack_gain).log10();
        assert!((db + 10.0).abs() < 0.5, "{db}");
    }

    #[test]
    fn release_samples_wait_for_pedal_up() {
        let mut inst = Instrument::builtin_sine(SR);
        let mut rel = inst.regions[0].clone();
        rel.trigger = Trigger::Release;
        inst.regions.push(rel);
        inst.by_key = vec![vec![0, 1]; 128];
        let mut rig = Rig::new(inst);
        rig.send(cc(64, 127));
        rig.send(on(60, 100));
        rig.send(off(60));
        let releases = |r: &Rig| r.engine.voices.iter().filter(|v| v.active && v.trigger == Trigger::Release).count();
        assert_eq!(releases(&rig), 0);
        rig.send(cc(64, 0));
        assert_eq!(releases(&rig), 1);
    }

    #[test]
    fn rerouted_sustain_uses_cc_release_time() {
        // Splendid style: sustain_cc moved away, pedal lengthens the release instead.
        let mut inst = Instrument::builtin_sine(SR);
        inst.sustain_cc = 90;
        inst.regions[0].ampeg.release =
            Param { base: 0.05, mods: vec![CcMod { cc: 64, depth: 10.0, curve: 0 }] };
        inst.regions[0].ampeg.dynamic = true;
        let mut rig = Rig::new(inst);
        rig.send(cc(64, 127));
        rig.send(on(69, 100));
        rig.send(off(69));
        rig.run(0.5);
        assert_eq!(rig.active(), 1, "long release while pedal down");
        rig.send(cc(64, 0));
        rig.run(0.5);
        assert_eq!(rig.active(), 0, "release shortens when the pedal lifts");
    }

    #[test]
    fn keyswitch_selects_regions() {
        let mut inst = Instrument::builtin_sine(SR);
        let mut alt = inst.regions[0].clone();
        inst.regions[0].sw_last = Some(12);
        alt.sw_last = Some(13);
        alt.volume = Param::constant(-20.0);
        inst.regions.push(alt);
        inst.by_key = vec![vec![0, 1]; 128];
        inst.sw_range = Some((12, 13));
        inst.sw_default = Some(12);
        let mut rig = Rig::new(inst);
        rig.send(on(60, 100));
        rig.send(on(13, 100));
        rig.send(on(62, 100));
        let regions: Vec<u32> = rig.engine.voices.iter().filter(|v| v.active).map(|v| v.region).collect();
        assert_eq!(regions, vec![0, 1]);
    }

    #[test]
    fn output_stays_bounded_with_many_voices() {
        let mut rig = Rig::new(Instrument::builtin_sine(SR));
        for note in 0..128 {
            rig.send(on(note, 127));
        }
        assert!(rig.run(0.1) <= 1.0 + 1e-6);
        assert_eq!(rig.active(), MAX_VOICES);
    }

    #[test]
    fn transpose_plays_shifted_note_and_releases_the_right_voice() {
        let mut rig = Rig::new(Instrument::builtin_sine(SR));
        rig.set(&[(ParamId::Transpose, 12)]);
        rig.send(on(57, 100));
        let v = rig.engine.voices.iter().find(|v| v.active).unwrap();
        assert_eq!(v.note, 69);
        assert!((v.step - 1.0).abs() < 1e-9, "A3 + 12 plays the A4 sample unshifted");
        // Changing transpose while held must not orphan the note.
        rig.set(&[(ParamId::Transpose, -3)]);
        rig.send(off(57));
        rig.run(0.5);
        assert_eq!(rig.active(), 0);
    }

    #[test]
    fn velocity_curve_reshapes_touch() {
        let mut rig = Rig::new(Instrument::builtin_sine(SR));
        rig.set(&[(ParamId::Velocity, 50)]); // light 100 → gamma 0.25
        rig.send(on(60, 32));
        let v = rig.engine.voices.iter().find(|v| v.active).unwrap();
        let expected = ((32.0f32 / 127.0).powf(0.25) * 127.0).round() as u8;
        assert_eq!(v.vel, expected);
        assert!(v.vel > 80);
    }

    #[test]
    fn volume_scales_output() {
        let mut rig = Rig::new(Instrument::builtin_sine(SR));
        rig.send(on(69, 100));
        let base = rig.rms(0.1);
        rig.set(&[(ParamId::Volume, -12)]); // -6 dB
        rig.run(0.05); // let the gain glide settle
        let quieter = rig.rms(0.1);
        // The sample decays on its own, so compare to its expected natural decay too.
        let db = 20.0 * (quieter / base).log10();
        assert!(db < -6.0 && db > -9.0, "{db}");
    }

    #[test]
    fn detune_changes_pitch_live() {
        let mut rig = Rig::new(Instrument::builtin_sine(SR));
        rig.send(on(69, 100));
        rig.set(&[(ParamId::Detune, 200)]); // +100 cents
        assert!((rig.engine.user.detune - 2f32.powf(100.0 / 1200.0)).abs() < 1e-6);
        let pos0 = rig.engine.voices.iter().find(|v| v.active).unwrap().pos;
        rig.run(0.01);
        let pos1 = rig.engine.voices.iter().find(|v| v.active).unwrap().pos;
        let frames = (0.01f32 * SR as f32 / 256.0).ceil() as f64 * 256.0;
        assert!(((pos1 - pos0) / frames - 2f64.powf(100.0 / 1200.0)).abs() < 1e-4);
    }

    #[test]
    fn reverb_adds_a_tail_and_bypasses_at_zero() {
        let mut rig = Rig::new(Instrument::builtin_sine(SR));
        rig.send(on(69, 100));
        rig.send(off(69));
        rig.run(0.6);
        assert_eq!(rig.active(), 0);
        assert!(rig.rms(0.1) == 0.0, "dry: silence once the voice is gone");

        let mut rig = Rig::new(Instrument::builtin_sine(SR));
        rig.set(&[(ParamId::Reverb, 50)]);
        rig.send(on(69, 100));
        rig.send(off(69));
        rig.run(0.6);
        assert_eq!(rig.active(), 0);
        assert!(rig.rms(0.1) > 1e-4, "reverb tail still audible after the note ended");

        rig.set(&[]); // back to 0 %
        rig.run(0.2);
        assert!(!rig.engine.reverb_running);
        assert_eq!(rig.rms(0.1), 0.0);
    }

    #[test]
    fn samples_fade_out_at_their_end() {
        let mut rig = Rig::new(Instrument::builtin_sine(SR));
        rig.send(on(69, 127));
        let frames = rig.engine.inst.samples[0].frames;
        // Jump close to the end of the 3 s sample.
        rig.engine.voices.iter_mut().find(|v| v.active).unwrap().pos = (frames - 2400) as f64;
        rig.run(0.005); // 240 frames in: 2160 left of a 6000-frame fade
        let last = rig.buf.last().unwrap()[0].abs();
        let v = rig.engine.voices.iter().find(|v| v.active).unwrap();
        let g = v.gain[0] * v.env.level * 32767.0;
        assert!(last <= g * 0.4, "fading near the end: {last} vs {g}");
        rig.run(0.1);
        assert_eq!(rig.active(), 0);
    }

    fn synth(name: &str) -> Instrument {
        let patch = crate::synth::PRESETS.iter().find(|p| p.name == name).unwrap().clone();
        Instrument::synth(patch)
    }

    #[test]
    fn synth_follows_its_adsr() {
        let mut rig = Rig::new(synth("Saw"));
        // Attack 0 → instant, Decay default 0.4 s to Sustain 50 %, Release 0.01 s.
        rig.set(&[(ParamId::Attack, -100), (ParamId::Sustain, -20), (ParamId::Release, -100)]);
        rig.send(on(57, 127));
        rig.run(3.0);
        let v = rig.engine.voices.iter().find(|v| v.active).unwrap();
        assert!((v.env.level - 0.5).abs() < 0.01, "held at the sustain level: {}", v.env.level);
        assert!(rig.rms(0.1) > 0.01, "a held synth note never runs out");
        rig.send(off(57));
        rig.run(0.05);
        assert_eq!(rig.active(), 0, "10 ms release");
    }

    #[test]
    fn synth_attack_ramps_up() {
        let mut rig = Rig::new(synth("Sine"));
        rig.set(&[(ParamId::Attack, 50)]); // ~1.4 s
        let attack = rig.engine.user.attack;
        assert!(attack > 1.0 && attack < 2.0, "{attack}");
        rig.send(on(69, 127));
        rig.run(0.25);
        let elapsed = (0.25f32 * SR as f32 / 256.0).ceil() * 256.0 / SR as f32;
        let level = rig.engine.voices.iter().find(|v| v.active).unwrap().env.level;
        assert!((level - elapsed / attack).abs() < 0.01, "linear attack: {level}");
    }

    #[test]
    fn synth_pitch_and_unison() {
        let mut rig = Rig::new(synth("Super saw"));
        rig.send(on(69, 100));
        let v = *rig.engine.voices.iter().find(|v| v.active).unwrap();
        assert!((v.step - 440.0 / SR as f64).abs() < 1e-12);
        assert!(v.phases.iter().filter(|&&p| p != 0.0).count() >= 6, "unison phases randomised");
        let out = rig.rms(0.1);
        assert!(out > 0.01 && out < 0.5, "{out}");
        // Wide unison: left and right differ.
        assert!(rig.buf.iter().any(|f| (f[0] - f[1]).abs() > 1e-3));
    }

    #[test]
    fn sampled_instruments_ignore_the_envelope_cells() {
        let mut rig = Rig::new(Instrument::builtin_sine(SR));
        rig.set(&[(ParamId::Attack, 100), (ParamId::Release, 100), (ParamId::Sustain, -100)]);
        rig.send(on(69, 100));
        let level = { rig.run(0.01); rig.engine.voices[0].env.level };
        assert_eq!(level, 1.0, "no added attack");
        rig.send(off(69));
        rig.run(0.5);
        assert_eq!(rig.active(), 0, "instrument's own 80 ms release, not 10 s");
    }

    #[test]
    fn lfo_wobbles_synth_pitch() {
        let mut rig = Rig::new(synth("Sine"));
        rig.set(&[(ParamId::Lfo, 100)]); // ±100 cents
        rig.send(on(69, 100));
        // Measure phase advance per block over one LFO period: it must swing ~±6 %.
        let (mut lo, mut hi) = (f32::MAX, 0.0f32);
        for _ in 0..40 {
            let before = rig.engine.voices[0].phases[0];
            rig.engine.render(&mut rig.buf);
            let mut adv = rig.engine.voices[0].phases[0] - before;
            if adv < 0.0 {
                adv += 1.0;
            }
            let cycles = adv + (440.0 * 256.0 / SR as f32).floor();
            lo = lo.min(cycles);
            hi = hi.max(cycles);
        }
        let swing = hi / lo;
        assert!(swing > 1.08 && swing < 1.16, "max/min pitch ratio {swing}");
    }

    #[test]
    fn resonance_boosts_around_the_cutoff() {
        let level_at_cutoff = |res_steps: i32| {
            let mut rig = Rig::new(synth("Sine"));
            rig.set(&[(ParamId::LowPass, -60), (ParamId::Resonance, res_steps)]); // ~1.4 kHz
            let cutoff = rig.engine.user.low_pass_hz;
            let note = (69.0 + 12.0 * (cutoff / 440.0).log2()).round() as u8;
            rig.send(on(note, 127));
            rig.run(0.2);
            rig.rms(0.1)
        };
        let flat = level_at_cutoff(0);
        let peaked = level_at_cutoff(48); // 12 dB
        let gain_db = 20.0 * (peaked / flat).log10();
        assert!(gain_db > 6.0, "resonance gain at cutoff: {gain_db} dB");
    }

    /// Throws a long random stream of MIDI and matrix changes at every kind of instrument.
    /// Guards against out-of-bounds lookups that would kill the audio thread.
    #[test]
    fn random_events_never_panic() {
        let mut instruments: Vec<Instrument> = vec![Instrument::builtin_sine(SR)];
        instruments.extend(crate::synth::PRESETS.iter().cloned().map(Instrument::synth));
        if let Some(home) = std::env::var_os("HOME") {
            for f in [
                "Sounds/SFZ/SplendidGrandPiano/Splendid Grand Piano.sfz",
                "Sounds/SFZ/SalamanderGrandPiano/Salamander Grand Piano V3.sfz",
            ] {
                let path = std::path::Path::new(&home).join(f);
                if path.exists() {
                    instruments.push(crate::instrument::load_sfz(&path, &Default::default()).unwrap());
                }
            }
        }
        let mut seed = 12345u32;
        let mut rnd = |n: u32| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed % n
        };
        for inst in instruments {
            let mut rig = Rig::new(inst);
            for _ in 0..3000 {
                let event = match rnd(10) {
                    0..=3 => on(rnd(128) as u8, 1 + rnd(127) as u8),
                    4..=6 => off(rnd(128) as u8),
                    7 => cc(64, if rnd(2) == 0 { 0 } else { 127 }),
                    8 => cc(rnd(128) as u8, rnd(128) as u8),
                    _ => MidiEvent::PitchBend { ch: 0, value: rnd(16384) as i16 - 8192 },
                };
                rig.send(event);
                if rnd(50) == 0 {
                    let id = ParamId::ALL[rnd(ParamId::ALL.len() as u32) as usize];
                    rig.set(&[(id, rnd(200) as i32 - 100)]);
                }
                if rnd(4) == 0 {
                    rig.engine.render(&mut rig.buf);
                    assert!(rig.buf.iter().all(|f| f[0].is_finite() && f[0].abs() <= 1.0));
                }
            }
            rig.send(cc(ALL_NOTES_OFF_CC, 0));
        }
    }

    /// High-frequency share of the left channel: RMS of the sample-to-sample difference over RMS.
    fn brightness(buf: &[[f32; 2]]) -> f32 {
        let d: f32 = buf.windows(2).map(|w| (w[1][0] - w[0][0]).powi(2)).sum::<f32>();
        let x: f32 = buf.iter().map(|f| f[0] * f[0]).sum::<f32>();
        (d / x.max(1e-12)).sqrt()
    }

    #[test]
    fn filter_envelope_opens_then_closes() {
        let mut rig = Rig::new(synth("Saw"));
        // Low pass ~600 Hz, envelope +4 octaves, decay 0.4 s (default).
        rig.set(&[(ParamId::LowPass, -84), (ParamId::FilterEnv, 32), (ParamId::Attack, -100)]);
        rig.run(0.1); // let the master cutoff glide settle
        rig.send(on(45, 127)); // 110 Hz
        rig.engine.render(&mut rig.buf);
        let early = brightness(&rig.buf);
        rig.run(2.0);
        rig.engine.render(&mut rig.buf);
        let late = brightness(&rig.buf);
        assert!(early > late * 2.0, "bright attack ({early}) closing to the base cutoff ({late})");
    }

    #[test]
    fn glide_slides_legato_notes_but_not_chords() {
        let mut rig = Rig::new(synth("Sine"));
        rig.set(&[(ParamId::Glide, 50)]); // 0.5 s
        rig.send(on(57, 100));
        rig.run(0.1);
        rig.send(on(69, 100)); // legato: 57 still held
        let v = *rig.engine.voices.iter().filter(|v| v.active).max_by_key(|v| v.started).unwrap();
        assert!((v.glide_ratio - 0.5).abs() < 1e-3, "starts an octave below: {}", v.glide_ratio);
        rig.run(0.6);
        let v = *rig.engine.voices.iter().filter(|v| v.active).max_by_key(|v| v.started).unwrap();
        assert!((v.glide_ratio - 1.0).abs() < 0.01, "arrived: {}", v.glide_ratio);

        // A chord struck at once: no glide between its notes.
        let mut rig = Rig::new(synth("Sine"));
        rig.set(&[(ParamId::Glide, 50)]);
        rig.send(on(60, 100));
        rig.send(on(64, 100));
        rig.send(on(67, 100));
        assert!(rig.engine.voices.iter().filter(|v| v.active).all(|v| v.glide_ratio == 1.0));
    }

    #[test]
    fn lfo_rate_sets_vibrato_speed() {
        let mut rig = Rig::new(synth("Sine"));
        rig.set(&[(ParamId::LfoRate, 100)]); // 20 Hz
        let before = rig.engine.lfo_phase;
        rig.run(0.5); // 94 blocks of 256 = 0.5013 s → 10.03 cycles
        let cycles = (rig.engine.lfo_phase - before).rem_euclid(1.0);
        assert!((cycles - 0.0267).abs() < 0.01, "fractional cycles {cycles}");
    }

    #[test]
    fn master_filters_cut() {
        let mut rig = Rig::new(Instrument::builtin_sine(SR));
        rig.send(on(105, 127)); // ~3.5 kHz
        let open = rig.rms(0.05);
        rig.send(on(105, 127));
        rig.set(&[(ParamId::LowPass, -96)]); // ~500 Hz
        rig.run(0.2);
        let lp = rig.rms(0.05);
        assert!(lp < open * 0.1, "low pass: {lp} vs {open}");

        let mut rig = Rig::new(Instrument::builtin_sine(SR));
        rig.send(on(33, 127)); // 55 Hz
        let open = rig.rms(0.05);
        rig.send(on(33, 127));
        rig.set(&[(ParamId::HighPass, 80)]); // ~1 kHz
        rig.run(0.2);
        let hp = rig.rms(0.05);
        assert!(hp < open * 0.1, "high pass: {hp} vs {open}");
    }

    #[test]
    fn limiter_is_transparent_below_knee_and_bounded() {
        for x in [0.0, 0.1, -0.5, 0.79] {
            assert_eq!(soft_clip(x), x);
        }
        let mut last = soft_clip(CLIP_KNEE);
        for i in 1..=400 {
            let y = soft_clip(CLIP_KNEE + i as f32 * 0.02);
            assert!(y >= last && y <= 1.0, "monotonic and bounded at step {i}: {y}");
            last = y;
        }
        assert_eq!(soft_clip(-10.0), -1.0);
    }

    #[test]
    fn filter_attenuates_above_cutoff() {
        let mut lp = Biquad::new(FilterKind::Lpf2p, 1000.0, 0.0, SR as f32);
        let mut peak = 0.0f32;
        for i in 0..4800 {
            let x = (std::f32::consts::TAU * 8000.0 * i as f32 / SR as f32).sin();
            let y = lp.process(0, x);
            if i > 2400 {
                peak = peak.max(y.abs());
            }
        }
        assert!(peak < 0.03, "8 kHz through a 1 kHz 12 dB/oct low-pass: {peak}");
    }
}
