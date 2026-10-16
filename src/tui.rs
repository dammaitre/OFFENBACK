//! Terminal UI: MIDI port picker and live event monitor.

use std::collections::VecDeque;
use std::time::Duration;

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};
use midir::MidiInputConnection;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::midi::{self, MidiEvent, TimedEvent, note_name};

const LOG_CAPACITY: usize = 500;
const SUSTAIN_CC: u8 = 64;

enum Screen {
    PortPicker,
    Monitor,
}

struct Connection {
    name: String,
    _conn: MidiInputConnection<()>,
}

pub struct App {
    screen: Screen,
    ports: Vec<String>,
    port_list: ListState,
    connection: Option<Connection>,
    tx: Sender<TimedEvent>,
    rx: Receiver<TimedEvent>,
    log: VecDeque<TimedEvent>,
    held: [bool; 128],
    sustain: bool,
    event_count: u64,
    status: String,
    quit: bool,
}

impl App {
    pub fn new() -> Self {
        let (tx, rx) = crossbeam_channel::bounded(4096);
        let mut app = Self {
            screen: Screen::PortPicker,
            ports: Vec::new(),
            port_list: ListState::default(),
            connection: None,
            tx,
            rx,
            log: VecDeque::with_capacity(LOG_CAPACITY),
            held: [false; 128],
            sustain: false,
            event_count: 0,
            status: String::new(),
            quit: false,
        };
        app.refresh_ports();
        app
    }

    /// Connects straight away, skipping the picker.
    pub fn connect(&mut self, query: &str) {
        match midi::connect(query, self.tx.clone()) {
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
        self.port_list
            .select((!self.ports.is_empty()).then_some(0));
    }

    fn reset_state(&mut self) {
        while self.rx.try_recv().is_ok() {}
        self.log.clear();
        self.held = [false; 128];
        self.sustain = false;
        self.event_count = 0;
    }

    fn drain_events(&mut self) {
        for ev in self.rx.try_iter() {
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
        match self.screen {
            Screen::PortPicker => match code {
                KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
                KeyCode::Up | KeyCode::Char('k') => self.port_list.select_previous(),
                KeyCode::Down | KeyCode::Char('j') => self.port_list.select_next(),
                KeyCode::Char('r') => self.refresh_ports(),
                KeyCode::Enter => {
                    if let Some(name) = self.port_list.selected().and_then(|i| self.ports.get(i)) {
                        let name = name.clone();
                        self.connect(&name);
                    }
                }
                _ => {}
            },
            Screen::Monitor => match code {
                KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
                KeyCode::Char('p') => {
                    self.connection = None;
                    self.screen = Screen::PortPicker;
                    self.refresh_ports();
                }
                KeyCode::Char('c') => self.log.clear(),
                _ => {}
            },
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let [title, body, footer] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
                .areas(f.area());

        f.render_widget(Line::from(" OFFENBACK ".bold().reversed()), title);
        match self.screen {
            Screen::PortPicker => self.draw_picker(f, body),
            Screen::Monitor => self.draw_monitor(f, body),
        }
        let keys = match self.screen {
            Screen::PortPicker => "↑↓ select  ⏎ connect  r refresh  q quit",
            Screen::Monitor => "p change port  c clear log  q quit",
        };
        f.render_widget(
            Line::from(vec![Span::raw(format!(" {keys}  │ ")), Span::raw(&self.status).dim()]),
            footer,
        );
    }

    fn draw_picker(&mut self, f: &mut Frame, area: ratatui::layout::Rect) {
        let block = Block::bordered().title(" MIDI input ");
        if self.ports.is_empty() {
            let msg = Paragraph::new("No MIDI input found. Plug a keyboard or start fake_keyboard, then press r.")
                .block(block);
            f.render_widget(msg, area);
            return;
        }
        let items: Vec<ListItem> = self.ports.iter().map(|p| ListItem::new(p.as_str())).collect();
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::new().fg(Color::Black).bg(Color::Cyan))
            .highlight_symbol("▶ ");
        f.render_stateful_widget(list, area, &mut self.port_list);
    }

    fn draw_monitor(&mut self, f: &mut Frame, area: ratatui::layout::Rect) {
        let [info, log_area] =
            Layout::vertical([Constraint::Length(4), Constraint::Min(0)]).areas(area);

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
            Line::from(vec![
                format!("Held ({}) ", held.len()).dim(),
                Span::raw(held.join(" ")).cyan(),
            ]),
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
