//! Drawing: top bar, effects matrix, mini keyboard, master strip, bottom bar and popups.

use std::sync::atomic::Ordering::Relaxed;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, Padding, Paragraph};

use super::{App, BUFFER_SIZES, MATRIX_COLS, Popup, matrix_cols};
use crate::audio;
use crate::params::{MASTER_COUNT, Master, ParamId};

const ACCENT: Color = Color::Cyan;
const LIGHT_ON: Color = Color::LightGreen;
const LIGHT_OFF: Color = Color::DarkGray;
const CELL_MIN_WIDTH: u16 = 16;
const CELL_HEIGHT: u16 = 4;
const LAYER_COL_WIDTH: u16 = 26;
/// Master level column; the EQ bands share the rest of the strip.
const MASTER_COL_WIDTH: u16 = 18;
const MASTER_COL_GAP: u16 = 2;
/// Piano range shown by the mini keyboard (A0..C8).
const KEY_LO: u8 = 21;
const KEY_HI: u8 = 108;

impl App {
    pub(super) fn draw(&mut self, f: &mut Frame) {
        let [top, layers, main, keyboard, master, bottom] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(CELL_HEIGHT * self.visible_params().len().div_ceil(MATRIX_COLS) as u16 + 2),
            Constraint::Length(3),
            Constraint::Length(4),
            Constraint::Length(1),
        ])
        .areas(f.area());

        self.draw_top_bar(f, top);
        self.draw_layer_bar(f, layers);
        self.draw_matrix(f, main);
        self.draw_keyboard(f, keyboard);
        self.draw_master(f, master);
        self.draw_bottom_bar(f, bottom);
        if let Some(popup) = self.popup {
            self.draw_popup(f, popup, main);
        }
    }

    fn draw_top_bar(&self, f: &mut Frame, area: Rect) {
        let light = |on: bool| Span::styled("●", Style::new().fg(if on { LIGHT_ON } else { LIGHT_OFF }));
        let port = self.connection.as_ref().map_or("none".to_owned(), |c| short_port_name(&c.name));
        let output = self.audio.as_ref().map_or("none".to_owned(), |a| a.device_name.clone());
        let left = Line::from(vec![
            Span::raw(" "),
            light(self.held.iter().any(|&h| h)),
            "  Port ".dim(),
            Span::raw(port).bold(),
            "   Output ".dim(),
            Span::raw(output).bold(),
        ]);
        let mut left = left;
        if let Some(name) = &self.profile_name {
            left.extend(["   Profile ".dim(), Span::raw(name.clone()).bold()]);
        }
        let mut right = vec![];
        if let Some(rec) = &self.recording {
            let t = rec.elapsed().as_secs();
            // Blink the dot once per second.
            let dot = if rec.elapsed().subsec_millis() < 500 { "●" } else { " " };
            right.push(Span::styled(format!("{dot} REC {}:{:02}   ", t / 60, t % 60), Style::new().fg(Color::LightRed).bold()));
        }
        right.extend(["Sustain ".dim(), light(self.sustain), Span::raw(" ")]);
        let right = Line::from(right).alignment(Alignment::Right);
        let [l, r] = Layout::horizontal([Constraint::Min(0), Constraint::Length(26)]).areas(area);
        f.render_widget(left, l);
        f.render_widget(right, r);
    }

    /// One column per layer, scrolling sideways to keep the current one visible.
    fn draw_layer_bar(&mut self, f: &mut Frame, area: Rect) {
        let visible = ((area.width / LAYER_COL_WIDTH).max(1) as usize).min(self.layers.len());
        if self.current < self.layer_scroll {
            self.layer_scroll = self.current;
        } else if self.current >= self.layer_scroll + visible {
            self.layer_scroll = self.current + 1 - visible;
        }
        let scroll = self.layer_scroll.min(self.layers.len() - visible);
        self.layer_rects.clear();
        for (k, i) in (scroll..scroll + visible).enumerate() {
            let rect = Rect { x: area.x + k as u16 * LAYER_COL_WIDTH, width: LAYER_COL_WIDTH, ..area }.intersection(area);
            let l = &self.layers[i];
            let selected = i == self.current;
            let loading = self
                .loading
                .as_ref()
                .filter(|ld| l.source.as_ref() == Some(&super::Source::Sfz(ld.path.clone())));
            let (border, text) = match (selected, l.muted) {
                (_, true) => (Style::new().fg(Color::DarkGray), Style::new().fg(Color::DarkGray).italic()),
                (true, false) => (Style::new().fg(ACCENT).bold(), Style::new().fg(ACCENT).bold()),
                (false, false) => (Style::new().fg(Color::Gray), Style::new()),
            };
            let mut title = format!(" {} ", i + 1);
            if l.muted {
                title.push_str("muted ");
            }
            let block = Block::bordered()
                .border_type(if selected { BorderType::Thick } else { BorderType::Rounded })
                .border_style(border)
                .title(title);
            let label = match loading {
                Some(ld) => {
                    let (done, total) = (ld.progress.done.load(Relaxed), ld.progress.total.load(Relaxed).max(1));
                    format!("{} {}%", ld.name, done * 100 / total)
                }
                None => l.instrument.name.clone(),
            };
            let width = rect.width.saturating_sub(4) as usize;
            let label: String = if label.chars().count() > width {
                label.chars().take(width.saturating_sub(1)).chain(['…']).collect()
            } else {
                label
            };
            f.render_widget(Paragraph::new(Line::styled(label, text).centered()).block(block), rect);
            self.layer_rects.push((i, rect));
        }
        // Room left: scroll hints and a reminder of the layer keys.
        let used = visible as u16 * LAYER_COL_WIDTH;
        if area.width > used + 2 {
            let rest = Rect { x: area.x + used + 1, width: area.width - used - 1, y: area.y + 1, height: 1 };
            let mut hint = String::new();
            if scroll > 0 {
                hint.push_str("◀ ");
            }
            if scroll + visible < self.layers.len() {
                hint.push_str("▶ ");
            }
            hint.push_str("Alt+N new  Alt+D duplicate  Alt+←→ select");
            f.render_widget(Line::from(hint).dark_gray(), rest);
        }
    }

    fn draw_matrix(&mut self, f: &mut Frame, area: Rect) {
        let title = format!(" {} ", self.layer().instrument.name);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(title.bold())
            .title_bottom(Line::from(format!(" {} ", self.status)).dark_gray());
        let inner = block.inner(area);

        let params = self.visible_params();
        let selected = self.selected_index(&params);
        let cols = matrix_cols(params.len());
        let visible = ((inner.width / CELL_MIN_WIDTH).max(1) as usize).min(cols);
        let sel_col = selected % cols;
        if sel_col < self.matrix_scroll {
            self.matrix_scroll = sel_col;
        } else if sel_col >= self.matrix_scroll + visible {
            self.matrix_scroll = sel_col + 1 - visible;
        }
        let scroll = self.matrix_scroll.min(cols - visible);
        let mut hints = Vec::new();
        if scroll > 0 {
            hints.push("◀");
        }
        if scroll + visible < cols {
            hints.push("▶");
        }
        let block = if hints.is_empty() {
            block
        } else {
            block.title(Line::from(format!(" {} ", hints.join(" "))).right_aligned().fg(ACCENT))
        };
        f.render_widget(block, area);

        let rows = params.len().div_ceil(cols) as u16;
        let grid_h = (rows * CELL_HEIGHT).min(inner.height);
        let [grid] = Layout::vertical([Constraint::Length(grid_h)]).flex(Flex::Center).areas(inner);
        // Fill splits the leftover columns evenly instead of leaving a gap on the right.
        let columns = Layout::horizontal(vec![Constraint::Fill(1); visible]).split(grid);

        self.cell_rects.clear();
        for (i, &id) in params.iter().enumerate() {
            let (row, col) = ((i / cols) as u16, i % cols);
            if col < scroll || col >= scroll + visible {
                continue;
            }
            let column = columns[col - scroll];
            let rect = Rect { y: grid.y + row * CELL_HEIGHT, height: CELL_HEIGHT, ..column }.intersection(grid);
            if rect.height < 3 {
                continue;
            }
            self.cell_rects.push((id, rect));
            self.draw_cell(f, rect, id, i == selected);
        }
    }

    fn draw_cell(&self, f: &mut Frame, area: Rect, id: ParamId, selected: bool) {
        let spec = id.spec();
        let params = &self.layer().params;
        let value = params.get(id);
        let changed = !params.is_default(id);
        let border = if selected {
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::DarkGray)
        };
        let name = if selected { spec.name.bold().fg(ACCENT) } else { Span::raw(spec.name) };
        let block = Block::bordered()
            .border_type(if selected { BorderType::Thick } else { BorderType::Rounded })
            .border_style(border)
            .title(Line::from(vec![Span::raw(" "), name, Span::raw(" ")]));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let gauge_w = inner.width.saturating_sub(2) as usize;
        let value_style = if changed { Style::new().fg(Color::Yellow).bold() } else { Style::new().bold() };
        let lines = vec![
            Line::styled(spec.format(value), value_style).centered(),
            gauge(gauge_w, spec.to_norm(spec.default), spec.to_norm(value), selected).centered(),
        ];
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_keyboard(&self, f: &mut Frame, area: Rect) {
        let [_, area] = Layout::vertical([Constraint::Length(1), Constraint::Length(2)]).areas(area);
        let keys = (KEY_HI - KEY_LO + 1) as u16;
        let width = keys.min(area.width);
        // Narrow terminals: keep the middle of the keyboard.
        let first = KEY_LO + ((keys - width) / 2) as u8;
        let mut top = Vec::with_capacity(width as usize);
        let mut bottom = Vec::with_capacity(width as usize);
        for note in first..first + width as u8 {
            let black = matches!(note % 12, 1 | 3 | 6 | 8 | 10);
            let held = self.held[note as usize];
            let color = match (held, black) {
                (true, _) => ACCENT,
                (false, true) => Color::Black,
                (false, false) => Color::Gray,
            };
            top.push(Span::styled("█", Style::new().fg(color)));
            // Lower row only shows white keys, so the gaps read as black keys above.
            bottom.push(if black {
                Span::raw(" ")
            } else {
                Span::styled("▀", Style::new().fg(if held { ACCENT } else { Color::Gray }))
            });
        }
        let [kb] = Layout::horizontal([Constraint::Length(width)]).flex(Flex::Center).areas(area);
        f.render_widget(Paragraph::new(vec![Line::from(top), Line::from(bottom)]), kb);
    }

    /// Thick-bordered strip: per control a "label value" row and a gauge, keys in the border.
    fn draw_master(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Thick)
            .border_style(Style::new().fg(Color::Gray))
            .padding(Padding::horizontal(1))
            .title(" Master bus ".bold())
            .title_bottom(Line::from(" Ctrl+←→ select  Ctrl+↑↓/scroll adjust  Ctrl+Space reset ").dark_gray().right_aligned());
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [labels, gauges] = Layout::vertical([Constraint::Length(1); 2]).areas(inner);

        let mut widths = vec![Constraint::Length(MASTER_COL_WIDTH)];
        widths.extend([Constraint::Fill(1); MASTER_COUNT - 1]);
        let columns = Layout::horizontal(widths).spacing(MASTER_COL_GAP).split(labels);
        // Divider between the level and the EQ bands.
        if let Some(&first) = columns.first()
            && inner.width > first.width + MASTER_COL_GAP
        {
            let x = first.right() + MASTER_COL_GAP / 2;
            for y in [labels.y, gauges.y] {
                f.render_widget(Line::from("│").dark_gray(), Rect { x, y, width: 1, height: 1 });
            }
        }
        self.master_rects.clear();
        for (i, &col) in columns.iter().enumerate() {
            let spec = Master::spec(i);
            let value = self.master.get(i);
            let selected = i == self.master_selected;
            let label_style = if selected {
                Style::new().fg(Color::Black).bg(ACCENT).bold()
            } else {
                Style::new().fg(Color::Gray)
            };
            let value_style = if !self.master.is_default(i) { Style::new().fg(Color::Yellow).bold() } else { Style::new() };
            let (label, shown) = (Master::label(i), spec.format(value));
            let line = Line::from(vec![
                Span::styled(format!(" {label} "), label_style),
                Span::raw(" "),
                Span::styled(shown.clone(), value_style),
            ]);
            // Too narrow for both: label on top, the value replaces the gauge.
            let fits = line.width() as u16 <= col.width;
            let top = if fits { line } else { Line::from(Span::styled(format!(" {label} "), label_style)) };
            let below = Rect { y: gauges.y, ..col };
            f.render_widget(top.centered(), col);
            let bottom = if fits {
                gauge(col.width.saturating_sub(2) as usize, spec.to_norm(spec.default), spec.to_norm(value), selected)
            } else {
                Line::styled(shown, value_style)
            };
            f.render_widget(bottom.centered(), below);
            self.master_rects.push((i, Rect { height: 2, ..col }));
        }
    }

    fn draw_bottom_bar(&self, f: &mut Frame, area: Rect) {
        let key = |k: &'static str, label: &'static str| {
            [Span::styled(format!(" {k} "), Style::new().fg(Color::Black).bg(ACCENT)), Span::raw(format!(" {label}  "))]
        };
        let record = if self.recording.is_some() { "Stop rec" } else { "Record" };
        let mut spans: Vec<Span> =
            [key("o", "Output"), key("p", "Port"), key("r", record), key("s", "Save"), key("l", "Load"), key("q", "Quit")]
            .into_iter()
            .flatten()
            .collect();
        spans.push(Span::raw("│ ").dark_gray());
        spans.extend(
            [key("Alt+⏎", "Instrument"), key("Alt+M", "Mute"), key("Alt+K", "Kill")].into_iter().flatten(),
        );
        f.render_widget(Line::from(spans), area);
    }

    fn draw_popup(&mut self, f: &mut Frame, popup: Popup, over: Rect) {
        if popup == Popup::Save {
            return self.draw_save_popup(f, over);
        }
        let extra = if popup == Popup::Output { 4 } else { 0 };
        let list_len = match popup {
            Popup::Port => self.ports.len(),
            Popup::Output => self.outputs.len(),
            Popup::Instrument => self.instruments.len(),
            Popup::Load => self.profiles.len(),
            Popup::Save => 0,
        } as u16;
        let width = (over.width * 4 / 5).max(40).min(over.width);
        let height = (list_len.max(1) + extra + 3).min(over.height);
        let [area] = Layout::horizontal([Constraint::Length(width)]).flex(Flex::Center).areas(over);
        let [area] = Layout::vertical([Constraint::Length(height)]).flex(Flex::Center).areas(area);
        f.render_widget(Clear, area);

        let (title, keys) = match popup {
            Popup::Port => (" MIDI input ", " ↑↓ select  ⏎ connect  r refresh  esc close "),
            Popup::Output => (" Audio output ", " ⏎ use  [ ] buffer  t test  l reset worst  esc close "),
            Popup::Instrument => (" Instrument for this layer ", " ↑↓ select  ⏎ load  r refresh  esc close "),
            Popup::Load => (" Load profile ", " ↑↓ select  ⏎ load (replaces every layer)  r refresh  esc close "),
            Popup::Save => unreachable!("drawn by draw_save_popup"),
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(ACCENT))
            .title(title.bold())
            .title_bottom(Line::from(keys).dim());
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [list_area, info_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(extra)]).areas(inner);

        let (items, state, empty): (Vec<ListItem>, _, _) = match popup {
            Popup::Port => {
                let current = self.connection.as_ref().map(|c| c.name.as_str());
                let items = self.ports.iter().map(|p| item(p.clone(), None, Some(p.as_str()) == current)).collect();
                (items, &mut self.port_list, "No MIDI input. Plug a keyboard or start fake_keyboard, then press r.")
            }
            Popup::Output => {
                let current = self.output.as_deref();
                let items = self
                    .outputs
                    .iter()
                    .map(|o| item(o.name.clone(), Some(o.id.clone()), Some(o.id.as_str()) == current))
                    .collect();
                (items, &mut self.output_list, "No audio output found. Press r to refresh.")
            }
            Popup::Instrument => {
                let current = self.layer().source.as_ref();
                let home = std::env::var("HOME").unwrap_or_default();
                let items = self
                    .instruments
                    .iter()
                    .map(|source| {
                        let (shown, detail) = match source {
                            super::Source::Synth(i) => (format!("Synth · {}", crate::synth::PRESETS[*i].name), "synth"),
                            super::Source::Sfz(p) => {
                                (p.to_string_lossy().replacen(&format!("{home}/Sounds/SFZ/"), "", 1), "~/Sounds/SFZ")
                            }
                        };
                        item(shown, Some(detail.to_owned()), Some(source) == current)
                    })
                    .collect();
                (items, &mut self.instrument_list, "No .sfz file found in ~/Sounds/SFZ.")
            }
            Popup::Load => {
                let current = self.profile_name.as_deref();
                let items = self.profiles.iter().map(|n| item(n.clone(), None, Some(n.as_str()) == current)).collect();
                (items, &mut self.profile_list, "No profile saved yet. Press s in the main screen to save one.")
            }
            Popup::Save => unreachable!("drawn by draw_save_popup"),
        };
        if list_len == 0 {
            f.render_widget(Paragraph::new(empty).dim(), list_area);
        } else {
            let list = List::new(items)
                .highlight_style(Style::new().fg(Color::Black).bg(ACCENT))
                .highlight_symbol("▶ ");
            f.render_stateful_widget(list, list_area, state);
        }
        if popup == Popup::Output {
            self.draw_audio_info(f, info_area);
        }
    }

    /// Name field plus where the file goes.
    fn draw_save_popup(&self, f: &mut Frame, over: Rect) {
        let width = (over.width * 3 / 5).max(40).min(over.width);
        let [area] = Layout::horizontal([Constraint::Length(width)]).flex(Flex::Center).areas(over);
        let [area] = Layout::vertical([Constraint::Length(5)]).flex(Flex::Center).areas(area);
        f.render_widget(Clear, area);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(ACCENT))
            .title(" Save profile ".bold())
            .title_bottom(Line::from(" type a name  ⏎ save  esc cancel ").dim());
        let inner = block.inner(area);
        f.render_widget(block, area);
        let name = crate::profile::sanitize_name(&self.save_name);
        let target = if name.is_empty() {
            Span::raw(" layers, instruments, settings and master").dim()
        } else if crate::profile::path(&name).exists() {
            Span::raw(format!(" replaces profiles/{name}.json")).yellow()
        } else {
            Span::raw(format!(" → profiles/{name}.json")).dim()
        };
        let lines = vec![
            Line::from(vec![" Name ".dim(), Span::raw(self.save_name.clone()).bold(), Span::styled("▏", Style::new().fg(ACCENT))]),
            Line::from(target),
        ];
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_audio_info(&self, f: &mut Frame, area: Rect) {
        let Some(out) = &self.audio else {
            let err = self.audio_error.as_deref().unwrap_or("not open");
            f.render_widget(Paragraph::new(Line::from(err).red()), area);
            return;
        };
        let s = &self.stats;
        let frames = s.frames_per_callback.load(Relaxed);
        let ms = |us: u32| format!("{:.1} ms", us as f64 / 1000.0);
        let requested = self.buffer.map_or("default".to_owned(), |i| BUFFER_SIZES[i].to_string());
        let errors = s.errors.load(Relaxed);
        let peak = s.peak();
        let level = if peak > 1e-5 { format!("{:.1} dB", 20.0 * peak.log10()) } else { "-∞ dB".into() };
        let lines = vec![
            Line::raw(""),
            Line::from(vec![
                format!("{} · {} Hz · buffer ", audio::host_name(), out.sample_rate).dim(),
                Span::raw(format!("{requested} asked, {frames} got")),
                format!(" ({:.1} ms)", frames as f64 * 1000.0 / out.sample_rate as f64).dim(),
            ]),
            Line::from(vec![
                "Latency ".dim(),
                "output ".dim(),
                Span::raw(ms(s.output_us.load(Relaxed))),
                " · last note ".dim(),
                Span::raw(ms(s.note_latency_us.load(Relaxed))).bold(),
                " · worst ".dim(),
                Span::raw(ms(s.note_latency_max_us.load(Relaxed))),
            ]),
            Line::from(vec![
                "Voices ".dim(),
                Span::raw(s.active_voices.load(Relaxed).to_string()),
                "   Level ".dim(),
                Span::raw(level),
                "   Errors ".dim(),
                if errors > 0 { Span::raw(errors.to_string()).red() } else { Span::raw("0") },
            ]),
        ];
        f.render_widget(Paragraph::new(lines), area);
    }
}

