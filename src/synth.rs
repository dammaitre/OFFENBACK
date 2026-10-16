//! Synth patches and band-limited oscillators.
//!
//! Saw, square and pulse use PolyBLEP: the waveform's jumps are smoothed by a 2-sample
//! polynomial residual, which removes most aliasing at a tiny cost.

use crate::params::ParamId;

/// Most oscillators one synth voice runs (super saw).
pub const MAX_UNISON: usize = 7;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Waveform {
    Sine,
    Triangle,
    Saw,
    Square,
    /// Pulse with the given duty cycle (0..1).
    Pulse(f32),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SynthPatch {
    pub name: &'static str,
    pub waveform: Waveform,
    /// Oscillators per voice, spread in pitch and stereo.
    pub unison: usize,
    /// Pitch offset of the outermost unison oscillators.
    pub spread_cents: f32,
    /// Output level, balancing the waveforms' different loudness.
    pub gain: f32,
    /// Level of a sine one octave below, relative to the main oscillators.
    pub sub: f32,
    /// Saturation before the filter (0 = clean). Adds harmonics a small speaker can play.
    pub drive: f32,
    /// One note at a time: a new note fades the previous one out.
    pub mono: bool,
    /// Matrix values the preset suggests, applied when it's chosen.
    pub params: &'static [(ParamId, f32)],
}

/// A plain polyphonic patch: one waveform, no extras.
const fn basic(name: &'static str, waveform: Waveform, gain: f32) -> SynthPatch {
    SynthPatch { name, waveform, unison: 1, spread_cents: 0.0, gain, sub: 0.0, drive: 0.0, mono: false, params: &[] }
}

pub const PRESETS: [SynthPatch; 8] = [
    basic("Sine", Waveform::Sine, 0.35),
    basic("Triangle", Waveform::Triangle, 0.35),
    basic("Saw", Waveform::Saw, 0.3),
    basic("Square", Waveform::Square, 0.2),
    basic("Pulse 25%", Waveform::Pulse(0.25), 0.24),
    SynthPatch { name: "Super saw", unison: 7, spread_cents: 25.0, ..basic("", Waveform::Saw, 0.3) },
    SynthPatch {
        name: "Bass",
        sub: 0.7,
        drive: 1.5,
        mono: true,
        params: &[
            (ParamId::LowPass, 700.0),
            (ParamId::Resonance, 4.0),
            (ParamId::FilterEnv, 2.5),
            (ParamId::Attack, 0.002),
            (ParamId::Decay, 0.25),
            (ParamId::Sustain, 55.0),
            (ParamId::Release, 0.08),
            (ParamId::Glide, 0.06),
        ],
        ..basic("", Waveform::Saw, 0.3)
    },
    SynthPatch {
        name: "Sub bass",
        drive: 2.5,
        mono: true,
        params: &[
            (ParamId::LowPass, 20_000.0),
            (ParamId::FilterEnv, 0.0),
            (ParamId::Attack, 0.003),
            (ParamId::Decay, 0.5),
            (ParamId::Sustain, 80.0),
            (ParamId::Release, 0.1),
            (ParamId::Glide, 0.04),
        ],
        ..basic("", Waveform::Sine, 0.4)
    },
];

impl SynthPatch {
    /// Pitch ratio and stereo position (-1..1) of unison oscillator `k`.
    pub fn unison_voice(&self, k: usize) -> (f32, f32) {
        if self.unison <= 1 {
            return (1.0, 0.0);
        }
        let x = k as f32 / (self.unison - 1) as f32 * 2.0 - 1.0; // -1..1
        (2f32.powf(x * self.spread_cents / 1200.0), x)
    }

    /// Per-oscillator gain so stacking oscillators doesn't change loudness much.
    pub fn unison_gain(&self) -> f32 {
        self.gain / (self.unison as f32).sqrt()
    }
}

/// PolyBLEP residual for a discontinuity at phase 0. `t` is the phase (0..1), `dt` the
/// phase increment per sample.
#[inline]
fn poly_blep(t: f32, dt: f32) -> f32 {
    if t < dt {
        let x = t / dt;
        x + x - x * x - 1.0
    } else if t > 1.0 - dt {
        let x = (t - 1.0) / dt;
        x * x + x + x + 1.0
    } else {
        0.0
    }
}

/// One sample of `waveform` at `phase` (0..1), roughly -1..1.
#[inline]
pub fn oscillator(waveform: Waveform, phase: f32, dt: f32) -> f32 {
    match waveform {
        Waveform::Sine => (phase * std::f32::consts::TAU).sin(),
        // Harmonics fall at 12 dB/octave, so the naive shape aliases very little.
        Waveform::Triangle => 4.0 * (phase - 0.5).abs() - 1.0,
        Waveform::Saw => 2.0 * phase - 1.0 - poly_blep(phase, dt),
        Waveform::Square => pulse(phase, dt, 0.5),
        Waveform::Pulse(width) => pulse(phase, dt, width),
    }
}

/// Saturation normalised so a full-scale input stays full scale; `drive` 0 is clean.
#[inline]
pub fn saturate(x: f32, drive: f32) -> f32 {
    if drive <= 0.0 {
        return x;
    }
    let k = 1.0 + drive;
    fast_tanh(x * k) / fast_tanh(k)
}

#[inline]
fn fast_tanh(x: f32) -> f32 {
    let x = x.clamp(-3.0, 3.0);
    x * (27.0 + x * x) / (27.0 + 9.0 * x * x)
}

#[inline]
fn pulse(phase: f32, dt: f32, width: f32) -> f32 {
    let naive = if phase < width { 1.0 } else { -1.0 };
    let mut falling = phase - width;
    if falling < 0.0 {
        falling += 1.0;
    }
    naive + poly_blep(phase, dt) - poly_blep(falling, dt)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders `seconds` of `waveform` at `freq` and returns the samples.
    fn render(waveform: Waveform, freq: f32, sr: f32, n: usize) -> Vec<f32> {
        let dt = freq / sr;
        let mut phase = 0.0f32;
        (0..n)
            .map(|_| {
                let s = oscillator(waveform, phase, dt);
                phase += dt;
                if phase >= 1.0 {
                    phase -= 1.0;
                }
                s
            })
            .collect()
    }

    #[test]
    fn waveforms_are_bounded_and_centred() {
        for w in [Waveform::Sine, Waveform::Triangle, Waveform::Saw, Waveform::Square, Waveform::Pulse(0.25)] {
            let x = render(w, 441.0, 44_100.0, 44_100);
            let peak = x.iter().fold(0.0f32, |p, v| p.max(v.abs()));
            assert!(peak <= 1.3, "{w:?} peak {peak}");
            let mean = x.iter().sum::<f32>() / x.len() as f32;
            let expected = if let Waveform::Pulse(d) = w { 2.0 * d - 1.0 } else { 0.0 };
            assert!((mean - expected).abs() < 0.02, "{w:?} DC {mean}");
        }
    }

    /// Energy that lands between the harmonics of a high note: aliasing.
    fn inharmonic_energy(x: &[f32], freq: f32, sr: f32) -> f32 {
        // Goertzel at bins halfway between harmonics, below Nyquist.
        let mut total = 0.0;
        let mut f = freq * 1.5;
        while f < sr / 2.0 {
            let w = std::f32::consts::TAU * f / sr;
            let (mut s1, mut s2) = (0.0f32, 0.0f32);
            for &v in x {
                let s = v + 2.0 * w.cos() * s1 - s2;
                s2 = s1;
                s1 = s;
            }
            total += s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2;
            f += freq;
        }
        total
    }

    #[test]
    fn poly_blep_reduces_aliasing() {
        let (sr, freq, n) = (48_000.0, 3_137.0, 48_000);
        let blep = render(Waveform::Saw, freq, sr, n);
        let dt = freq / sr;
        let mut phase = 0.0f32;
        let naive: Vec<f32> = (0..n)
            .map(|_| {
                let s = 2.0 * phase - 1.0;
                phase = (phase + dt) % 1.0;
                s
            })
            .collect();
        let ratio = inharmonic_energy(&blep, freq, sr) / inharmonic_energy(&naive, freq, sr);
        assert!(ratio < 0.25, "PolyBLEP keeps {ratio} of the naive aliasing");
    }

    #[test]
    fn drive_adds_harmonics_but_keeps_scale() {
        assert_eq!(saturate(0.3, 0.0), 0.3);
        assert!((saturate(1.0, 2.5) - 1.0).abs() < 1e-6);
        assert!(saturate(0.3, 2.5) > 0.3, "quiet parts are lifted (compression)");
    }

    #[test]
    fn unison_spreads_symmetrically() {
        let p = &PRESETS[5];
        let (lo, pan_lo) = p.unison_voice(0);
        let (mid, pan_mid) = p.unison_voice(3);
        let (hi, pan_hi) = p.unison_voice(6);
        assert!((lo * hi - 1.0).abs() < 1e-5 && mid == 1.0);
        assert_eq!((pan_lo, pan_mid, pan_hi), (-1.0, 0.0, 1.0));
    }
}
