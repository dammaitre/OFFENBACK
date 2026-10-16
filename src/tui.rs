//! Terminal UI: MIDI port picker, audio output picker and live monitor.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};
use midir::MidiInputConnection;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::audio::{self, AudioOut, OutputInfo};
use crate::engine::{EngineCmd, Stats};
use crate::midi::{self, MidiEvent, TimedEvent, note_name};

const LOG_CAPACITY: usize = 500;
const SUSTAIN_CC: u8 = 64;
/// Buffer sizes offered with `[` / `]`; past the end means "server default".
const BUFFER_SIZES: [u32; 6] = [32, 64, 128, 256, 512, 1024];
const TEST_NOTE: u8 = 60;
const TEST_NOTE_LENGTH: Duration = Duration::from_millis(600);

#[derive(Clone, Copy)]
enum Screen {
    PortPicker,
    OutputPicker,
    Monitor,
}

struct Connection {
    name: String,
    _conn: MidiInputConnection<()>,
}

pub struct App {
    screen: Screen,
    /// Where the output picker returns to.
    previous: Screen,
    ports: Vec<String>,
    port_list: ListState,
    connection: Option<Connection>,
    ui_tx: Sender<TimedEvent>,
    ui_rx: Receiver<TimedEvent>,
    engine_tx: Sender<EngineCmd>,
    engine_rx: Receiver<EngineCmd>,
    stats: Arc<Stats>,
    audio: Option<AudioOut>,
    audio_error: Option<String>,
    outputs: Vec<OutputInfo>,
    output_list: ListState,
    /// Output id chosen by the user; `None` follows the system default.
    output: Option<String>,
    /// Index into `BUFFER_SIZES`; `None` lets the server choose.
    buffer: Option<usize>,
    test_note_off: Option<Instant>,
    log: VecDeque<TimedEvent>,
    held: [bool; 128],
    sustain: bool,
    event_count: u64,
    status: String,
    quit: bool,
}

impl App {
    pub fn new(output: Option<String>, buffer: Option<u32>) -> Self {
        let (ui_tx, ui_rx) = crossbeam_channel::bounded(4096);
        let (engine_tx, engine_rx) = crossbeam_channel::bounded(1024);
        let buffer = buffer.map(|b| {
            BUFFER_SIZES.iter().position(|&s| s >= b).unwrap_or(BUFFER_SIZES.len() - 1)
        });
        let mut app = Self {
            screen: Screen::PortPicker,
            previous: Screen::PortPicker,
            ports: Vec::new(),
            port_list: ListState::default(),
            connection: None,
            ui_tx,
            ui_rx,
            engine_tx,
            engine_rx,
            stats: Arc::new(Stats::default()),
            audio: None,
            audio_error: None,
            outputs: Vec::new(),
            output_list: ListState::default(),
            output,
            buffer,
            test_note_off: None,
            log: VecDeque::with_capacity(LOG_CAPACITY),
            held: [false; 128],
            sustain: false,
            event_count: 0,
            status: String::new(),
            quit: false,
        };
        app.refresh_ports();
        app.open_audio();
        app
    }

    /// Connects straight away, skipping the picker.
    pub fn connect(&mut self, query: &str) {
        let ui_tx = self.ui_tx.clone();
        let engine_tx = self.engine_tx.clone();
        let result = midi::connect(query, move |ev| {
            let at = Instant::now();
            let _ = engine_tx.try_send(EngineCmd::Midi { event: ev.event, at });
            let _ = ui_tx.try_send(ev);
        });
        match result {
            Ok((name, conn)) => {
                self.status = format!("connected to {name}");
                self.connection = Some(Connection { name, _conn: conn });
                self.reset_state();
                self.screen = Screen::Monitor;
            }
            Err(e) => self.status = e.to_string(),
        }
    }

