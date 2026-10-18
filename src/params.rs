//! User-facing sound parameters (the effects matrix): ranges, steps, display.
//!
//! Values are stored in real units. Editing happens in a normalized 0..1 space with a fixed
//! number of steps, so one scroll notch feels similar on every parameter.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamId {
    Volume,
    Attack,
    Decay,
    Sustain,
    Release,
    Reverb,
    Transpose,
    Detune,
    LowPass,
    HighPass,
    Velocity,
    Resonance,
    Lfo,
    LfoRate,
    FilterEnv,
    Glide,
}

impl ParamId {
    /// Matrix order, 6 per row: level & pitch, synth envelope & LFO, filters.
    pub const ALL: [ParamId; 16] = [
        ParamId::Volume,
        ParamId::Transpose,
        ParamId::Detune,
        ParamId::Velocity,
        ParamId::Reverb,
        ParamId::Glide,
        ParamId::Attack,
        ParamId::Decay,
        ParamId::Sustain,
        ParamId::Release,
        ParamId::Lfo,
        ParamId::LfoRate,
        ParamId::LowPass,
        ParamId::Resonance,
        ParamId::FilterEnv,
        ParamId::HighPass,
    ];

    /// Only meaningful for synths: a recorded piano has its own envelope.
    pub fn synth_only(self) -> bool {
        matches!(
            self,
            ParamId::Attack
                | ParamId::Decay
                | ParamId::Sustain
                | ParamId::Release
                | ParamId::Lfo
                | ParamId::LfoRate
                | ParamId::FilterEnv
                | ParamId::Glide
        )
    }

    pub fn spec(self) -> &'static Spec {
        &SPECS[self as usize]
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|id| id.spec().key == key)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Scale {
    Linear,
    /// Equal ratios per step (frequencies, time multipliers).
    Log,
    /// Fine control near the minimum (short times).
    Square,
}

pub struct Spec {
    pub name: &'static str,
    /// Stable identifier in saved profiles; never change it once released.
    pub key: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub scale: Scale,
    /// Steps from min to max.
    pub steps: u32,
    format: fn(f32) -> String,
    /// Shown instead of the value when at default (e.g. "off", "inst").
    default_label: Option<&'static str>,
}

const SPECS: [Spec; 16] = [
    Spec {
        name: "Volume",
        key: "volume",
        min: -48.0,
        max: 12.0,
        default: 0.0,
        scale: Scale::Linear,
        steps: 120,
        format: |v| format!("{v:+.1} dB"),
        default_label: None,
    },
    Spec {
        name: "Attack",
        key: "attack",
        min: 0.0,
        max: 5.0,
        default: 0.005,
        scale: Scale::Square,
        steps: 100,
        format: format_seconds,
        default_label: None,
    },
    Spec {
        name: "Decay",
        key: "decay",
        min: 0.01,
        max: 10.0,
        default: 0.4,
        scale: Scale::Log,
        steps: 100,
        format: format_seconds,
        default_label: None,
    },
    Spec {
        name: "Sustain",
        key: "sustain",
        min: 0.0,
        max: 100.0,
        default: 70.0,
        scale: Scale::Linear,
        steps: 100,
        format: |v| format!("{v:.0} %"),
        default_label: None,
    },
    Spec {
        name: "Release",
        key: "release",
        min: 0.01,
        max: 10.0,
        default: 0.3,
        scale: Scale::Log,
        steps: 100,
        format: format_seconds,
        default_label: None,
    },
    Spec {
        name: "Reverb",
        key: "reverb",
        min: 0.0,
        max: 100.0,
        default: 0.0,
        scale: Scale::Linear,
        steps: 100,
        format: |v| format!("{v:.0} %"),
        default_label: Some("off"),
    },
    Spec {
        name: "Transpose",
        key: "transpose",
        min: -24.0,
        max: 24.0,
        default: 0.0,
        scale: Scale::Linear,
        steps: 48,
        format: |v| format!("{v:+.0} st"),
        default_label: None,
    },
    Spec {
        name: "Detune",
        key: "detune",
        min: -100.0,
        max: 100.0,
        default: 0.0,
        scale: Scale::Linear,
        steps: 200,
        format: |v| format!("{v:+.0} ct"),
        default_label: None,
    },
    Spec {
        name: "Low pass",
        key: "low_pass",
        min: 100.0,
        max: 20_000.0,
        default: 20_000.0,
        scale: Scale::Log,
        steps: 120,
        format: format_hz,
        default_label: Some("off"),
    },
    Spec {
        name: "High pass",
        key: "high_pass",
        min: 20.0,
        max: 5_000.0,
        default: 20.0,
        scale: Scale::Log,
        steps: 120,
        format: format_hz,
        default_label: Some("off"),
    },
    Spec {
        name: "Velocity",
        key: "velocity",
        min: -100.0,
        max: 100.0,
        default: 0.0,
        scale: Scale::Linear,
        steps: 100,
        format: |v| if v > 0.0 { format!("light {v:.0}") } else { format!("heavy {:.0}", -v) },
        default_label: Some("linear"),
    },
    Spec {
        name: "Resonance",
        key: "resonance",
        min: 0.0,
        max: 18.0,
        default: 0.0,
        scale: Scale::Linear,
        steps: 72,
        format: |v| format!("{v:.1} dB"),
        default_label: Some("off"),
    },
    Spec {
        name: "LFO",
        key: "lfo",
        min: 0.0,
        max: 100.0,
        default: 0.0,
        scale: Scale::Square,
        steps: 100,
        format: |v| format!("±{v:.0} ct"),
        default_label: Some("off"),
    },
    Spec {
        name: "LFO rate",
        key: "lfo_rate",
        min: 0.1,
        max: 20.0,
        default: 5.5,
        scale: Scale::Log,
        steps: 100,
        format: |v| if v < 1.0 { format!("{v:.2} Hz") } else { format!("{v:.1} Hz") },
        default_label: None,
    },
    Spec {
        name: "Filter env",
        key: "filter_env",
        min: 0.0,
        max: 8.0,
        default: 0.0,
        scale: Scale::Linear,
        steps: 64,
        format: |v| format!("+{v:.2} oct"),
        default_label: Some("off"),
    },
    Spec {
        name: "Glide",
        key: "glide",
        min: 0.0,
        max: 2.0,
        default: 0.0,
        scale: Scale::Square,
        steps: 100,
        format: format_seconds,
        default_label: Some("off"),
    },
];

