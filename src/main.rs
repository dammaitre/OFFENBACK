mod audio;
mod engine;
mod midi;
mod tui;

use anyhow::{Context, Result, bail};

const USAGE: &str = "\
OFFENBACK - terminal MIDI keyboard controller

Usage: offenback [OPTIONS]

  --list            print MIDI input ports and exit
  --list-outputs    print audio outputs and exit
  --port NAME       connect to the first MIDI port containing NAME, skipping the picker
  --output NAME     play through the first audio output whose id or name contains NAME
  --buffer FRAMES   audio buffer size (32..1024); default lets the audio server choose";

fn main() -> Result<()> {
    let mut port = None;
    let mut output = None;
    let mut buffer = None;
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
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }

    let mut app = tui::App::new(output, buffer);
    if let Some(query) = port {
        app.connect(&query);
    }
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal);
    ratatui::restore();
    result
}
