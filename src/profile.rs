//! Profiles: the whole setup (layers with their instrument and matrix, master strip) as a
//! JSON file in `profiles/` at the project root.
//!
//! Parameters are stored by their stable `Spec::key`, so profiles survive reordering the
//! matrix or renaming a label. Unknown keys are ignored and missing ones keep their default,
//! so older and newer files both load.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::params::{EQ_BANDS, MASTER_COUNT, Master, ParamId, Params};

const VERSION: u64 = 1;

/// Where a layer's instrument comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum InstrumentRef {
    /// A synth preset, by name.
    Synth(String),
    Sfz(PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
pub struct LayerProfile {
    pub instrument: InstrumentRef,
    pub params: Params,
    pub muted: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub layers: Vec<LayerProfile>,
    /// Layer shown in the matrix.
    pub current: usize,
    pub master: Master,
}

/// `profiles/` next to `Cargo.toml`.
pub fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("profiles")
}

pub fn path(name: &str) -> PathBuf {
    dir().join(format!("{name}.json"))
}

/// Profile names (file stems) in `dir()`, sorted.
pub fn list() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir()) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("json")))
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names
}

/// Keeps letters, digits, space, `-`, `_` and `.`; anything else becomes `_`.
pub fn sanitize_name(name: &str) -> String {
    let clean: String = name
        .trim()
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.') { c } else { '_' })
        .collect();
    clean.trim_start_matches('.').to_owned()
}

