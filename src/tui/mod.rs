//! Terminal UI: state and input handling. Drawing lives in `view.rs`.

mod view;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};
use midir::MidiInputConnection;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::ListState;

use crate::audio::{self, AudioOut, OutputInfo};
use crate::engine::{EngineCmd, Stats};
use crate::instrument::{self, Instrument, Progress};
use crate::midi::{self, MidiEvent};
use crate::params::{ParamId, Params};
use crate::recorder::{RecMsg, Recording};
use crate::synth::PRESETS;

const SUSTAIN_CC: u8 = 64;
/// Buffer sizes offered with `[` / `]`; past the end means "server default".
const BUFFER_SIZES: [u32; 6] = [32, 64, 128, 256, 512, 1024];
const DEFAULT_BUFFER: u32 = 128;
const TEST_NOTE: u8 = 60;
const TEST_NOTE_LENGTH: Duration = Duration::from_millis(600);
/// Matrix shape: parameters are laid out row by row.
const MATRIX_COLS: usize = 5;
/// Steps per PgUp/PgDn.
const COARSE_STEPS: i32 = 10;

#[derive(Clone, Copy, PartialEq)]
enum Popup {
    Port,
    Output,
    Instrument,
}

struct Connection {
    name: String,
    _conn: MidiInputConnection<()>,
}

/// Something the Instrument window can load.
#[derive(Clone, PartialEq)]
enum Source {
    /// Index into `synth::PRESETS`.
    Synth(usize),
    Sfz(PathBuf),
}

struct Loading {
    name: String,
    progress: Arc<Progress>,
    handle: JoinHandle<Result<Instrument>>,
}

pub struct App {
    popup: Option<Popup>,
    ports: Vec<String>,
    port_list: ListState,
    connection: Option<Connection>,
    /// Incoming MIDI with its arrival time, for display and recording.
    ui_tx: Sender<(MidiEvent, Instant)>,
    ui_rx: Receiver<(MidiEvent, Instant)>,
    engine_tx: Sender<EngineCmd>,
    engine_rx: Receiver<EngineCmd>,
    stats: Arc<Stats>,
    instrument: Arc<Instrument>,
    instrument_source: Option<Source>,
    loading: Option<Loading>,
    /// Instruments the audio thread no longer uses; dropped here, off the real-time thread.
    garbage_tx: Sender<Arc<Instrument>>,
    garbage_rx: Receiver<Arc<Instrument>>,
    instruments: Vec<Source>,
    instrument_list: ListState,
    audio: Option<AudioOut>,
    audio_error: Option<String>,
    outputs: Vec<OutputInfo>,
    output_list: ListState,
    /// Output id chosen by the user; `None` follows the system default.
    output: Option<String>,
    /// Index into `BUFFER_SIZES`; `None` lets the server choose.
    buffer: Option<usize>,
    test_note_off: Option<Instant>,
    params: Params,
    /// What the engine has; `None` forces a resend (e.g. after reopening the stream).
    params_sent: Option<Params>,
    /// Index into `ParamId::ALL`.
    selected: usize,
    /// First visible matrix column when the terminal is too narrow for all of them.
    matrix_scroll: usize,
    /// Where each matrix cell was last drawn, for mouse clicks.
    cell_rects: Vec<(usize, Rect)>,
    recording: Option<Recording>,
    /// Kept for a moment after stopping so the engine never holds the last sender: the
    /// queue's memory must be freed here, not on the audio thread.
    rec_keepalive: Option<(Sender<RecMsg>, Instant)>,
    held: [bool; 128],
    sustain: bool,
    status: String,
    quit: bool,
}

impl App {
    pub fn new(output: Option<String>, buffer: Option<u32>, instrument: Option<PathBuf>) -> Self {
        let (ui_tx, ui_rx) = crossbeam_channel::bounded(4096);
        let (engine_tx, engine_rx) = crossbeam_channel::bounded(1024);
        let (garbage_tx, garbage_rx) = crossbeam_channel::bounded(8);
        // The server default is often 1024 frames (~43 ms on built-in speakers); ask for less.
        let buffer = Some(buffer.unwrap_or(DEFAULT_BUFFER)).map(|b| {
            BUFFER_SIZES.iter().position(|&s| s >= b).unwrap_or(BUFFER_SIZES.len() - 1)
        });
        let mut app = Self {
            popup: None,
            ports: Vec::new(),
            port_list: ListState::default(),
            connection: None,
            ui_tx,
            ui_rx,
            engine_tx,
            engine_rx,
            stats: Arc::new(Stats::default()),
            // Until an SFZ loads (or if none is installed), play the plain sine synth.
            instrument: Arc::new(Instrument::synth(PRESETS[0].clone())),
            instrument_source: Some(Source::Synth(0)),
            loading: None,
            garbage_tx,
            garbage_rx,
            instruments: Vec::new(),
            instrument_list: ListState::default(),
            audio: None,
            audio_error: None,
            outputs: Vec::new(),
            output_list: ListState::default(),
            output,
            buffer,
            test_note_off: None,
            params: Params::default(),
            params_sent: None,
            selected: 0,
            matrix_scroll: 0,
            cell_rects: Vec::new(),
            recording: None,
            rec_keepalive: None,
            held: [false; 128],
            sustain: false,
            status: String::new(),
            quit: false,
        };
        app.refresh_ports();
        app.open_audio();
        if let Some(path) = instrument {
            app.load_instrument(path);
        }
        app
    }

