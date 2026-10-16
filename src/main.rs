mod audio;
mod engine;
mod instrument;
mod midi;
mod params;
mod recorder;
mod reverb;
mod sfz;
mod tui;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};

const USAGE: &str = "\
OFFENBACK - terminal MIDI keyboard controller

Usage: offenback [OPTIONS]

  --list            print MIDI input ports and exit
  --list-outputs    print audio outputs and exit
  --port NAME       connect to the first MIDI port containing NAME, skipping the picker
  --output NAME     play through the first audio output whose id or name contains NAME
  --buffer FRAMES   audio buffer size, 32..1024 (default 128); `]` past 1024 lets the server choose
  --sfz FILE        instrument to play (default: Splendid Grand Piano in ~/Sounds/SFZ if present)
  --inspect FILE    load an SFZ instrument, print a summary and exit";

const DEFAULT_SFZ: &str = "Sounds/SFZ/SplendidGrandPiano/Splendid Grand Piano.sfz";

fn main() -> Result<()> {
    let mut port = None;
    let mut output = None;
    let mut buffer = None;
    let mut sfz = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--list" => {
                for name in midi::list_ports()? {
                    println!("{name}");
                }
                return Ok(());
            }
            "--list-outputs" => {
                println!("host: {}", audio::host_name());
                for o in audio::list_outputs()? {
                    println!("{:<40} {}", o.name, o.id);
                }
                return Ok(());
            }
            "--port" => port = Some(args.next().unwrap_or_default()),
            "--output" => output = Some(args.next().unwrap_or_default()),
            "--buffer" => {
                let v = args.next().unwrap_or_default();
                buffer = Some(v.parse::<u32>().with_context(|| format!("bad --buffer {v:?}"))?);
            }
            "--sfz" => sfz = Some(PathBuf::from(args.next().unwrap_or_default())),
            "--inspect" => return inspect(&PathBuf::from(args.next().unwrap_or_default())),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }

    let sfz = sfz.or_else(|| {
        let p = PathBuf::from(std::env::var_os("HOME")?).join(DEFAULT_SFZ);
        p.exists().then_some(p)
    });
    let mut app = tui::App::new(output, buffer, sfz);
    if let Some(query) = port {
        app.connect(&query);
    }
    app.ask_for_port_if_needed();

    let mut terminal = ratatui::init();
    // Mouse reporting carries touchpad scrolling. Turn it off again even on panic, or the
    // shell is left printing escape codes on every mouse move.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = ratatui::crossterm::execute!(std::io::stdout(), DisableMouseCapture);
        hook(info);
    }));
    ratatui::crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
    let result = app.run(&mut terminal);
    let _ = ratatui::crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn inspect(path: &std::path::Path) -> Result<()> {
    let start = std::time::Instant::now();
    let inst = instrument::load_sfz(path, &instrument::Progress::default())?;
    println!("{}", inst.name);
    println!("  regions   {}", inst.regions.len());
    println!("  samples   {} ({} MB in RAM)", inst.samples.len(), inst.memory_bytes() >> 20);
    println!("  loaded in {:.2} s", start.elapsed().as_secs_f32());
    println!("  sustain   CC{}", inst.sustain_cc);
    if let Some((lo, hi)) = inst.sw_range {
        println!("  keyswitch {}..{} (default {:?})", midi::note_name(lo), midi::note_name(hi), inst.sw_default.map(midi::note_name));
    }
    let covered: Vec<usize> = (0..128).filter(|&k| !inst.by_key[k].is_empty()).collect();
    if let (Some(lo), Some(hi)) = (covered.first(), covered.last()) {
        println!("  keys      {}..{}", midi::note_name(*lo as u8), midi::note_name(*hi as u8));
    }
    println!("  cc-triggered regions {}", inst.cc_triggered.len());
    if !inst.unsupported.is_empty() {
        println!("  ignored opcodes: {}", inst.unsupported.join(" "));
    }
    for w in &inst.warnings {
        println!("  warning: {w}");
    }
    Ok(())
}