pub fn save(name: &str, profile: &Profile) -> Result<PathBuf> {
    let path = path(name);
    std::fs::create_dir_all(dir()).with_context(|| format!("cannot create {}", dir().display()))?;
    let text = serde_json::to_string_pretty(&to_json(profile))? + "\n";
    std::fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

pub fn load(name: &str) -> Result<Profile> {
    let path = path(name);
    let text = std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let value: Value = serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", path.display()))?;
    from_json(&value).with_context(|| format!("bad profile {}", path.display()))
}

pub fn to_json(p: &Profile) -> Value {
    let layers: Vec<Value> = p
        .layers
        .iter()
        .map(|l| {
            let instrument = match &l.instrument {
                InstrumentRef::Synth(name) => json!({ "synth": name }),
                InstrumentRef::Sfz(path) => json!({ "sfz": home_relative(path) }),
            };
            // Matrix order, so the file reads like the screen.
            let params: Map<String, Value> =
                ParamId::ALL.iter().map(|&id| (id.spec().key.to_owned(), number(l.params.get(id)))).collect();
            json!({ "instrument": instrument, "muted": l.muted, "params": params })
        })
        .collect();
    let eq: Map<String, Value> =
        (1..MASTER_COUNT).map(|i| (Master::label(i).to_owned(), number(p.master.get(i)))).collect();
    json!({
        "version": VERSION,
        "current_layer": p.current,
        "master": { "level_db": number(p.master.level_db()), "eq_db": eq },
        "layers": layers,
    })
}

pub fn from_json(v: &Value) -> Result<Profile> {
    if let Some(version) = v.get("version").and_then(Value::as_u64)
        && version > VERSION
    {
        bail!("made by a newer OFFENBACK (version {version})");
    }
    let mut master = Master::default();
    if let Some(m) = v.get("master") {
        if let Some(db) = m.get("level_db").and_then(Value::as_f64) {
            master.set(0, db as f32);
        }
        for i in 1..MASTER_COUNT {
            if let Some(db) = m.get("eq_db").and_then(|eq| eq.get(Master::label(i))).and_then(Value::as_f64) {
                master.set(i, db as f32);
            }
        }
    }
    let Some(layers) = v.get("layers").and_then(Value::as_array) else { bail!("no \"layers\" list") };
    let layers = layers
        .iter()
        .enumerate()
        .map(|(i, l)| layer_from_json(l).with_context(|| format!("layer {}", i + 1)))
        .collect::<Result<Vec<_>>>()?;
    if layers.is_empty() {
        bail!("no layers");
    }
    let current = v.get("current_layer").and_then(Value::as_u64).unwrap_or(0) as usize;
    Ok(Profile { current: current.min(layers.len() - 1), layers, master })
}

fn layer_from_json(l: &Value) -> Result<LayerProfile> {
    let inst = l.get("instrument");
    let instrument = if let Some(name) = inst.and_then(|i| i.get("synth")).and_then(Value::as_str) {
        InstrumentRef::Synth(name.to_owned())
    } else if let Some(path) = inst.and_then(|i| i.get("sfz")).and_then(Value::as_str) {
        InstrumentRef::Sfz(expand_home(path))
    } else {
        bail!("\"instrument\" needs \"synth\" or \"sfz\"");
    };
    let mut params = Params::default();
    if let Some(map) = l.get("params").and_then(Value::as_object) {
        for (key, value) in map {
            if let (Some(id), Some(v)) = (ParamId::from_key(key), value.as_f64()) {
                params.set(id, v as f32);
            }
        }
    }
    let muted = l.get("muted").and_then(Value::as_bool).unwrap_or(false);
    Ok(LayerProfile { instrument, params, muted })
}

/// Rounded so the file shows `-3.5`, not `-3.4999998`.
fn number(v: f32) -> Value {
    json!((v as f64 * 10_000.0).round() / 10_000.0)
}

/// `~/…` for paths under the home folder, so profiles move between machines.
fn home_relative(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

// The EQ is keyed by band label; make sure there is one per band.
const _: () = assert!(MASTER_COUNT == EQ_BANDS.len() + 1);

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Profile {
        let mut piano = Params::default();
        piano.nudge(ParamId::Volume, -7);
        piano.nudge(ParamId::LowPass, -30);
        let mut bass = Params::default();
        bass.nudge(ParamId::Glide, 12);
        bass.nudge(ParamId::Transpose, -12);
        let mut master = Master::default();
        master.nudge(0, -6);
        master.nudge(4, 5);
        master.nudge(10, -24);
        let home = std::env::var("HOME").unwrap_or_default();
        Profile {
            layers: vec![
                LayerProfile {
                    instrument: InstrumentRef::Sfz(PathBuf::from(format!("{home}/Sounds/SFZ/Piano/Piano.sfz"))),
                    params: piano,
                    muted: false,
                },
                LayerProfile { instrument: InstrumentRef::Synth("Saw".into()), params: bass, muted: true },
            ],
            current: 1,
            master,
        }
    }

    fn close(a: &Profile, b: &Profile) -> bool {
        let near = |x: f32, y: f32| (x - y).abs() <= 1e-3 * x.abs().max(1.0);
        a.current == b.current
            && a.layers.len() == b.layers.len()
            && a.layers.iter().zip(&b.layers).all(|(x, y)| {
                x.instrument == y.instrument
                    && x.muted == y.muted
                    && ParamId::ALL.iter().all(|&id| near(x.params.get(id), y.params.get(id)))
            })
            && (0..MASTER_COUNT).all(|i| near(a.master.get(i), b.master.get(i)))
    }

    #[test]
    fn round_trips_through_json_text() {
        let p = sample();
        let text = serde_json::to_string_pretty(&to_json(&p)).unwrap();
        assert!(text.contains("\"sfz\": \"~/Sounds/SFZ/Piano/Piano.sfz\""), "{text}");
        assert!(text.contains("\"low_pass\"") && text.contains("\"1k\""), "{text}");
        let back = from_json(&serde_json::from_str(&text).unwrap()).unwrap();
        assert!(close(&p, &back), "{back:?}");
    }

    #[test]
    fn tolerates_missing_and_unknown_fields() {
        let v = json!({
            "layers": [{ "instrument": { "synth": "Sine" }, "params": { "volume": -6, "warp_drive": 3 } }],
            "master": { "eq_db": { "1k": 40 } },
            "current_layer": 9,
        });
        let p = from_json(&v).unwrap();
        assert_eq!(p.layers[0].params.get(ParamId::Volume), -6.0);
        assert!(p.layers[0].params.is_default(ParamId::Reverb));
        assert_eq!(p.master.get(6), 12.0, "clamped to the band range");
        assert_eq!(p.current, 0);
    }

    #[test]
    fn rejects_bad_profiles() {
        assert!(from_json(&json!({})).is_err());
        assert!(from_json(&json!({ "layers": [] })).is_err());
        assert!(from_json(&json!({ "layers": [{ "params": {} }] })).is_err());
        assert!(from_json(&json!({ "version": 99, "layers": [{ "instrument": { "synth": "Sine" } }] })).is_err());
    }

    #[test]
    fn names_are_safe_file_stems() {
        assert_eq!(sanitize_name("  Jazz trio  "), "Jazz trio");
        assert_eq!(sanitize_name("../etc/passwd"), "_etc_passwd");
        assert_eq!(sanitize_name("a/b:c"), "a_b_c");
    }
}
