//! Instrument model shared by all sample sources (SFZ today, maybe Muse `.sts` later),
//! and the sample decoder.
//!
//! Every tweakable value is a [`Param`]: a base plus CC modulations through curves, so
//! things like "sustain pedal lengthens the release" need no special casing.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use anyhow::{Context, Result, anyhow, bail};

use crate::sfz::{self, Opcodes, parse_key};

/// MIDI CCs 0..127 plus ARIA extended CCs (131 = note-on velocity, 133 = note number).
pub const CC_COUNT: usize = 160;
pub const CC_VELOCITY: u16 = 131;
pub const CC_NOTE: u16 = 133;
const DEFAULT_SUSTAIN_CC: u16 = 64;
/// Silent frames before/after each sample so 4-point interpolation never reads out of bounds.
pub const PAD_START: usize = 1;
pub const PAD_END: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CcMod {
    pub cc: u16,
    pub depth: f32,
    pub curve: u16,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub base: f32,
    pub mods: Vec<CcMod>,
}

impl Param {
    pub fn constant(base: f32) -> Self {
        Self { base, mods: Vec::new() }
    }

    /// base + Σ depth · curve(cc)
    pub fn eval(&self, ctx: &Ctx) -> f32 {
        self.mods
            .iter()
            .fold(self.base, |acc, m| acc + m.depth * ctx.curve(m.curve, ctx.cc(m.cc)))
    }

    /// For `amplitude`: base% · Π (depth% · curve(cc)).
    pub fn eval_product(&self, ctx: &Ctx) -> f32 {
        self.mods.iter().fold(self.base / 100.0, |acc, m| {
            acc * m.depth / 100.0 * ctx.curve(m.curve, ctx.cc(m.cc))
        })
    }
}

/// What a parameter evaluation can see: controller values plus per-note extended CCs.
pub struct Ctx<'a> {
    pub ccs: &'a [f32; CC_COUNT],
    pub curves: &'a Curves,
    pub note: u8,
    pub vel: u8,
}

impl Ctx<'_> {
    pub fn cc(&self, cc: u16) -> f32 {
        match cc {
            CC_VELOCITY => self.vel as f32 / 127.0,
            CC_NOTE => self.note as f32 / 127.0,
            n => self.ccs.get(n as usize).copied().unwrap_or(0.0),
        }
    }

    fn curve(&self, index: u16, x: f32) -> f32 {
        self.curves.eval(index, x)
    }
}

/// Curve tables, 128 points each. 0..=6 are the SFZ built-ins; `<curve>` may add/replace any.
pub struct Curves {
    tables: Vec<Option<Box<[f32; 128]>>>,
}

impl Curves {
    fn new(custom: &[(u16, Vec<(u8, f32)>)]) -> Self {
        let builtin: [fn(f32) -> f32; 7] = [
            |x| x,
            |x| 2.0 * x - 1.0,
            |x| 1.0 - x,
            |x| 1.0 - 2.0 * x,
            |x| x * x,
            |x| x.sqrt(),
            |x| (1.0 - x).sqrt(),
        ];
        let size = custom.iter().map(|(i, _)| *i as usize + 1).max().unwrap_or(0).max(builtin.len());
        let mut tables: Vec<Option<Box<[f32; 128]>>> = vec![None; size];
        for (i, f) in builtin.iter().enumerate() {
            tables[i] = Some(Box::new(std::array::from_fn(|p| f(p as f32 / 127.0))));
        }
        for (index, points) in custom {
            let mut pts = points.clone();
            if pts.first().is_none_or(|&(p, _)| p != 0) {
                pts.insert(0, (0, 0.0));
            }
            if pts.last().is_none_or(|&(p, _)| p != 127) {
                pts.push((127, 1.0));
            }
            let table = std::array::from_fn(|p| {
                let p = p as u8;
                let hi = pts.iter().position(|&(q, _)| q >= p).unwrap_or(pts.len() - 1);
                let (p1, v1) = pts[hi];
                if hi == 0 || p1 == p {
                    return v1;
                }
                let (p0, v0) = pts[hi - 1];
                v0 + (v1 - v0) * (p - p0) as f32 / (p1 - p0) as f32
            });
            tables[*index as usize] = Some(Box::new(table));
        }
        Self { tables }
    }

