//! Real-time sampler engine, run inside the audio callback.
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

const MAX_VOICES: usize = 128;
const BEND_RANGE_CENTS: f32 = 200.0;
const MASTER_GAIN: f32 = 1.0;
/// -80 dB: below this a voice is considered finished.
const SILENCE: f32 = 1e-4;
/// ln(1000): exponential segments fall by 60 dB over their nominal time.
const LN_1000: f32 = 6.907_755;
const ALL_SOUND_OFF_CC: u8 = 120;
const ALL_NOTES_OFF_CC: u8 = 123;

pub enum EngineCmd {
    /// `at` is when the event reached us, for latency measurement.
    Midi { event: MidiEvent, at: Instant },
    SetInstrument(Arc<Instrument>),
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
        Self { b0, b1, b2, a1, a2, z: [[0.0; 2]; 2] }
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
        env: Env {
            stage: Stage::Done,
            level: 0.0,
            delay_left: 0,
            hold_left: 0,
            attack_step: 0.0,
            decay_coef: 0.0,
            sustain: 0.0,
            release_coef: 0.0,
            release_after_attack: false,
        },
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
    sw_last: Option<u8>,
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

    fn note_on(&mut self, note: u8, vel: u8) {
        if self.is_keyswitch(note) {
            self.sw_last = Some(note);
            return;
        }
        let n = note as usize;
        self.key_vel[n] = vel;
        self.key_time[n] = self.clock;
        self.key_held[n] = true;
        self.release_pending[n] = false;

        let inst = self.inst.clone();
        let rand = self.random();
        for &ri in &inst.by_key[n] {
            let r = &inst.regions[ri as usize];
            if r.trigger == Trigger::Attack && self.region_matches(r, vel, rand) {
                self.start_voice(&inst, ri, note, vel, 1.0);
            }
        }
    }

    fn note_off(&mut self, note: u8) {
        if self.is_keyswitch(note) {
            return;
        }
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
            if inst.regions[v.region as usize].one_shot {
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
        let c = cc as usize;
        let old = self.ccs[c];
        let new = value as f32 / 127.0;
        self.ccs[c] = new;
        let inst = self.inst.clone();

        if cc == ALL_NOTES_OFF_CC || cc == ALL_SOUND_OFF_CC {
            for i in 0..self.voices.len() {
                if self.voices[i].active {
                    self.voices[i].key_down = false;
                    self.release_voice(&inst, i);
                }
            }
            return;
        }

        if cc as u16 == inst.sustain_cc && old >= 0.5 && new < 0.5 {
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
            if v.active && !v.offed && v.env.stage == Stage::Release && inst.regions[v.region as usize].ampeg.dynamic {
                self.release_voice(&inst, i);
            }
        }
    }

    fn ctx<'a>(&'a self, inst: &'a Instrument, note: u8, vel: u8) -> Ctx<'a> {
        Ctx { ccs: &self.ccs, curves: &inst.curves, note, vel }
    }

    fn release_voice(&mut self, inst: &Instrument, i: usize) {
        let v = self.voices[i];
        let r = &inst.regions[v.region as usize];
        let seconds = r.ampeg.release.eval(&self.ctx(inst, v.note, v.vel)).max(0.0005);
        self.voices[i].env.release(seconds, self.sr);
    }

    /// Fades a voice out because something else took its place.
    fn off_voice(&mut self, inst: &Instrument, i: usize) {
        let r = &inst.regions[self.voices[i].region as usize];
        match r.off_time {
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
                if v.active && !v.offed && inst.regions[v.region as usize].off_by == Some(r.group) {
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
            filter,
            key_down: r.trigger == Trigger::Attack,
            sustained: false,
            offed: false,
            started: self.voice_counter,
        };
        let slot = self.voices.iter().position(|v| !v.active).unwrap_or_else(|| {
            // Steal the quietest voice.
            (0..self.voices.len())
                .min_by(|&a, &b| {
                    let level = |v: &Voice| v.env.level * (v.gain[0] + v.gain[1]);
                    level(&self.voices[a]).total_cmp(&level(&self.voices[b]))
                })
                .unwrap_or(0)
        });
        self.voices[slot] = voice;
    }

    /// Fills `out` with stereo frames.
    pub fn render(&mut self, out: &mut [[f32; 2]]) {
        out.fill([0.0; 2]);
        let inst = &*self.inst;
        let bend = self.bend as f64;
        let mut active = 0;
        for v in self.voices.iter_mut().filter(|v| v.active) {
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
                frame[0] += l * v.gain[0] * e;
                frame[1] += r * v.gain[1] * e;
                v.pos += step;
            }
            if v.active {
                active += 1;
            }
        }

        for frame in out.iter_mut() {
            for s in frame.iter_mut() {
                *s = soft_clip(*s * MASTER_GAIN);
            }
            let level = frame[0].abs().max(frame[1].abs());
            self.peak = if level > self.peak { level } else { self.peak * self.peak_fall };
        }
        self.clock += out.len() as u64;
        self.stats.active_voices.store(active, Relaxed);
        self.stats.peak.store(self.peak.to_bits(), Relaxed);
    }
}

/// Smooth limiter: rational tanh approximation, nearly linear below ~0.3, saturates at ±1.
fn soft_clip(x: f32) -> f32 {
    let x = x.clamp(-3.0, 3.0);
    x * (27.0 + x * x) / (27.0 + 9.0 * x * x)
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