    pub fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            self.drain_events();
            if self.test_note_off.is_some_and(|t| Instant::now() >= t) {
                self.test_note_off = None;
                self.send_engine(MidiEvent::NoteOff { ch: 0, note: TEST_NOTE, vel: 0 });
            }
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(16))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        self.on_key(key.code);
                    }
                }
            }
        }
        Ok(())
    }

    fn buffer_frames(&self) -> Option<u32> {
        self.buffer.map(|i| BUFFER_SIZES[i])
    }

    fn open_audio(&mut self) {
        // Drop the old stream first so the device is free.
        self.audio = None;
        while self.engine_rx.try_recv().is_ok() {}
        self.stats.reset_latency();
        let buffer = self.buffer_frames();
        match audio::open(self.output.as_deref(), buffer, self.engine_rx.clone(), self.stats.clone()) {
            Ok(out) => {
                self.audio_error = None;
                self.status = format!("audio → {}", out.device_name);
                self.audio = Some(out);
            }
            Err(e) => self.audio_error = Some(format!("{e:#}")),
        }
    }

    fn send_engine(&self, event: MidiEvent) {
        let _ = self.engine_tx.try_send(EngineCmd::Midi { event, at: Instant::now() });
    }

    fn refresh_ports(&mut self) {
        match midi::list_ports() {
            Ok(ports) => {
                self.status = format!("{} MIDI input port(s) found", ports.len());
                self.ports = ports;
            }
            Err(e) => {
                self.status = e.to_string();
                self.ports.clear();
            }
        }
        self.port_list.select((!self.ports.is_empty()).then_some(0));
    }

    fn open_output_picker(&mut self) {
        match audio::list_outputs() {
            Ok(outputs) => self.outputs = outputs,
            Err(e) => {
                self.status = e.to_string();
                self.outputs.clear();
            }
        }
        let current = self
            .output
            .as_ref()
            .and_then(|id| self.outputs.iter().position(|o| &o.id == id));
        self.output_list.select(current.or((!self.outputs.is_empty()).then_some(0)));
        if !matches!(self.screen, Screen::OutputPicker) {
            self.previous = self.screen;
        }
        self.screen = Screen::OutputPicker;
    }

    fn reset_state(&mut self) {
        while self.ui_rx.try_recv().is_ok() {}
        self.log.clear();
        self.held = [false; 128];
        self.sustain = false;
        self.event_count = 0;
    }

    fn drain_events(&mut self) {
        for ev in self.ui_rx.try_iter() {
            match ev.event {
                MidiEvent::NoteOn { note, .. } => self.held[note as usize] = true,
                MidiEvent::NoteOff { note, .. } => self.held[note as usize] = false,
                MidiEvent::ControlChange { cc: SUSTAIN_CC, value, .. } => self.sustain = value >= 64,
                _ => {}
            }
            self.event_count += 1;
            if self.log.len() == LOG_CAPACITY {
                self.log.pop_front();
            }
            self.log.push_back(ev);
        }
    }

    fn on_key(&mut self, code: KeyCode) {
        match (self.screen, code) {
            (Screen::OutputPicker, KeyCode::Esc | KeyCode::Char('q')) => self.screen = self.previous,
            (_, KeyCode::Char('q') | KeyCode::Esc) => self.quit = true,

            (Screen::PortPicker, KeyCode::Up | KeyCode::Char('k')) => self.port_list.select_previous(),
            (Screen::PortPicker, KeyCode::Down | KeyCode::Char('j')) => self.port_list.select_next(),
            (Screen::PortPicker, KeyCode::Char('r')) => self.refresh_ports(),
            (Screen::PortPicker, KeyCode::Enter) => {
                if let Some(name) = self.port_list.selected().and_then(|i| self.ports.get(i)) {
                    let name = name.clone();
                    self.connect(&name);
                }
            }

            (Screen::OutputPicker, KeyCode::Up | KeyCode::Char('k')) => self.output_list.select_previous(),
            (Screen::OutputPicker, KeyCode::Down | KeyCode::Char('j')) => self.output_list.select_next(),
            (Screen::OutputPicker, KeyCode::Char('r')) => self.open_output_picker(),
            (Screen::OutputPicker, KeyCode::Enter) => {
                if let Some(out) = self.output_list.selected().and_then(|i| self.outputs.get(i)) {
                    self.output = Some(out.id.clone());
                    self.open_audio();
                    self.screen = self.previous;
                }
            }

            (Screen::Monitor, KeyCode::Char('p')) => {
                self.connection = None;
                self.screen = Screen::PortPicker;
                self.refresh_ports();
            }
            (Screen::Monitor, KeyCode::Char('c')) => self.log.clear(),
            (Screen::Monitor, KeyCode::Char('r')) => self.stats.reset_latency(),

            // Available everywhere except the output picker.
            (_, KeyCode::Char('o')) => self.open_output_picker(),
            (_, KeyCode::Char('t')) => {
                self.send_engine(MidiEvent::NoteOn { ch: 0, note: TEST_NOTE, vel: 100 });
                self.test_note_off = Some(Instant::now() + TEST_NOTE_LENGTH);
            }
            (_, KeyCode::Char('[')) => {
                self.buffer = Some(self.buffer.map_or(BUFFER_SIZES.len() - 1, |i| i.saturating_sub(1)));
                self.open_audio();
            }
            (_, KeyCode::Char(']')) => {
                self.buffer = self.buffer.and_then(|i| (i + 1 < BUFFER_SIZES.len()).then_some(i + 1));
                self.open_audio();
            }
            _ => {}
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let [title, body, audio_area, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(5),
            Constraint::Length(1),
        ])
        .areas(f.area());

        f.render_widget(Line::from(" OFFENBACK ".bold().reversed()), title);
        match self.screen {
            Screen::PortPicker => self.draw_picker(f, body),
            Screen::OutputPicker => self.draw_output_picker(f, body),
            Screen::Monitor => self.draw_monitor(f, body),
        }
        self.draw_audio(f, audio_area);
        let keys = match self.screen {
            Screen::PortPicker => "↑↓ select  ⏎ connect  r refresh  o output  t test  [ ] buffer  q quit",
            Screen::OutputPicker => "↑↓ select  ⏎ use  r refresh  esc back",
            Screen::Monitor => "t test  [ ] buffer  o output  r reset worst  p port  c clear  q quit",
        };
        f.render_widget(
            Line::from(vec![Span::raw(format!(" {keys}  │ ")), Span::raw(&self.status).dim()]),
            footer,
        );
    }

    fn draw_picker(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::bordered().title(" MIDI input ");
        if self.ports.is_empty() {
            let msg = Paragraph::new("No MIDI input found. Plug a keyboard or start fake_keyboard, then press r.")
                .block(block);
            f.render_widget(msg, area);
            return;
        }
        let items: Vec<ListItem> = self.ports.iter().map(|p| ListItem::new(p.as_str())).collect();
        f.render_stateful_widget(picker_list(items, block), area, &mut self.port_list);
    }

    fn draw_output_picker(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::bordered().title(format!(" Audio output ({}) ", audio::host_name()));
        if self.outputs.is_empty() {
            f.render_widget(Paragraph::new("No audio output found. Press r to refresh.").block(block), area);
            return;
        }
        let items: Vec<ListItem> = self
            .outputs
            .iter()
            .map(|o| {
                let current = self.output.as_deref() == Some(o.id.as_str());
                let mark = if current { "● " } else { "  " };
                ListItem::new(Line::from(vec![
                    Span::raw(mark).green(),
                    Span::raw(o.name.clone()),
                    Span::raw(format!("  {}", o.id)).dim(),
                ]))
            })
            .collect();
        f.render_stateful_widget(picker_list(items, block), area, &mut self.output_list);
    }

    fn draw_monitor(&mut self, f: &mut Frame, area: Rect) {
        let [info, log_area] = Layout::vertical([Constraint::Length(4), Constraint::Min(0)]).areas(area);

        let port = self.connection.as_ref().map_or("-", |c| c.name.as_str());
        let held: Vec<String> = (0..128u8).filter(|&n| self.held[n as usize]).map(note_name).collect();
        let sustain = if self.sustain { "ON".green().bold() } else { "off".dim() };
        let info_text = vec![
            Line::from(vec![
                "Port ".dim(),
                Span::raw(port),
                "   Events ".dim(),
                Span::raw(self.event_count.to_string()),
                "   Sustain ".dim(),
                sustain,
            ]),
            Line::from(vec![format!("Held ({}) ", held.len()).dim(), Span::raw(held.join(" ")).cyan()]),
        ];
        f.render_widget(Paragraph::new(info_text).block(Block::bordered()), info);

        let block = Block::bordered().title(" Events ");
        let visible = block.inner(log_area).height as usize;
        let lines: Vec<Line> = self
            .log
            .iter()
            .skip(self.log.len().saturating_sub(visible))
            .map(format_event)
            .collect();
        f.render_widget(Paragraph::new(lines).block(block), log_area);
    }

    fn draw_audio(&self, f: &mut Frame, area: Rect) {
        let block = Block::bordered().title(" Audio ");
        let Some(out) = &self.audio else {
            let err = self.audio_error.as_deref().unwrap_or("not open");
            f.render_widget(Paragraph::new(Line::from(err).red()).block(block), area);
            return;
        };
        let s = &self.stats;
        let frames = s.frames_per_callback.load(Relaxed);
        let ms = |us: u32| format!("{:.1} ms", us as f64 / 1000.0);
        let requested = out.requested_buffer.map_or("default".to_owned(), |b| b.to_string());
        let peak = s.peak();
        let db = if peak > 1e-5 { format!("{:>5.1} dB", 20.0 * peak.log10()) } else { "  -∞ dB".into() };
        let meter_len = ((peak.clamp(0.0, 1.0)) * 20.0).round() as usize;
        let meter_color = if peak > 0.9 { Color::Red } else { Color::Green };
        let errors = s.errors.load(Relaxed);

        let lines = vec![
            Line::from(vec![
                "Output ".dim(),
                Span::raw(out.device_name.clone()).bold(),
                format!("   {} · {} Hz · buffer ", audio::host_name(), out.sample_rate).dim(),
                Span::raw(format!("{requested} asked, {frames} got")),
                format!(" ({:.1} ms)", frames as f64 * 1000.0 / out.sample_rate as f64).dim(),
            ]),
            Line::from(vec![
                "Latency ".dim(),
                "queue ".dim(),
                Span::raw(ms(s.queue_us.load(Relaxed))),
                " · output ".dim(),
                Span::raw(ms(s.output_us.load(Relaxed))),
                " · last note ".dim(),
                Span::raw(ms(s.note_latency_us.load(Relaxed))).bold(),
                " · worst ".dim(),
                Span::raw(ms(s.note_latency_max_us.load(Relaxed))),
            ]),
            Line::from(vec![
                "Voices ".dim(),
                Span::raw(format!("{:>2}", s.active_voices.load(Relaxed))),
                "   Level ".dim(),
                Span::styled(format!("{:<20}", "█".repeat(meter_len)), Style::new().fg(meter_color)),
                Span::raw(format!(" {db}")),
                "   Errors ".dim(),
                if errors > 0 { Span::raw(errors.to_string()).red() } else { Span::raw("0") },
            ]),
        ];
        f.render_widget(Paragraph::new(lines).block(block), area);
    }
}

