//! SFZ text parsing: preprocessor (`#define`, `#include`, comments), header inheritance
//! (`<control>`, `<global>`, `<master>`, `<group>`, `<region>`, `<curve>`) and opcode maps.
//!
//! The output is untyped: one flattened opcode map per region. `instrument.rs` gives it meaning.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub type Opcodes = HashMap<String, String>;

pub struct RawSfz {
    /// One entry per `<region>`, with global/master/group opcodes merged in.
    /// `sample` is already resolved to a full path.
    pub regions: Vec<Opcodes>,
    /// `<curve>` sections: index and (point, value) pairs.
    pub curves: Vec<(u16, Vec<(u8, f32)>)>,
    /// `set_ccN` / `set_hdccN` from `<control>`, normalized to 0..1.
    pub cc_defaults: Vec<(u16, f32)>,
}

const MAX_INCLUDE_DEPTH: usize = 16;

pub fn parse_file(path: &Path) -> Result<RawSfz> {
    let root = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let mut text = String::new();
    let mut defines = Vec::new();
    preprocess(path, &root, &mut defines, &mut text, 0)?;
    parse_text(&text, &root)
}

/// Expands includes and defines, strips comments. Defines apply from where they appear
/// onwards, so files can redefine a name between two includes (Splendid does).
fn preprocess(
    path: &Path,
    root: &Path,
    defines: &mut Vec<(String, String)>,
    out: &mut String,
    depth: usize,
) -> Result<()> {
    if depth > MAX_INCLUDE_DEPTH {
        bail!("#include nested too deep at {}", path.display());
    }
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let source = strip_comments(&String::from_utf8_lossy(&bytes));

    for line in source.lines() {
        let mut rest = line;
        while let Some(i) = find_directive(rest) {
            out.push_str(&substitute(&rest[..i], defines));
            let after = &rest[i..];
            if let Some(args) = after.strip_prefix("#define") {
                let (name, args) = next_word(args);
                let (value, args) = next_word(args);
                rest = args;
                if !name.is_empty() {
                    defines.retain(|(n, _)| n != name);
                    defines.push((name.to_owned(), value.to_owned()));
                    // Longest first so `$DIR` never shadows `$DIRX`.
                    defines.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));
                }
            } else {
                let args = &after["#include".len()..];
                let open = args.find('"').context("#include without opening quote")?;
                let close = args[open + 1..].find('"').context("#include without closing quote")? + open + 1;
                let file = substitute(&args[open + 1..close], defines).replace('\\', "/");
                rest = &args[close + 1..];
                out.push('\n');
                preprocess(&root.join(file), root, defines, out, depth + 1)?;
                out.push('\n');
            }
        }
        out.push_str(&substitute(rest, defines));
        out.push('\n');
    }
    Ok(())
}

/// Returns the first whitespace-delimited word of `s` and what follows it.
fn next_word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    let end = s.find(char::is_whitespace).unwrap_or(s.len());
    (&s[..end], &s[end..])
}

