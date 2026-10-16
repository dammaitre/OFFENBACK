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
}

impl ParamId {
    /// Matrix order, 5 per row: level & pitch, synth envelope & LFO, filters.
    pub const ALL: [ParamId; 13] = [
        ParamId::Volume,
        ParamId::Transpose,
        ParamId::Detune,
        ParamId::Velocity,
        ParamId::Reverb,
        ParamId::Attack,
        ParamId::Decay,
        ParamId::Sustain,
        ParamId::Release,
        ParamId::Lfo,
        ParamId::LowPass,
        ParamId::Resonance,
        ParamId::HighPass,
    ];

    /// Only meaningful for synths: a recorded piano has its own envelope.
    pub fn synth_only(self) -> bool {
        matches!(self, ParamId::Attack | ParamId::Decay | ParamId::Sustain | ParamId::Release | ParamId::Lfo)
    }

    pub fn spec(self) -> &'static Spec {
        &SPECS[self as usize]
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

const SPECS: [Spec; 13] = [
    Spec {
        name: "Volume",
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
        min: 0.0,
        max: 100.0,
        default: 0.0,
        scale: Scale::Square,
        steps: 100,
        format: |v| format!("±{v:.0} ct"),
        default_label: Some("off"),
    },
];

/// Vibrato rate of the LFO cell.
pub const LFO_RATE_HZ: f32 = 5.5;

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

    /// Moves `id` by `steps` notches (negative = down). The grid is anchored on the default,
    /// so stepping away and back always lands exactly on it, even when it's between grid
    /// points of the raw range.
    pub fn nudge(&mut self, id: ParamId, steps: i32) {
        let spec = id.spec();
        let (n_def, per) = (spec.to_norm(spec.default), spec.steps as f32);
        let k = ((spec.to_norm(self.get(id)) - n_def) * per).round() as i32 + steps;
        let n = (n_def + k as f32 / per).clamp(0.0, 1.0);
        self.values[id as usize] = if k == 0 { spec.default } else { spec.from_norm(n) };
    }

    pub fn reset(&mut self, id: ParamId) {
        self.values[id as usize] = id.spec().default;
    }

    pub fn is_default(&self, id: ParamId) -> bool {
        let spec = id.spec();
        (spec.to_norm(self.get(id)) - spec.to_norm(spec.default)).abs() < 0.5 / spec.steps as f32
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
        assert!(ParamId::Sustain.synth_only() && !ParamId::Resonance.synth_only());
    }
}