fn picker_list<'a>(items: Vec<ListItem<'a>>, block: Block<'a>) -> List<'a> {
    List::new(items)
        .block(block)
        .highlight_style(Style::new().fg(Color::Black).bg(Color::Cyan))
        .highlight_symbol("▶ ")
}

fn format_event(ev: &TimedEvent) -> Line<'static> {
    let t = format!("{:>10.3}s  ", ev.micros as f64 / 1e6);
    let (kind, detail, color) = match ev.event {
        MidiEvent::NoteOn { ch, note, vel } => {
            ("NOTE ON ", format!("ch{:<2} {:<4} vel {vel:>3} {}", ch + 1, note_name(note), vel_bar(vel)), Color::Green)
        }
        MidiEvent::NoteOff { ch, note, vel } => {
            ("NOTE OFF", format!("ch{:<2} {:<4} vel {vel:>3}", ch + 1, note_name(note)), Color::DarkGray)
        }
        MidiEvent::ControlChange { ch, cc, value } => {
            ("CC      ", format!("ch{:<2} #{cc:<3} = {value}", ch + 1), Color::Yellow)
        }
        MidiEvent::ProgramChange { ch, program } => {
            ("PROGRAM ", format!("ch{:<2} {program}", ch + 1), Color::Magenta)
        }
        MidiEvent::PitchBend { ch, value } => ("BEND    ", format!("ch{:<2} {value:+}", ch + 1), Color::Blue),
        MidiEvent::Other { status } => ("OTHER   ", format!("status 0x{status:02X}"), Color::DarkGray),
    };
    Line::from(vec![
        Span::raw(t).dim(),
        Span::styled(kind, Style::new().fg(color).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::raw(detail),
    ])
}

fn vel_bar(vel: u8) -> String {
    "█".repeat((vel as usize).div_ceil(8))
}
