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
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::ListState;

use crate::audio::{self, AudioOut, OutputInfo};
use crate::engine::{EngineCmd, Garbage, Layer, LayerId, LayerSpec, MAX_LAYERS, Stats};
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
const MATRIX_COLS: usize = 6;
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
    /// Layer that receives the instrument (if it still exists when loading ends).
    layer: LayerId,
    name: String,
    progress: Arc<Progress>,
    handle: JoinHandle<Result<Instrument>>,
}

/// UI side of one layer; the engine holds the matching `engine::Layer`.
struct LayerUi {
    id: LayerId,
    instrument: Arc<Instrument>,
    source: Option<Source>,
    params: Params,
    /// What the engine has; `None` forces a resend.
    params_sent: Option<Params>,
    muted: bool,
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
    layers: Vec<LayerUi>,
    /// Index into `layers` of the layer shown in the matrix.
    current: usize,
    next_layer_id: LayerId,
    /// First visible column of the layer bar, and where each column was drawn.
    layer_scroll: usize,
    layer_rects: Vec<(usize, Rect)>,
    /// Instrument for Alt+N.
    piano: Option<PathBuf>,
    loading: Option<Loading>,
    /// Memory the audio thread is done with; dropped here, off the real-time thread.
    garbage_tx: Sender<Garbage>,
    garbage_rx: Receiver<Garbage>,
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
    /// `instrument` is loaded into the first layer; `piano` is what Alt+N adds.
    pub fn new(output: Option<String>, buffer: Option<u32>, instrument: Option<PathBuf>, piano: Option<PathBuf>) -> Self {
        let (ui_tx, ui_rx) = crossbeam_channel::bounded(4096);
        let (engine_tx, engine_rx) = crossbeam_channel::bounded(1024);
        let (garbage_tx, garbage_rx) = crossbeam_channel::bounded(16);
        // The server default is often 1024 frames (~43 ms on built-in speakers); ask for less.
        let buffer = Some(buffer.unwrap_or(DEFAULT_BUFFER)).map(|b| {
            BUFFER_SIZES.iter().position(|&s| s >= b).unwrap_or(BUFFER_SIZES.len() - 1)
        });
        // Until an SFZ loads (or if none is installed), play the plain sine synth.
        let first = LayerUi {
            id: 0,
            instrument: Arc::new(Instrument::synth(PRESETS[0].clone())),
            source: Some(Source::Synth(0)),
            params: Params::default(),
            params_sent: None,
            muted: false,
        };
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
            layers: vec![first],
            current: 0,
            next_layer_id: 1,
            layer_scroll: 0,
            layer_rects: Vec::new(),
            piano,
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
            app.load_instrument(path, 0);
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
            self.send_params();
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
                        Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
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

    // --- Layers -------------------------------------------------------------------------

    fn layer(&self) -> &LayerUi {
        &self.layers[self.current]
    }

    fn layer_mut(&mut self) -> &mut LayerUi {
        &mut self.layers[self.current]
    }

    fn layer_index(&self, id: LayerId) -> Option<usize> {
        self.layers.iter().position(|l| l.id == id)
    }

    fn send_params(&mut self) {
        for l in &mut self.layers {
            if l.params_sent != Some(l.params) && self.engine_tx.try_send(EngineCmd::Params(l.id, l.params)).is_ok() {
                l.params_sent = Some(l.params);
            }
        }
    }

    /// Adds a layer right after the current one and selects it.
    fn add_layer(&mut self, instrument: Arc<Instrument>, source: Option<Source>, params: Params) -> Option<LayerId> {
        if self.layers.len() >= MAX_LAYERS {
            self.status = format!("at most {MAX_LAYERS} layers");
            return None;
        }
        let id = self.next_layer_id;
        self.next_layer_id += 1;
        // Built here, with all its memory, so the audio thread only has to plug it in.
        let layer = Layer::new(id, self.sample_rate(), instrument.clone(), &params, false);
        let _ = self.engine_tx.send(EngineCmd::AddLayer(layer));
        self.current = (self.current + 1).min(self.layers.len());
        self.layers.insert(
            self.current,
            LayerUi { id, instrument, source, params, params_sent: Some(params), muted: false },
        );
        Some(id)
    }

    fn duplicate_layer(&mut self) {
        let l = self.layer();
        let (inst, source, params) = (l.instrument.clone(), l.source.clone(), l.params);
        if self.add_layer(inst, source, params).is_some() {
            self.status = format!("layer {} duplicated", self.current);
        }
    }

    fn new_piano_layer(&mut self) {
        let Some(path) = self.piano.clone() else {
            // No piano installed: a fresh sine synth layer instead.
            let inst = Arc::new(Instrument::synth(PRESETS[0].clone()));
            self.add_layer(inst, Some(Source::Synth(0)), Params::default());
            return;
        };
        if self.loading.is_some() && self.shared_instrument(&path).is_none() {
            self.status = "wait for the current instrument to finish loading".into();
            return;
        }
        let name = path.file_stem().map_or("piano".into(), |s| s.to_string_lossy().into_owned());
        let placeholder = Arc::new(Instrument::empty(name));
        if let Some(id) = self.add_layer(placeholder, Some(Source::Sfz(path.clone())), Params::default()) {
            self.load_instrument(path, id);
        }
    }

    fn toggle_mute(&mut self) {
        let l = self.layer_mut();
        l.muted = !l.muted;
        let (id, muted) = (l.id, l.muted);
        let _ = self.engine_tx.send(EngineCmd::Mute(id, muted));
        self.status = format!("layer {} {}", self.current + 1, if muted { "muted" } else { "unmuted" });
    }

    fn kill_layer(&mut self) {
        if self.layers.len() == 1 {
            self.status = "the last layer can't be removed".into();
            return;
        }
        let l = self.layers.remove(self.current);
        let _ = self.engine_tx.send(EngineCmd::RemoveLayer(l.id));
        self.status = format!("removed layer {} ({})", self.current + 1, l.instrument.name);
        self.current = self.current.min(self.layers.len() - 1);
    }

    /// An already loaded copy of this SFZ in another layer, to share instead of reloading.
    fn shared_instrument(&self, path: &Path) -> Option<Arc<Instrument>> {
        self.layers
            .iter()
            .find(|l| l.source.as_ref() == Some(&Source::Sfz(path.to_owned())) && !l.instrument.regions.is_empty())
            .map(|l| l.instrument.clone())
    }

    fn set_layer_instrument(&mut self, id: LayerId, instrument: Arc<Instrument>, source: Option<Source>) {
        let Some(i) = self.layer_index(id) else { return };
        let _ = self.engine_tx.send(EngineCmd::SetInstrument(id, instrument.clone()));
        self.layers[i].instrument = instrument;
        self.layers[i].source = source;
    }

    fn select_instrument(&mut self, source: Source) {
        let id = self.layer().id;
        match source {
            Source::Synth(i) => {
                let inst = Arc::new(Instrument::synth(PRESETS[i].clone()));
                // Presets like the basses come with their own filter/envelope settings.
                let suggested = PRESETS[i].params;
                for &(param, value) in suggested {
                    self.layer_mut().params.set(param, value);
                }
                self.status = if suggested.is_empty() {
                    format!("{} ready", inst.name)
                } else {
                    format!("{} ready, with its suggested settings", inst.name)
                };
                self.set_layer_instrument(id, inst, Some(source));
            }
            Source::Sfz(path) => self.load_instrument(path, id),
        }
    }

    /// Loads an SFZ into layer `id`, sharing it if another layer already has it.
    fn load_instrument(&mut self, path: PathBuf, id: LayerId) {
        if let Some(inst) = self.shared_instrument(&path) {
            self.status = format!("{} shared with another layer", inst.name);
            self.set_layer_instrument(id, inst, Some(Source::Sfz(path)));
            return;
        }
        if self.loading.is_some() {
            self.status = "wait for the current instrument to finish loading".into();
            return;
        }
        let progress = Arc::new(Progress::default());
        let name = path.file_stem().map_or("instrument".into(), |s| s.to_string_lossy().into_owned());
        let p = progress.clone();
        let load_path = path.clone();
        let handle = std::thread::spawn(move || instrument::load_sfz(&load_path, &p));
        self.status = format!("loading {name}…");
        if let Some(i) = self.layer_index(id) {
            self.layers[i].source = Some(Source::Sfz(path));
        }
        self.loading = Some(Loading { layer: id, name, progress, handle });
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
                let source = self.layer_index(loading.layer).and_then(|i| self.layers[i].source.clone());
                self.set_layer_instrument(loading.layer, Arc::new(inst), source);
            }
            Ok(Err(e)) => self.status = format!("cannot load {}: {e:#}", loading.name),
            Err(_) => self.status = format!("loading {} crashed", loading.name),
        }
    }

    // --- Audio, recording ---------------------------------------------------------------

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

    fn buffer_frames(&self) -> Option<u32> {
        self.buffer.map(|i| BUFFER_SIZES[i])
    }

    fn open_audio(&mut self) {
        // Drop the old stream first so the device is free.
        self.audio = None;
        while self.engine_rx.try_recv().is_ok() {}
        self.stats.reset_latency();
        let specs: Vec<LayerSpec> = self
            .layers
            .iter()
            .map(|l| LayerSpec { id: l.id, instrument: l.instrument.clone(), params: l.params, muted: l.muted })
            .collect();
        let result = audio::open(
            self.output.as_deref(),
            self.buffer_frames(),
            &specs,
            self.engine_rx.clone(),
            self.garbage_tx.clone(),
            self.stats.clone(),
        );
        match result {
            Ok(out) => {
                for l in &mut self.layers {
                    l.params_sent = Some(l.params);
                }
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

    // --- Popups -------------------------------------------------------------------------

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
            .layer()
            .source
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

    fn popup_list(&mut self) -> Option<&mut ListState> {
        match self.popup? {
            Popup::Port => Some(&mut self.port_list),
            Popup::Output => Some(&mut self.output_list),
            Popup::Instrument => Some(&mut self.instrument_list),
        }
    }

    // --- Input --------------------------------------------------------------------------

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

    fn on_key(&mut self, key: KeyEvent) {
        if let Some(popup) = self.popup {
            self.on_popup_key(popup, key.code);
            return;
        }
        if key.modifiers.contains(KeyModifiers::ALT) {
            match key.code {
                KeyCode::Enter => self.open_popup(Popup::Instrument),
                KeyCode::Char('d' | 'D') => self.duplicate_layer(),
                KeyCode::Char('n' | 'N') => self.new_piano_layer(),
                KeyCode::Char('m' | 'M') => self.toggle_mute(),
                KeyCode::Char('k' | 'K') => self.kill_layer(),
                KeyCode::Left => self.current = self.current.saturating_sub(1),
                KeyCode::Right => self.current = (self.current + 1).min(self.layers.len() - 1),
                _ => {}
            }
            return;
        }
        let selected = ParamId::ALL[self.selected];
        let count = ParamId::ALL.len();
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('o') => self.open_popup(Popup::Output),
            KeyCode::Char('p') => self.open_popup(Popup::Port),
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
                    self.layer_mut().params.reset(selected);
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
                    self.select_instrument(source);
                    self.popup = None;
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
                } else if let Some(&(i, _)) = self.layer_rects.iter().find(|(_, r)| r.contains(pos)) {
                    self.current = i;
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
        !id.synth_only() || self.layer().instrument.synth.is_some()
    }

    fn edit(&mut self, id: ParamId, steps: i32) {
        if self.is_enabled(id) {
            self.layer_mut().params.nudge(id, steps);
        } else {
            self.status = format!("{} only applies to synths (Alt+Enter to choose one)", id.spec().name);
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