fn format_seconds(v: f32) -> String {
    if v < 1.0 { format!("{:.0} ms", v * 1000.0) } else { format!("{v:.2} s") }
}

/// Touch curve exponent for the Velocity parameter: light (+100) → 0.25, heavy (-100) → 4.
pub fn velocity_gamma(amount: f32) -> f32 {
    4f32.powf(-amount / 100.0)
}

fn format_hz(v: f32) -> String {
    if v >= 1000.0 { format!("{:.1} kHz", v / 1000.0) } else { format!("{v:.0} Hz") }
}

impl Spec {
    /// Position of `v` in 0..1.
    pub fn to_norm(&self, v: f32) -> f32 {
        let v = v.clamp(self.min, self.max);
        match self.scale {
            Scale::Linear => (v - self.min) / (self.max - self.min),
            Scale::Log => (v / self.min).ln() / (self.max / self.min).ln(),
            Scale::Square => ((v - self.min) / (self.max - self.min)).sqrt(),
        }
    }

    pub fn from_norm(&self, n: f32) -> f32 {
        let n = n.clamp(0.0, 1.0);
        match self.scale {
            Scale::Linear => self.min + n * (self.max - self.min),
            Scale::Log => self.min * (self.max / self.min).powf(n),
            Scale::Square => self.min + n * n * (self.max - self.min),
        }
    }

    /// `v` moved by `steps` notches (negative = down). The grid is anchored on the default,
    /// so stepping away and back always lands exactly on it, even when it's between grid
    /// points of the raw range.
    pub fn nudged(&self, v: f32, steps: i32) -> f32 {
        let (n_def, per) = (self.to_norm(self.default), self.steps as f32);
        let k = ((self.to_norm(v) - n_def) * per).round() as i32 + steps;
        let n = (n_def + k as f32 / per).clamp(0.0, 1.0);
        if k == 0 { self.default } else { self.from_norm(n) }
    }

    pub fn is_default(&self, v: f32) -> bool {
        (self.to_norm(v) - self.to_norm(self.default)).abs() < 0.5 / self.steps as f32
    }

    pub fn format(&self, v: f32) -> String {
        match self.default_label {
            Some(label) if (v - self.default).abs() <= f32::EPSILON * self.max.abs().max(1.0) => label.into(),
            _ => (self.format)(v),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    values: [f32; SPECS.len()],
}

impl Default for Params {
    fn default() -> Self {
        // Indexed by `ParamId as usize` (= SPECS order), not by matrix order.
        Self { values: std::array::from_fn(|i| SPECS[i].default) }
    }
}

impl Params {
    pub fn get(&self, id: ParamId) -> f32 {
        self.values[id as usize]
    }

    /// Moves `id` by `steps` notches (negative = down), see `Spec::nudged`.
    pub fn nudge(&mut self, id: ParamId, steps: i32) {
        self.values[id as usize] = id.spec().nudged(self.get(id), steps);
    }

    /// Sets `id` to `value`, clamped to its range (presets).
    pub fn set(&mut self, id: ParamId, value: f32) {
        let spec = id.spec();
        self.values[id as usize] = value.clamp(spec.min, spec.max);
    }

    pub fn reset(&mut self, id: ParamId) {
        self.values[id as usize] = id.spec().default;
    }

    pub fn is_default(&self, id: ParamId) -> bool {
        id.spec().is_default(self.get(id))
    }
}

/// Centre frequencies of the master graphic EQ, one band per octave.
pub const EQ_BANDS: [f32; 10] = [31.0, 63.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16_000.0];
const EQ_NAMES: [&str; 10] = ["31", "63", "125", "250", "500", "1k", "2k", "4k", "8k", "16k"];
/// Master strip controls: level, then one per EQ band.
pub const MASTER_COUNT: usize = 1 + EQ_BANDS.len();

const MASTER_LEVEL: Spec = Spec {
    name: "Master",
    key: "level",
    min: -48.0,
    max: 12.0,
    default: 0.0,
    scale: Scale::Linear,
    steps: 120,
    format: |v| format!("{v:+.1} dB"),
    default_label: None,
};

const EQ_GAIN: Spec = Spec {
    name: "EQ",
    key: "eq",
    min: -12.0,
    max: 12.0,
    default: 0.0,
    scale: Scale::Linear,
    steps: 48,
    format: |v| format!("{v:+.1}"),
    default_label: Some("0"),
};

/// The master strip, applied to the mix of every layer: level and graphic EQ.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Master {
    /// `MASTER_COUNT` values: level in dB, then each band's gain in dB.
    values: [f32; MASTER_COUNT],
}

impl Default for Master {
    fn default() -> Self {
        Self { values: std::array::from_fn(|i| Self::spec(i).default) }
    }
}

impl Master {
    pub fn spec(i: usize) -> &'static Spec {
        if i == 0 { &MASTER_LEVEL } else { &EQ_GAIN }
    }

