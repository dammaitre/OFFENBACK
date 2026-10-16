mod midi;
mod tui;

use anyhow::{Result, bail};

const USAGE: &str = "\
OFFENBACK - terminal MIDI keyboard controller

Usage: offenback [--list] [--port NAME]

  --list         print MIDI input ports and exit
  --port NAME    connect to the first port containing NAME, skipping the picker";

fn main() -> Result<()> {
    let mut port = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--list" => {
                for name in midi::list_ports()? {
                    println!("{name}");
                }
                return Ok(());
            }
            "--port" => port = Some(args.next().unwrap_or_default()),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }

    let mut app = tui::App::new();
    if let Some(query) = port {
        app.connect(&query);
    }
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal);
    ratatui::restore();
    result
}