    pub fn eval(&self, index: u16, x: f32) -> f32 {
        let Some(Some(t)) = self.tables.get(index as usize) else { return x };
        let pos = x.clamp(0.0, 1.0) * 127.0;
        let i = (pos as usize).min(126);
        let f = pos - i as f32;
        t[i] + (t[i + 1] - t[i]) * f
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Trigger {
    Attack,
    Release,
    /// Started by a controller crossing into `on_locc..on_hicc` (e.g. pedal noises).
    Cc,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FilterKind {
    Lpf1p,
    Lpf2p,
    Hpf1p,
    Hpf2p,
    Bpf2p,
}

#[derive(Clone, Debug)]
pub struct FilterSpec {
    pub kind: FilterKind,
    pub cutoff_hz: f32,
    /// Cents added to the cutoff, CC-modulated.
    pub cutoff_cents: Param,
    /// Cents added at full velocity.
    pub veltrack_cents: f32,
    pub resonance_db: f32,
}

#[derive(Clone, Debug)]
pub struct EnvSpec {
    pub delay: Param,
    pub attack: Param,
    pub hold: Param,
    pub decay: Param,
    /// Percent.
    pub sustain: Param,
    pub release: Param,
    /// Re-evaluate the release time when a controller moves (half-pedalling).
    pub dynamic: bool,
}

#[derive(Clone, Debug)]
pub struct Region {
    pub sample: usize,
    pub lokey: i16,
    pub hikey: i16,
    pub lovel: u8,
    pub hivel: u8,
    pub lorand: f32,
    pub hirand: f32,
    /// `loccN`/`hiccN`: controller ranges required at trigger time (normalized).
    pub cc_ranges: Vec<(u16, f32, f32)>,
    /// `on_loccN`/`on_hiccN` for CC-triggered regions.
    pub on_cc: Option<(u16, f32, f32)>,
    pub sw_last: Option<u8>,
    pub trigger: Trigger,
    pub pitch_keycenter: f32,
    /// Cents per key.
    pub pitch_keytrack: f32,
    /// Cents (`tune` + 100 · `transpose`).
    pub tune: Param,
    /// Frames.
    pub offset: Param,
    /// dB, includes group/master/global volume.
    pub volume: Param,
    /// Percent, modulations multiply.
    pub amplitude: Param,
    /// -100 (left) .. 100 (right).
    pub pan: Param,
    /// Percent.
    pub amp_veltrack: Param,
    pub ampeg: EnvSpec,
    pub filter: Option<FilterSpec>,
    pub group: i64,
    pub off_by: Option<i64>,
    /// Fade time when silenced by polyphony or `off_by`; `None` uses the region release.
    pub off_time: Option<f32>,
    pub note_polyphony: Option<u32>,
    pub polyphony: Option<u32>,
    /// dB lost per second the key was held (release samples).
    pub rt_decay: f32,
    pub one_shot: bool,
}

pub struct Sample {
    /// Interleaved, normalized to use the full i16 range, padded with silence.
    pub data: Vec<i16>,
    pub channels: usize,
    pub rate: u32,
    /// Frames of audio, excluding padding.
    pub frames: usize,
    /// Multiply decoded i16 values by this to get -1..1 floats.
    pub gain: f32,
}

pub struct Instrument {
    pub name: String,
    pub regions: Vec<Region>,
    pub samples: Vec<Sample>,
    pub curves: Curves,
    pub cc_defaults: [f32; CC_COUNT],
    pub sustain_cc: u16,
    /// Keyswitch range and starting switch.
    pub sw_range: Option<(u8, u8)>,
    pub sw_default: Option<u8>,
    /// Attack and release regions that can play on each key.
    pub by_key: Vec<Vec<u32>>,
    pub cc_triggered: Vec<u32>,
    pub unsupported: Vec<String>,
    pub warnings: Vec<String>,
}

impl Instrument {
    pub fn memory_bytes(&self) -> usize {
        self.samples.iter().map(|s| s.data.len() * 2).sum()
    }

    fn finish(mut self) -> Self {
        self.by_key = vec![Vec::new(); 128];
        for (i, r) in self.regions.iter().enumerate() {
            if r.trigger == Trigger::Cc {
                self.cc_triggered.push(i as u32);
                continue;
            }
            for key in r.lokey.max(0)..=r.hikey.min(127) {
                self.by_key[key as usize].push(i as u32);
            }
        }
        self
    }

    /// A decaying harmonic tone mapped across the keyboard, used when no instrument is loaded.
    pub fn builtin_sine(rate: u32) -> Self {
        let seconds = 3.0;
        let frames = (seconds * rate as f32) as usize;
        let mut data = vec![0i16; (frames + PAD_START + PAD_END) * 2];
        for i in 0..frames {
            let t = i as f32 / rate as f32;
            let p = std::f32::consts::TAU * 440.0 * t;
            let env = (-t / 0.8).exp() * (t / 0.002).min(1.0);
            let s = (p.sin() + 0.3 * (2.0 * p).sin() + 0.1 * (3.0 * p).sin()) / 1.4 * env;
            let v = (s * 32767.0) as i16;
            data[(i + PAD_START) * 2] = v;
            data[(i + PAD_START) * 2 + 1] = v;
        }
        let env = EnvSpec {
            delay: Param::constant(0.0),
            attack: Param::constant(0.0),
            hold: Param::constant(0.0),
            decay: Param::constant(0.0),
            sustain: Param::constant(100.0),
            release: Param::constant(0.08),
            dynamic: false,
        };
        let region = Region {
            sample: 0,
            lokey: 0,
            hikey: 127,
            lovel: 1,
            hivel: 127,
            lorand: 0.0,
            hirand: 1.0,
            cc_ranges: Vec::new(),
            on_cc: None,
            sw_last: None,
            trigger: Trigger::Attack,
            pitch_keycenter: 69.0,
            pitch_keytrack: 100.0,
            tune: Param::constant(0.0),
            offset: Param::constant(0.0),
            volume: Param::constant(-6.0),
            amplitude: Param::constant(100.0),
            pan: Param::constant(0.0),
            amp_veltrack: Param::constant(100.0),
            ampeg: env,
            filter: None,
            group: 0,
            off_by: None,
            off_time: None,
            note_polyphony: None,
            polyphony: None,
            rt_decay: 0.0,
            one_shot: false,
        };
        Self {
            name: "Built-in sine".into(),
            regions: vec![region],
            samples: vec![Sample { data, channels: 2, rate, frames, gain: 1.0 / 32767.0 }],
            curves: Curves::new(&[]),
            cc_defaults: default_ccs(),
            sustain_cc: DEFAULT_SUSTAIN_CC,
            sw_range: None,
            sw_default: None,
            by_key: Vec::new(),
            cc_triggered: Vec::new(),
            unsupported: Vec::new(),
            warnings: Vec::new(),
        }
        .finish()
    }
}

fn default_ccs() -> [f32; CC_COUNT] {
    let mut ccs = [0.0; CC_COUNT];
    ccs[7] = 100.0 / 127.0; // volume
    ccs[10] = 0.5; // pan
    ccs[11] = 1.0; // expression
    ccs
}

/// Loading progress, readable from the UI thread.
#[derive(Default)]
pub struct Progress {
    pub done: AtomicUsize,
    pub total: AtomicUsize,
}

/// Parses an SFZ file and decodes all its samples (in parallel).
pub fn load_sfz(path: &Path, progress: &Progress) -> Result<Instrument> {
    let raw = sfz::parse_file(path)?;
    let mut sample_index: HashMap<String, usize> = HashMap::new();
    let mut sample_paths: Vec<PathBuf> = Vec::new();
    let mut regions = Vec::new();
    let mut unsupported = BTreeSet::new();
    let mut warnings = Vec::new();

    let curves = Curves::new(&raw.curves);
    let mut cc_defaults = default_ccs();
    for &(cc, v) in &raw.cc_defaults {
        if let Some(slot) = cc_defaults.get_mut(cc as usize) {
            *slot = v;
        }
    }

    let first = raw.regions.first();
    let sustain_cc = first
        .and_then(|r| r.get("sustain_cc"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_SUSTAIN_CC);
    let key_opt = |name: &str| first.and_then(|r| r.get(name)).and_then(|v| parse_key(v));
    let sw_range = match (key_opt("sw_lokey"), key_opt("sw_hikey")) {
        (Some(lo), Some(hi)) => Some((lo.clamp(0, 127) as u8, hi.clamp(0, 127) as u8)),
        _ => None,
    };
    let sw_default = key_opt("sw_default").map(|k| k.clamp(0, 127) as u8);

    for ops in &raw.regions {
        for name in ops.keys() {
            if !is_supported(name) {
                unsupported.insert(name.clone());
            }
        }
        let Some(sample) = ops.get("sample") else { continue };
        if sample.contains('*') {
            // Generators like *sine, *noise.
            unsupported.insert(format!("sample={}", sample.rsplit('/').next().unwrap_or(sample)));
            continue;
        }
        let index = *sample_index.entry(sample.clone()).or_insert_with(|| {
            sample_paths.push(PathBuf::from(sample));
            sample_paths.len() - 1
        });
        regions.push(build_region(ops, index));
    }
    if regions.is_empty() {
        bail!("{} has no playable regions", path.display());
    }

    progress.total.store(sample_paths.len(), Relaxed);
    let decoded = decode_all(&sample_paths, progress);

    // Drop regions whose sample failed, remapping indices.
    let mut samples = Vec::new();
    let mut remap = vec![None; decoded.len()];
    for (i, result) in decoded.into_iter().enumerate() {
        match result {
            Ok(s) => {
                remap[i] = Some(samples.len());
                samples.push(s);
            }
            Err(e) => warnings.push(format!("{e:#}")),
        }
    }
    regions.retain_mut(|r| match remap[r.sample] {
        Some(i) => {
            r.sample = i;
            true
        }
        None => false,
    });
    if regions.is_empty() {
        bail!("no sample of {} could be loaded: {}", path.display(), warnings.join("; "));
    }

    let name = path.file_stem().map_or("instrument".into(), |s| s.to_string_lossy().into_owned());
    Ok(Instrument {
        name,
        regions,
        samples,
        curves,
        cc_defaults,
        sustain_cc,
        sw_range,
        sw_default,
        by_key: Vec::new(),
        cc_triggered: Vec::new(),
        unsupported: unsupported.into_iter().collect(),
        warnings,
    }
    .finish())
}

fn build_region(ops: &Opcodes, sample: usize) -> Region {
    let f = |name: &str, default: f32| ops.get(name).and_then(|v| v.parse::<f32>().ok()).unwrap_or(default);
    let key = |name: &str| ops.get(name).and_then(|v| parse_key(v));
    let int = |name: &str| ops.get(name).and_then(|v| v.parse::<i64>().ok());

    let (mut lokey, mut hikey, mut keycenter) = (key("lokey").unwrap_or(0), key("hikey").unwrap_or(127), None);
    if let Some(k) = key("key") {
        (lokey, hikey, keycenter) = (k, k, Some(k));
    }
    let keycenter = key("pitch_keycenter").or(keycenter).unwrap_or(60) as f32;

    let mut cc_ranges = Vec::new();
    let mut on_cc = None;
    for (name, value) in ops {
        let v: f32 = value.parse().unwrap_or(0.0);
        if let Some(cc) = name.strip_prefix("locc").and_then(|n| n.parse::<u16>().ok()) {
            let hi = f(&format!("hicc{cc}"), 127.0);
            cc_ranges.push((cc, v / 127.0, hi / 127.0));
        } else if let Some(cc) = name.strip_prefix("hicc").and_then(|n| n.parse::<u16>().ok()) {
            if !ops.contains_key(&format!("locc{cc}")) {
                cc_ranges.push((cc, 0.0, v / 127.0));
            }
        } else if let Some(cc) = name.strip_prefix("on_locc").and_then(|n| n.parse::<u16>().ok()) {
            let hi = f(&format!("on_hicc{cc}"), 127.0);
            on_cc = Some((cc, v / 127.0, hi / 127.0));
        }
    }

    let trigger = if on_cc.is_some() {
        Trigger::Cc
    } else {
        match ops.get("trigger").map(String::as_str) {
            Some("release" | "release_key") => Trigger::Release,
            _ => Trigger::Attack,
        }
    };

    let mut volume = param(ops, "volume", 0.0);
    volume.base += f("group_volume", 0.0) + f("master_volume", 0.0) + f("global_volume", 0.0);
    let mut tune = param(ops, "tune", 0.0);
    tune.base += 100.0 * f("transpose", 0.0);

    let filter = ops.get("cutoff").and_then(|c| c.parse::<f32>().ok()).map(|cutoff_hz| FilterSpec {
        kind: match ops.get("fil_type").map(String::as_str) {
            Some("lpf_1p") => FilterKind::Lpf1p,
            Some("hpf_1p") => FilterKind::Hpf1p,
            Some("hpf_2p") => FilterKind::Hpf2p,
            Some("bpf_2p") => FilterKind::Bpf2p,
            _ => FilterKind::Lpf2p,
        },
        cutoff_hz,
        cutoff_cents: param(ops, "cutoff", 0.0).mods_only(),
        veltrack_cents: f("fil_veltrack", 0.0),
        resonance_db: f("resonance", 0.0),
    });

    let off_time = match ops.get("off_mode").map(String::as_str) {
        Some("normal") => None,
        _ => Some(f("off_time", 0.006)),
    };

    Region {
        sample,
        lokey,
        hikey,
        lovel: f("lovel", 1.0).clamp(0.0, 127.0) as u8,
        hivel: f("hivel", 127.0).clamp(0.0, 127.0) as u8,
        lorand: f("lorand", 0.0),
        hirand: f("hirand", 1.0),
        cc_ranges,
        on_cc,
        sw_last: key("sw_last").map(|k| k.clamp(0, 127) as u8),
        trigger,
        pitch_keycenter: keycenter,
        pitch_keytrack: f("pitch_keytrack", 100.0),
        tune,
        offset: param(ops, "offset", 0.0),
        volume,
        amplitude: param(ops, "amplitude", 100.0),
        pan: param(ops, "pan", 0.0),
        amp_veltrack: param(ops, "amp_veltrack", 100.0),
        ampeg: EnvSpec {
            delay: param(ops, "ampeg_delay", 0.0),
            attack: param(ops, "ampeg_attack", 0.0),
            hold: param(ops, "ampeg_hold", 0.0),
            decay: param(ops, "ampeg_decay", 0.0),
            sustain: param(ops, "ampeg_sustain", 100.0),
            release: param(ops, "ampeg_release", 0.001),
            dynamic: f("ampeg_dynamic", 0.0) != 0.0,
        },
        filter,
        group: int("group").unwrap_or(0),
        off_by: int("off_by"),
        off_time,
        note_polyphony: int("note_polyphony").map(|n| n.max(1) as u32),
        polyphony: int("polyphony").map(|n| n.max(1) as u32),
        rt_decay: f("rt_decay", 0.0),
        one_shot: ops.get("loop_mode").is_some_and(|m| m == "one_shot"),
    }
}

impl Param {
    fn mods_only(mut self) -> Self {
        self.base = 0.0;
        self
    }
}

/// `name`, `name_onccN` (and the SFZ v1 spelling `nameccN`) with `name_curveccN`.
fn param(ops: &Opcodes, name: &str, default: f32) -> Param {
    let base = ops.get(name).and_then(|v| v.parse().ok()).unwrap_or(default);
    let mut mods: Vec<CcMod> = ops
        .iter()
        .filter_map(|(k, v)| {
            let rest = k.strip_prefix(name)?;
            let n = rest.strip_prefix("_oncc").or_else(|| rest.strip_prefix("cc"))?;
            let cc: u16 = n.parse().ok()?;
            let curve = ops
                .get(&format!("{name}_curvecc{cc}"))
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            Some(CcMod { cc, depth: v.parse().ok()?, curve })
        })
        .collect();
    mods.sort_by_key(|m| m.cc);
    Param { base, mods }
}

fn is_supported(name: &str) -> bool {
    const EXACT: &[&str] = &[
        "sample", "lokey", "hikey", "key", "lovel", "hivel", "lorand", "hirand", "pitch_keycenter",
        "pitch_keytrack", "tune", "transpose", "offset", "volume", "group_volume", "master_volume",
        "global_volume", "amplitude", "pan", "amp_veltrack", "ampeg_delay", "ampeg_attack", "ampeg_hold",
        "ampeg_decay", "ampeg_sustain", "ampeg_release", "ampeg_dynamic", "cutoff", "fil_type",
        "fil_veltrack", "resonance", "group", "off_by", "off_mode", "off_time", "note_polyphony",
        "polyphony", "rt_decay", "loop_mode", "trigger", "sustain_cc", "sw_lokey", "sw_hikey",
        "sw_default", "sw_last",
        // Cosmetic.
        "region_label", "group_label", "master_label", "global_label", "sw_label",
    ];
    // Prefixes followed by a CC number.
    const NUMBERED: &[&str] = &["locc", "hicc", "on_locc", "on_hicc"];
    if EXACT.contains(&name) {
        return true;
    }
    let stem = name.trim_end_matches(|c: char| c.is_ascii_digit());
    if stem.len() == name.len() {
        return false;
    }
    if NUMBERED.contains(&stem) {
        return true;
    }
    // Modulated params: name_onccN, name_curveccN, nameccN.
    let base = stem
        .strip_suffix("_oncc")
        .or_else(|| stem.strip_suffix("_curvecc"))
        .or_else(|| stem.strip_suffix("cc"));
    base.is_some_and(|b| {
        matches!(
            b,
            "tune" | "offset" | "volume" | "amplitude" | "pan" | "amp_veltrack" | "cutoff" | "ampeg_delay"
                | "ampeg_attack" | "ampeg_hold" | "ampeg_decay" | "ampeg_sustain" | "ampeg_release"
        )
    })
}

fn decode_all(paths: &[PathBuf], progress: &Progress) -> Vec<Result<Sample>> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(paths.len().max(1));
    let next = AtomicUsize::new(0);
    let mut results: Vec<Option<Result<Sample>>> = (0..paths.len()).map(|_| None).collect();
    std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Relaxed);
                        let Some(path) = paths.get(i) else { break };
                        mine.push((i, decode(path).with_context(|| format!("{}", path.display()))));
                        progress.done.fetch_add(1, Relaxed);
                    }
                    mine
                })
            })
            .collect();
        for w in workers {
            for (i, r) in w.join().unwrap_or_default() {
                results[i] = Some(r);
            }
        }
    });
    results.into_iter().map(|r| r.unwrap_or_else(|| Err(anyhow!("not decoded")))).collect()
}