    /// Short label shown in the strip.
    pub fn label(i: usize) -> &'static str {
        if i == 0 { MASTER_LEVEL.name } else { EQ_NAMES[i - 1] }
    }

    pub fn get(&self, i: usize) -> f32 {
        self.values[i]
    }

    pub fn level_db(&self) -> f32 {
        self.values[0]
    }

    pub fn band_db(&self, band: usize) -> f32 {
        self.values[1 + band]
    }

    pub fn nudge(&mut self, i: usize, steps: i32) {
        self.values[i] = Self::spec(i).nudged(self.values[i], steps);
    }

    /// Sets control `i` to `value`, clamped to its range (profiles).
    pub fn set(&mut self, i: usize, value: f32) {
        let spec = Self::spec(i);
        self.values[i] = value.clamp(spec.min, spec.max);
    }

    pub fn reset(&mut self, i: usize) {
        self.values[i] = Self::spec(i).default;
    }

    pub fn is_default(&self, i: usize) -> bool {
        Self::spec(i).is_default(self.values[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nudge_round_trips_and_clamps() {
        let mut p = Params::default();
        for id in ParamId::ALL {
            // Move away from the nearest end first, so the round trip never clamps.
            let dir = if id.spec().to_norm(id.spec().default) < 0.5 { 3 } else { -3 };
            p.nudge(id, dir);
            p.nudge(id, -dir);
            let (spec, v) = (id.spec(), p.get(id));
            assert!((v - spec.default).abs() < 1e-3 * spec.max.abs().max(1.0), "{} {v}", spec.name);
            p.nudge(id, 10_000);
            assert!((p.get(id) - spec.max).abs() < 1e-3 * spec.max.abs().max(1.0));
            p.nudge(id, -10_000);
            assert!((p.get(id) - spec.min).abs() < 1e-3 * spec.max.abs().max(1.0));
            p.reset(id);
            assert!(p.is_default(id));
        }
    }

    #[test]
    fn steps_and_labels() {
        let mut p = Params::default();
        p.nudge(ParamId::Transpose, 1);
        assert_eq!(p.get(ParamId::Transpose), 1.0);
        p.nudge(ParamId::Volume, -1);
        assert_eq!(ParamId::Volume.spec().format(p.get(ParamId::Volume)), "-0.5 dB");
        assert_eq!(ParamId::LowPass.spec().format(20_000.0), "off");
        assert_eq!(ParamId::LowPass.spec().format(1200.0), "1.2 kHz");
        assert_eq!(ParamId::Release.spec().format(0.3), "300 ms");
        assert_eq!(ParamId::Attack.spec().format(2.5), "2.50 s");
        assert_eq!(ParamId::Lfo.spec().format(0.0), "off");
        assert_eq!(ParamId::LfoRate.spec().format(5.5), "5.5 Hz");
        assert_eq!(ParamId::FilterEnv.spec().format(3.0), "+3.00 oct");
        assert_eq!(ParamId::Glide.spec().format(0.0), "off");
        assert!(ParamId::Glide.synth_only() && ParamId::FilterEnv.synth_only());
        assert!(ParamId::Sustain.synth_only() && !ParamId::Resonance.synth_only());
    }

    #[test]
    fn master_strip_steps() {
        let mut m = Master::default();
        m.nudge(0, -1);
        assert_eq!(m.level_db(), -0.5);
        m.nudge(3, 1);
        assert_eq!(m.band_db(2), 0.5);
        assert_eq!(Master::spec(3).format(m.get(3)), "+0.5");
        m.nudge(3, -1);
        assert!(m.is_default(3));
        assert_eq!(Master::spec(3).format(m.get(3)), "0");
        m.nudge(10, 1000);
        assert_eq!(m.band_db(9), 12.0);
        assert_eq!((Master::label(0), Master::label(6), Master::label(10)), ("Master", "1k", "16k"));
    }
}