fn find_directive(s: &str) -> Option<usize> {
    let a = s.find("#define");
    let b = s.find("#include");
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn substitute(s: &str, defines: &[(String, String)]) -> String {
    let mut s = s.to_owned();
    if s.contains('$') {
        for (name, value) in defines {
            s = s.replace(name.as_str(), value);
        }
    }
    s
}

/// Removes `// line` and `/* block */` comments in one pass, so `//****` banners are line
/// comments. Newlines inside block comments are kept so opcode boundaries survive.
fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        let line = rest.find("//");
        let block = rest.find("/*");
        match (line, block) {
            (Some(l), b) if b.is_none_or(|b| l <= b) => {
                out.push_str(&rest[..l]);
                rest = rest[l..].find('\n').map_or("", |n| &rest[l + n..]);
            }
            (_, Some(b)) => {
                out.push_str(&rest[..b]);
                match rest[b..].find("*/") {
                    Some(end) => {
                        out.extend(rest[b..b + end].chars().filter(|&c| c == '\n'));
                        rest = &rest[b + end + 2..];
                    }
                    None => return out,
                }
            }
            _ => {
                out.push_str(rest);
                return out;
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Header {
    None,
    Control,
    Global,
    Master,
    Group,
    Region,
    Curve,
    /// Headers we don't use (`<effect>`, `<midi>`...): their opcodes are dropped.
    Ignored,
}

struct State {
    header: Header,
    default_path: String,
    global: Opcodes,
    master: Opcodes,
    group: Opcodes,
    region: Opcodes,
    curve: Opcodes,
    out: RawSfz,
}

impl State {
    fn close_section(&mut self, root: &Path) {
        match self.header {
            Header::Region => {
                let mut merged = self.global.clone();
                merged.extend(self.master.iter().map(|(k, v)| (k.clone(), v.clone())));
                merged.extend(self.group.iter().map(|(k, v)| (k.clone(), v.clone())));
                merged.extend(self.region.drain());
                if let Some(sample) = merged.get("sample") {
                    let rel = format!("{}{}", self.default_path, sample).replace('\\', "/");
                    let full: PathBuf = root.join(rel);
                    merged.insert("sample".into(), full.to_string_lossy().into_owned());
                }
                self.out.regions.push(merged);
            }
            Header::Curve => {
                let index = self.curve.get("curve_index").and_then(|v| v.parse().ok());
                if let Some(index) = index {
                    let mut points: Vec<(u8, f32)> = self
                        .curve
                        .iter()
                        .filter_map(|(k, v)| {
                            let p = k.strip_prefix('v')?.parse::<u8>().ok().filter(|&p| p <= 127)?;
                            Some((p, v.parse().ok()?))
                        })
                        .collect();
                    points.sort_by_key(|&(p, _)| p);
                    self.out.curves.push((index, points));
                }
                self.curve.clear();
            }
            _ => {}
        }
    }

    fn open_section(&mut self, name: &str) {
        self.header = match name {
            "control" => Header::Control,
            "global" => {
                self.global.clear();
                self.master.clear();
                self.group.clear();
                Header::Global
            }
            "master" => {
                self.master.clear();
                self.group.clear();
                Header::Master
            }
            "group" => {
                self.group.clear();
                Header::Group
            }
            "region" => Header::Region,
            "curve" => Header::Curve,
            _ => Header::Ignored,
        };
    }

    fn opcode(&mut self, key: &str, value: &str) {
        let (key, value) = (key.to_owned(), value.to_owned());
        match self.header {
            Header::Control => {
                if key == "default_path" {
                    self.default_path = value.replace('\\', "/");
                } else if let Some(cc) = key.strip_prefix("set_hdcc").and_then(|n| n.parse().ok()) {
                    let v: f32 = value.parse().unwrap_or(0.0);
                    self.out.cc_defaults.push((cc, v.clamp(0.0, 1.0)));
                } else if let Some(cc) = key.strip_prefix("set_cc").and_then(|n| n.parse().ok()) {
                    let v: f32 = value.parse().unwrap_or(0.0);
                    self.out.cc_defaults.push((cc, (v / 127.0).clamp(0.0, 1.0)));
                }
            }
            Header::Global => _ = self.global.insert(key, value),
            Header::Master => _ = self.master.insert(key, value),
            Header::Group => _ = self.group.insert(key, value),
            Header::Region => _ = self.region.insert(key, value),
            Header::Curve => _ = self.curve.insert(key, value),
            Header::None | Header::Ignored => {}
        }
    }
}

fn parse_text(text: &str, root: &Path) -> Result<RawSfz> {
    let mut st = State {
        header: Header::None,
        default_path: String::new(),
        global: Opcodes::new(),
        master: Opcodes::new(),
        group: Opcodes::new(),
        region: Opcodes::new(),
        curve: Opcodes::new(),
        out: RawSfz { regions: Vec::new(), curves: Vec::new(), cc_defaults: Vec::new() },
    };

    for line in text.lines() {
        let mut rest = line;
        loop {
            let (chunk, header) = match rest.find('<') {
                Some(open) => match rest[open..].find('>') {
                    Some(close) => (&rest[..open], Some((&rest[open + 1..open + close], open + close + 1))),
                    None => (rest, None),
                },
                None => (rest, None),
            };
            parse_opcodes(chunk, &mut st);
            match header {
                Some((name, next)) => {
                    st.close_section(root);
                    st.open_section(name.trim());
                    rest = &rest[next..];
                }
                None => break,
            }
        }
    }
    st.close_section(root);
    Ok(st.out)
}

/// Splits `a=1 sample=My File.wav b=2` into opcodes. A value runs until the next word
/// that looks like `name=`, so file names may contain spaces.
fn parse_opcodes(chunk: &str, st: &mut State) {
    let mut current: Option<(&str, String)> = None;
    for word in chunk.split_whitespace() {
        let starts_opcode = word.find('=').is_some_and(|eq| {
            eq > 0 && word[..eq].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        });
        if starts_opcode {
            if let Some((k, v)) = current.take() {
                st.opcode(k, &v);
            }
            let eq = word.find('=').unwrap_or(0);
            current = Some((&word[..eq], word[eq + 1..].to_owned()));
        } else if let Some((_, v)) = current.as_mut() {
            if !v.is_empty() {
                v.push(' ');
            }
            v.push_str(word);
        }
    }
    if let Some((k, v)) = current {
        st.opcode(k, &v);
    }
}

/// Parses a key given as a MIDI number or a note name (`c4` = 60, `C#0` = 13, `eb2`).
pub fn parse_key(s: &str) -> Option<i16> {
    if let Ok(n) = s.parse::<i16>() {
        return Some(n);
    }
    let s = s.to_ascii_lowercase();
    let mut chars = s.chars();
    let base = match chars.next()? {
        'c' => 0,
        'd' => 2,
        'e' => 4,
        'f' => 5,
        'g' => 7,
        'a' => 9,
        'b' => 11,
        _ => return None,
    };
    let rest: String = chars.collect();
    let (shift, octave) = if let Some(o) = rest.strip_prefix('#') {
        (1, o)
    } else if let Some(o) = rest.strip_prefix('b') {
        (-1, o)
    } else {
        (0, rest.as_str())
    };
    let octave: i16 = octave.parse().ok()?;
    Some((octave + 1) * 12 + base + shift)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tree(files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("offenback-sfz-{}-{}", std::process::id(), files[0].1.len()));
        let _ = std::fs::remove_dir_all(&dir);
        for (name, content) in files {
            let p = dir.join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        dir
    }

    #[test]
    fn defines_includes_and_inheritance() {
        let dir = write_tree(&[
            (
                "main.sfz",
                "//*** banner /* not a block\n// comment\n#define $EXT flac\n<control> default_path=Samples/ set_cc7=127 set_hdcc10=0.5\n\
                 <global> volume=-3 /* block\ncomment */ ampeg_release=1\n\
                 <group> lovel=1 hivel=64 #define $DYN pp\n#include \"Data/r.txt\"\n\
                 <group> lovel=65 hivel=127\n#define $DYN ff\n#include \"Data/r.txt\"\n\
                 <curve>curve_index=7 v000=0 v127=1\n",
            ),
            ("Data/r.txt", "<region> key=c4 sample=$DYN C4.$EXT\n<region> lokey=61 hikey=62 sample=$DYN D4.$EXT volume=0"),
        ]);
        let raw = parse_file(&dir.join("main.sfz")).unwrap();
        assert_eq!(raw.regions.len(), 4);
        let r0 = &raw.regions[0];
        assert!(r0["sample"].ends_with("Samples/pp C4.flac"), "{}", r0["sample"]);
        assert_eq!(r0["volume"], "-3");
        assert_eq!(r0["ampeg_release"], "1");
        assert_eq!(r0["hivel"], "64");
        assert_eq!(raw.regions[1]["volume"], "0", "region overrides global");
        assert!(raw.regions[2]["sample"].ends_with("Samples/ff C4.flac"));
        assert_eq!(raw.regions[3]["lovel"], "65");
        assert_eq!(raw.curves, vec![(7, vec![(0, 0.0), (127, 1.0)])]);
        assert_eq!(raw.cc_defaults, vec![(7, 1.0), (10, 0.5)]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn note_names() {
        assert_eq!(parse_key("c4"), Some(60));
        assert_eq!(parse_key("C#0"), Some(13));
        assert_eq!(parse_key("eb2"), Some(39));
        assert_eq!(parse_key("21"), Some(21));
        assert_eq!(parse_key("-1"), Some(-1));
        assert_eq!(parse_key("x"), None);
    }
}