/// Finds `path`, falling back to a case-insensitive match (libraries made on Windows).
fn resolve(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return Ok(path.to_owned());
    }
    let dir = path.parent().context("bad sample path")?;
    let want = path.file_name().context("bad sample path")?.to_string_lossy().to_lowercase();
    std::fs::read_dir(dir)
        .ok()
        .and_then(|entries| {
            entries
                .flatten()
                .find(|e| e.file_name().to_string_lossy().to_lowercase() == want)
                .map(|e| e.path())
        })
        .ok_or_else(|| anyhow!("sample not found"))
}

fn decode(path: &Path) -> Result<Sample> {
    let path = resolve(path)?;
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let (raw, channels, rate, bits) = match ext.as_str() {
        "flac" => {
            let mut r = claxon::FlacReader::open(&path)?;
            let info = r.streaminfo();
            let raw = r.samples().collect::<Result<Vec<i32>, _>>()?;
            (raw, info.channels as usize, info.sample_rate, info.bits_per_sample)
        }
        "wav" => {
            let mut r = hound::WavReader::open(&path)?;
            let spec = r.spec();
            let raw = match spec.sample_format {
                hound::SampleFormat::Int => r.samples::<i32>().collect::<Result<Vec<_>, _>>()?,
                hound::SampleFormat::Float => r
                    .samples::<f32>()
                    .map(|s| s.map(|v| (v * 8_388_607.0) as i32))
                    .collect::<Result<Vec<_>, _>>()?,
            };
            let bits = if spec.sample_format == hound::SampleFormat::Float { 24 } else { spec.bits_per_sample as u32 };
            (raw, spec.channels as usize, spec.sample_rate, bits)
        }
        other => bail!("unsupported sample format .{other}"),
    };
    if channels == 0 {
        bail!("no audio channels");
    }
    let out_ch = channels.min(2);
    let frames = raw.len() / channels;
    // Normalize per sample so quiet layers keep their resolution in 16 bits.
    let peak = raw.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0).max(1) as f32;
    let scale = 32767.0 / peak;
    let mut data = vec![0i16; (frames + PAD_START + PAD_END) * out_ch];
    for (f, frame) in raw.chunks_exact(channels).enumerate() {
        for c in 0..out_ch {
            data[(f + PAD_START) * out_ch + c] = (frame[c] as f32 * scale).round() as i16;
        }
    }
    let full_scale = (1u64 << (bits - 1)) as f32;
    Ok(Sample { data, channels: out_ch, rate, frames, gain: peak / full_scale / 32767.0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(ccs: &'a [f32; CC_COUNT], curves: &'a Curves) -> Ctx<'a> {
        Ctx { ccs, curves, note: 60, vel: 100 }
    }

    #[test]
    fn params_with_cc_mods() {
        let mut ops = Opcodes::new();
        for (k, v) in [
            ("ampeg_release", "0.5"),
            ("ampeg_release_oncc72", "2"),
            ("ampeg_releasecc64", "15"),
            ("ampeg_release_curvecc64", "11"),
            ("ampeg_release_shape", "-6"),
        ] {
            ops.insert(k.into(), v.into());
        }
        let p = param(&ops, "ampeg_release", 0.0);
        assert_eq!(p.base, 0.5);
        assert_eq!(p.mods, vec![CcMod { cc: 64, depth: 15.0, curve: 11 }, CcMod { cc: 72, depth: 2.0, curve: 0 }]);

        // Splendid's half-pedal curve: flat until 32, steps to 1 at 73.
        let curves = Curves::new(&[(11, vec![(0, 0.0), (32, 0.0), (64, 0.3), (73, 1.0), (127, 1.0)])]);
        let mut ccs = [0.0; CC_COUNT];
        ccs[72] = 0.35;
        assert!((p.eval(&ctx(&ccs, &curves)) - 1.2).abs() < 1e-4, "pedal up: 0.5 + 0.7");
        ccs[64] = 1.0;
        assert!((p.eval(&ctx(&ccs, &curves)) - 16.2).abs() < 1e-4, "pedal down adds 15 s");
    }

    #[test]
    fn amplitude_mods_multiply() {
        let p = Param { base: 100.0, mods: vec![CcMod { cc: 7, depth: 100.0, curve: 4 }] };
        let curves = Curves::new(&[]);
        let mut ccs = [0.0; CC_COUNT];
        ccs[7] = 0.5;
        assert!((p.eval_product(&ctx(&ccs, &curves)) - 0.25).abs() < 0.01);
    }

    #[test]
    fn extended_ccs() {
        let curves = Curves::new(&[]);
        let ccs = [0.0; CC_COUNT];
        let c = Ctx { ccs: &ccs, curves: &curves, note: 127, vel: 0 };
        assert_eq!(c.cc(CC_NOTE), 1.0);
        assert_eq!(c.cc(CC_VELOCITY), 0.0);
    }

    #[test]
    fn supported_opcodes() {
        for ok in ["ampeg_releasecc64", "amplitude_oncc7", "pan_curvecc10", "locc64", "on_hicc64", "sw_last"] {
            assert!(is_supported(ok), "{ok}");
        }
        for no in ["ampeg_release_shape", "label_cc7", "loop_start", "bend_up"] {
            assert!(!is_supported(no), "{no}");
        }
    }

    /// Loads the real library when present (skipped on machines without it).
    #[test]
    fn loads_splendid_if_installed() {
        let Some(home) = std::env::var_os("HOME") else { return };
        let path = Path::new(&home).join("Sounds/SFZ/SplendidGrandPiano/Splendid Grand Piano.sfz");
        if !path.exists() {
            return;
        }
        let inst = load_sfz(&path, &Progress::default()).unwrap();
        assert!(inst.warnings.is_empty(), "{:?}", inst.warnings);
        assert_eq!(inst.sustain_cc, 90);
        assert!(inst.regions.len() > 250);
        assert!((0..128).filter(|&k| (21..=108).contains(&k)).all(|k| !inst.by_key[k].is_empty()));
        // Lowest velocity layer is the PP samples through a 1 kHz low-pass.
        let soft = inst.regions.iter().find(|r| r.hivel == 40).unwrap();
        assert_eq!(soft.filter.as_ref().unwrap().cutoff_hz, 1000.0);
        // Resonance group only plays with the pedal down.
        assert!(inst.regions.iter().any(|r| r.cc_ranges.iter().any(|&(cc, lo, _)| cc == 64 && lo > 0.5)));
    }
}
