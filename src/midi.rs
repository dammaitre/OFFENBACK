//! MIDI input: port discovery, connection, and decoding raw bytes into events.

use anyhow::{Context, Result, anyhow};
use midir::{Ignore, MidiInput, MidiInputConnection};

const CLIENT_NAME: &str = "OFFENBACK";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MidiEvent {
    NoteOn { ch: u8, note: u8, vel: u8 },
    NoteOff { ch: u8, note: u8, vel: u8 },
    ControlChange { ch: u8, cc: u8, value: u8 },
    ProgramChange { ch: u8, program: u8 },
    /// Centered on 0, range -8192..=8191.
    PitchBend { ch: u8, value: i16 },
    Other { status: u8 },
}

impl MidiEvent {
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let &status = bytes.first()?;
        let ch = status & 0x0F;
        let d1 = bytes.get(1).copied().unwrap_or(0) & 0x7F;
        let d2 = bytes.get(2).copied().unwrap_or(0) & 0x7F;
        Some(match status & 0xF0 {
            // Note-on with velocity 0 is a note-off by convention (running status keyboards).
            0x90 if d2 == 0 => Self::NoteOff { ch, note: d1, vel: 0 },
            0x90 => Self::NoteOn { ch, note: d1, vel: d2 },
            0x80 => Self::NoteOff { ch, note: d1, vel: d2 },
            0xB0 => Self::ControlChange { ch, cc: d1, value: d2 },
            0xC0 => Self::ProgramChange { ch, program: d1 },
            0xE0 => Self::PitchBend { ch, value: ((d2 as i16) << 7 | d1 as i16) - 8192 },
            _ => Self::Other { status },
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TimedEvent {
    /// Microseconds since the connection was opened (as reported by the backend).
    pub micros: u64,
    pub event: MidiEvent,
}

pub fn list_ports() -> Result<Vec<String>> {
    let input = MidiInput::new(CLIENT_NAME).context("cannot open the ALSA sequencer")?;
    Ok(input
        .ports()
        .iter()
        .filter_map(|p| input.port_name(p).ok())
        .collect())
}

/// Connects to the first port whose name contains `query` (case-insensitive).
/// `on_event` runs on midir's callback thread for every decoded event, so keep it short.
pub fn connect<F>(query: &str, mut on_event: F) -> Result<(String, MidiInputConnection<()>)>
where
    F: FnMut(TimedEvent) + Send + 'static,
{
    let mut input = MidiInput::new(CLIENT_NAME).context("cannot open the ALSA sequencer")?;
    // Drop sysex, clock and active sensing: we only care about performance data.
    input.ignore(Ignore::All);

    let needle = query.to_lowercase();
    let port = input
        .ports()
        .into_iter()
        .find(|p| {
            input
                .port_name(p)
                .is_ok_and(|n| n.to_lowercase().contains(&needle))
        })
        .ok_or_else(|| anyhow!("no MIDI input port matching \"{query}\""))?;
    let name = input.port_name(&port)?;

    let conn = input
        .connect(
            &port,
            "offenback-in",
            move |micros, bytes, _| {
                if let Some(event) = MidiEvent::parse(bytes) {
                    on_event(TimedEvent { micros, event });
                }
            },
            (),
        )
        .map_err(|e| anyhow!("cannot connect to \"{name}\": {e}"))?;
    Ok((name, conn))
}

pub fn note_name(note: u8) -> String {
    const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    format!("{}{}", NAMES[note as usize % 12], note as i32 / 12 - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_channel_messages() {
        assert_eq!(MidiEvent::parse(&[0x91, 60, 100]), Some(MidiEvent::NoteOn { ch: 1, note: 60, vel: 100 }));
        assert_eq!(MidiEvent::parse(&[0x90, 60, 0]), Some(MidiEvent::NoteOff { ch: 0, note: 60, vel: 0 }));
        assert_eq!(MidiEvent::parse(&[0x80, 60, 40]), Some(MidiEvent::NoteOff { ch: 0, note: 60, vel: 40 }));
        assert_eq!(MidiEvent::parse(&[0xB0, 64, 127]), Some(MidiEvent::ControlChange { ch: 0, cc: 64, value: 127 }));
        assert_eq!(MidiEvent::parse(&[0xE0, 0, 64]), Some(MidiEvent::PitchBend { ch: 0, value: 0 }));
        assert_eq!(MidiEvent::parse(&[0xE0, 0, 0]), Some(MidiEvent::PitchBend { ch: 0, value: -8192 }));
        assert_eq!(MidiEvent::parse(&[]), None);
    }

    #[test]
    fn names_notes() {
        assert_eq!(note_name(60), "C4");
        assert_eq!(note_name(21), "A0");
        assert_eq!(note_name(70), "A#4");
    }
}