fn item(name: String, detail: Option<String>, current: bool) -> ListItem<'static> {
    let mut spans = vec![
        Span::styled(if current { "● " } else { "  " }, Style::new().fg(LIGHT_ON)),
        Span::raw(name),
    ];
    if let Some(d) = detail {
        spans.push(Span::raw(format!("  {d}")).dim());
    }
    ListItem::new(Line::from(spans))
}

/// `━━━━●───` style gauge, filled from the default position towards the value.
fn gauge(width: usize, default: f32, value: f32, selected: bool) -> Line<'static> {
    if width < 3 {
        return Line::raw("");
    }
    let pos = |n: f32| ((n * (width - 1) as f32).round() as usize).min(width - 1);
    let (d, v) = (pos(default), pos(value));
    let (lo, hi) = (d.min(v), d.max(v));
    let fill = if selected { ACCENT } else { Color::Gray };
    let spans = (0..width)
        .map(|i| {
            if i == v {
                Span::styled("●", Style::new().fg(if selected { ACCENT } else { Color::White }).bold())
            } else if i >= lo && i <= hi {
                Span::styled("━", Style::new().fg(fill))
            } else {
                Span::styled("─", Style::new().fg(Color::DarkGray))
            }
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

/// "Fake Keyboard:Fake Keyboard 129:0" → "Fake Keyboard".
fn short_port_name(name: &str) -> String {
    name.split(':').next().unwrap_or(name).to_owned()
}