    /// Connects straight away, skipping the picker.
    pub fn connect(&mut self, query: &str) {
        let ui_tx = self.ui_tx.clone();
        let engine_tx = self.engine_tx.clone();
        let result = midi::connect(query, move |event| {
            let at = Instant::now();
            let _ = engine_tx.try_send(EngineCmd::Midi { event, at });
            let _ = ui_tx.try_send((event, at));
        });
        match result {
            Ok((name, conn)) => {
                self.status = format!("connected to {name}");
                self.connection = Some(Connection { name, _conn: conn });
                while self.ui_rx.try_recv().is_ok() {}
                self.held = [false; 128];
                self.sustain = false;
            }
            Err(e) => self.status = e.to_string(),
        }
    }

    /// Opens the port picker when nothing is connected yet.
    pub fn ask_for_port_if_needed(&mut self) {
        if self.connection.is_none() {
            self.open_popup(Popup::Port);
        }
    }

    pub fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            self.drain_events();
            self.poll_loading();
            if self.params_sent != Some(self.params)
                && self.engine_tx.try_send(EngineCmd::Params(self.params)).is_ok()
            {
                self.params_sent = Some(self.params);
            }
            while self.garbage_rx.try_recv().is_ok() {}
            if self.rec_keepalive.as_ref().is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(1)) {
                self.rec_keepalive = None;
            }
            if self.test_note_off.is_some_and(|t| Instant::now() >= t) {
                self.test_note_off = None;
                self.send_engine(MidiEvent::NoteOff { ch: 0, note: TEST_NOTE, vel: 0 });
            }
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(16))? {
                // Touchpads send scroll bursts: handle everything queued before redrawing.
                loop {
                    match event::read()? {
                        Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key.code),
                        Event::Mouse(m) => self.on_mouse(m),
                        _ => {}
                    }
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
        }
        if self.recording.is_some() {
            self.toggle_recording();
        }
        Ok(())
    }

    fn sample_rate(&self) -> u32 {
        self.audio.as_ref().map_or(48_000, |a| a.sample_rate)
    }

    fn toggle_recording(&mut self) {
        match self.recording.take() {
            None => {
                let Some(home) = std::env::var_os("HOME") else { return };
                match Recording::start(&Path::new(&home).join("Music/OFFENBACK"), self.sample_rate()) {
                    Ok(rec) => {
                        self.stats.rec_dropped.store(0, std::sync::atomic::Ordering::Relaxed);
                        let _ = self.engine_tx.try_send(EngineCmd::Record(Some(rec.sender())));
                        self.status = format!("recording to {}", rec.wav_path.display());
                        self.recording = Some(rec);
                    }
                    Err(e) => self.status = format!("cannot record: {e:#}"),
                }
            }
            Some(rec) => {
                // Tell the engine first; anything it still sends after Stop is ignored.
                let _ = self.engine_tx.send(EngineCmd::Record(None));
                self.rec_keepalive = Some((rec.sender(), Instant::now()));
                let dropped = self.stats.rec_dropped.load(std::sync::atomic::Ordering::Relaxed);
                self.status = match rec.stop(self.sample_rate()) {
                    Ok(saved) => {
                        let warn = if dropped > 0 { format!(" ({dropped} blocks lost: disk too slow)") } else { String::new() };
                        format!(
                            "saved {} ({:.1} s, {} notes) + .mid{warn}",
                            saved.wav_path.display(),
                            saved.seconds,
                            saved.notes
                        )
                    }
                    Err(e) => format!("recording failed: {e:#}"),
                };
            }
        }
    }

    fn select_instrument(&mut self, source: Source) {
        match source {
            Source::Synth(i) => {
                self.loading = None;
                self.instrument = Arc::new(Instrument::synth(PRESETS[i].clone()));
                let _ = self.engine_tx.try_send(EngineCmd::SetInstrument(self.instrument.clone()));
                self.status = format!("{} ready", self.instrument.name);
                self.instrument_source = Some(source);
            }
            Source::Sfz(path) => self.load_instrument(path),
        }
    }

    fn load_instrument(&mut self, path: PathBuf) {
        let progress = Arc::new(Progress::default());
        let name = path.file_stem().map_or("instrument".into(), |s| s.to_string_lossy().into_owned());
        let p = progress.clone();
        let load_path = path.clone();
        let handle = std::thread::spawn(move || instrument::load_sfz(&load_path, &p));
        self.status = format!("loading {name}…");
        self.instrument_source = Some(Source::Sfz(path));
        self.loading = Some(Loading { name, progress, handle });
    }

    fn poll_loading(&mut self) {
        if !self.loading.as_ref().is_some_and(|l| l.handle.is_finished()) {
            return;
        }
        let Some(loading) = self.loading.take() else { return };
        match loading.handle.join() {
            Ok(Ok(inst)) => {
                self.status = format!(
                    "{} loaded: {} regions, {} MB",
                    inst.name,
                    inst.regions.len(),
                    inst.memory_bytes() >> 20
                );
                self.instrument = Arc::new(inst);
                let _ = self.engine_tx.try_send(EngineCmd::SetInstrument(self.instrument.clone()));
            }
            Ok(Err(e)) => self.status = format!("cannot load {}: {e:#}", loading.name),
            Err(_) => self.status = format!("loading {} crashed", loading.name),
        }
    }

    fn buffer_frames(&self) -> Option<u32> {
        self.buffer.map(|i| BUFFER_SIZES[i])
    }

    fn open_audio(&mut self) {
        // Drop the old stream first so the device is free.
        self.audio = None;
        while self.engine_rx.try_recv().is_ok() {}
        self.stats.reset_latency();
        let result = audio::open(
            self.output.as_deref(),
            self.buffer_frames(),
            self.instrument.clone(),
            self.engine_rx.clone(),
            self.garbage_tx.clone(),
            self.stats.clone(),
        );
        match result {
            Ok(out) => {
                self.params_sent = None;
                // A new engine starts without the recorder.
                if let Some(rec) = &self.recording {
                    let _ = self.engine_tx.try_send(EngineCmd::Record(Some(rec.sender())));
                }
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
            Ok(ports) => self.ports = ports,
            Err(e) => {
                self.status = e.to_string();
                self.ports.clear();
            }
        }
        let current = self
            .connection
            .as_ref()
            .and_then(|c| self.ports.iter().position(|p| p == &c.name));
        self.port_list.select(current.or((!self.ports.is_empty()).then_some(0)));
    }

    fn refresh_outputs(&mut self) {
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
    }

    fn refresh_instruments(&mut self) {
        self.instruments = (0..PRESETS.len())
            .map(Source::Synth)
            .chain(find_sfz_files().into_iter().map(Source::Sfz))
            .collect();
        let current = self
            .instrument_source
            .as_ref()
            .and_then(|c| self.instruments.iter().position(|i| i == c));
        self.instrument_list.select(current.or((!self.instruments.is_empty()).then_some(0)));
    }

    fn open_popup(&mut self, popup: Popup) {
        match popup {
            Popup::Port => self.refresh_ports(),
            Popup::Output => self.refresh_outputs(),
            Popup::Instrument => self.refresh_instruments(),
        }
        self.popup = Some(popup);
    }

    fn drain_events(&mut self) {
        for (event, at) in self.ui_rx.try_iter() {
            if let Some(rec) = self.recording.as_mut() {
                rec.midi(event, at);
            }
            match event {
                MidiEvent::NoteOn { note, .. } => self.held[note as usize] = true,
                MidiEvent::NoteOff { note, .. } => self.held[note as usize] = false,
                MidiEvent::ControlChange { cc: SUSTAIN_CC, value, .. } => self.sustain = value >= 64,
                _ => {}
            }
        }
    }

    fn popup_list(&mut self) -> Option<&mut ListState> {
        match self.popup? {
            Popup::Port => Some(&mut self.port_list),
            Popup::Output => Some(&mut self.output_list),
            Popup::Instrument => Some(&mut self.instrument_list),
        }
    }

    fn on_key(&mut self, code: KeyCode) {
        if let Some(popup) = self.popup {
            self.on_popup_key(popup, code);
            return;
        }
        let selected = ParamId::ALL[self.selected];
        let count = ParamId::ALL.len();
        match code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('o') => self.open_popup(Popup::Output),
            KeyCode::Char('p') => self.open_popup(Popup::Port),
            KeyCode::Char('i') => self.open_popup(Popup::Instrument),
            KeyCode::Char('r' | 'R') => self.toggle_recording(),
            KeyCode::Left => self.selected = self.selected.saturating_sub(1),
            KeyCode::Right => self.selected = (self.selected + 1).min(count - 1),
            KeyCode::Up => self.selected = self.selected.saturating_sub(MATRIX_COLS),
            // The last row may be shorter: land on its last cell rather than not moving.
            KeyCode::Down if self.selected / MATRIX_COLS < (count - 1) / MATRIX_COLS => {
                self.selected = (self.selected + MATRIX_COLS).min(count - 1)
            }
            KeyCode::Char('+' | '=') => self.edit(selected, 1),
            KeyCode::Char('-') => self.edit(selected, -1),
            KeyCode::PageUp => self.edit(selected, COARSE_STEPS),
            KeyCode::PageDown => self.edit(selected, -COARSE_STEPS),
            KeyCode::Char(' ') | KeyCode::Backspace | KeyCode::Delete => {
                if self.is_enabled(selected) {
                    self.params.reset(selected);
                }
            }
            _ => {}
        }
    }

    fn on_popup_key(&mut self, popup: Popup, code: KeyCode) {
        match code {
            KeyCode::Esc | KeyCode::Char('q') => self.popup = None,
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(l) = self.popup_list() {
                    l.select_previous();
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(l) = self.popup_list() {
                    l.select_next();
                }
            }
            KeyCode::Char('r') => self.open_popup(popup),
            KeyCode::Enter => self.choose(popup),
            // Output window extras.
            KeyCode::Char('t') if popup == Popup::Output => {
                self.send_engine(MidiEvent::NoteOn { ch: 0, note: TEST_NOTE, vel: 100 });
                self.test_note_off = Some(Instant::now() + TEST_NOTE_LENGTH);
            }
            KeyCode::Char('[') if popup == Popup::Output => {
                self.buffer = Some(self.buffer.map_or(BUFFER_SIZES.len() - 1, |i| i.saturating_sub(1)));
                self.open_audio();
            }
            KeyCode::Char(']') if popup == Popup::Output => {
                self.buffer = self.buffer.and_then(|i| (i + 1 < BUFFER_SIZES.len()).then_some(i + 1));
                self.open_audio();
            }
            KeyCode::Char('l') if popup == Popup::Output => self.stats.reset_latency(),
            _ => {}
        }
    }

    fn choose(&mut self, popup: Popup) {
        match popup {
            Popup::Port => {
                if let Some(name) = self.port_list.selected().and_then(|i| self.ports.get(i)).cloned() {
                    self.connection = None;
                    self.connect(&name);
                    self.popup = None;
                }
            }
            Popup::Output => {
                if let Some(id) = self.output_list.selected().and_then(|i| self.outputs.get(i)).map(|o| o.id.clone()) {
                    self.output = Some(id);
                    self.open_audio();
                }
            }
            Popup::Instrument => {
                if let Some(source) = self.instrument_list.selected().and_then(|i| self.instruments.get(i)).cloned() {
                    if self.loading.is_none() {
                        self.select_instrument(source);
                        self.popup = None;
                    }
                }
            }
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        let up = match m.kind {
            MouseEventKind::ScrollUp => true,
            MouseEventKind::ScrollDown => false,
            MouseEventKind::Down(MouseButton::Left) if self.popup.is_none() => {
                let pos = Position::new(m.column, m.row);
                if let Some(&(i, _)) = self.cell_rects.iter().find(|(_, r)| r.contains(pos)) {
                    self.selected = i;
                }
                return;
            }
            _ => return,
        };
        if let Some(list) = self.popup_list() {
            if up { list.select_previous() } else { list.select_next() }
        } else {
            // Reversed on request: matches natural (touchpad) scrolling.
            self.edit(ParamId::ALL[self.selected], if up { -1 } else { 1 });
        }
    }

    /// Envelope and LFO cells only do something on synths.
    fn is_enabled(&self, id: ParamId) -> bool {
        !id.synth_only() || self.instrument.synth.is_some()
    }

    fn edit(&mut self, id: ParamId, steps: i32) {
        if self.is_enabled(id) {
            self.params.nudge(id, steps);
        } else {
            self.status = format!("{} only applies to synths (choose one with i)", id.spec().name);
        }
    }
}

/// All `.sfz` files under `~/Sounds/SFZ`, a few levels deep.
fn find_sfz_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let path = e.path();
            let name = e.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            if path.is_dir() && depth > 0 {
                walk(&path, depth - 1, out);
            } else if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("sfz")) {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        walk(&Path::new(&home).join("Sounds/SFZ"), 3, &mut out);
    }
    out.sort();
    out
}
