//! Audio output: device discovery and the cpal stream that drives the engine.
//!
//! With the `pipewire` feature cpal talks to PipeWire natively: devices are PipeWire sinks
//! (speakers, Bluetooth headsets...) and a fixed buffer size becomes the node latency.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, DeviceDirection, FromSample, SampleFormat, SizedSample, StreamConfig};
use crossbeam_channel::{Receiver, Sender};

use crate::engine::{Engine, EngineCmd, Stats};
use crate::instrument::Instrument;

/// Frames rendered per engine call; the callback loops when the device asks for more.
const BLOCK: usize = 256;
const SAMPLE_RATE: u32 = 48_000;

pub struct OutputInfo {
    pub id: String,
    pub name: String,
}

pub fn host_name() -> String {
    cpal::default_host().id().name().to_owned()
}

pub fn list_outputs() -> Result<Vec<OutputInfo>> {
    let host = cpal::default_host();
    let pipewire = host.id().name() == "PipeWire";
    Ok(host
        .output_devices()
        .context("cannot enumerate audio outputs")?
        .filter_map(|d| {
            let id = d.id().ok()?.id().to_owned();
            let desc = d.description().ok()?;
            let mut name = desc.name().to_owned();
            if pipewire {
                match id.as_str() {
                    "output_default" => name = "System default".into(),
                    // Same thing as output_default.
                    "sink_default" => return None,
                    // Other applications' playback streams, not places to send sound.
                    _ if desc.direction() == DeviceDirection::Output => return None,
                    // PipeWire ≥ 1.4 puts a public sink in front of the codec node; the
                    // internal one skips that hop, which may matter for latency.
                    _ if id.starts_with("bluez_output.") && id.ends_with(".1") => {
                        name += " (Bluetooth, direct)"
                    }
                    _ if id.starts_with("bluez_output.") => name += " (Bluetooth)",
                    _ => {}
                }
            }
            Some(OutputInfo { id, name })
        })
        .collect())
}

pub struct AudioOut {
    _stream: cpal::Stream,
    pub device_name: String,
    pub sample_rate: u32,
    /// What we asked for; `None` lets the server choose.
    pub requested_buffer: Option<u32>,
}

/// Opens the first output whose id or name contains `query` (default output when `None`)
/// and starts rendering the engine into it.
pub fn open(
    query: Option<&str>,
    buffer: Option<u32>,
    instrument: Arc<Instrument>,
    cmds: Receiver<EngineCmd>,
    garbage: Sender<Arc<Instrument>>,
    stats: Arc<Stats>,
) -> Result<AudioOut> {
    let host = cpal::default_host();
    let device = match query {
        None => host.default_output_device().ok_or_else(|| anyhow!("no default audio output"))?,
        Some(q) => {
            let q = q.to_lowercase();
            host.output_devices()?
                .find(|d| {
                    let id = d.id().map(|i| i.id().to_lowercase()).unwrap_or_default();
                    let name = d.description().map(|n| n.name().to_lowercase()).unwrap_or_default();
                    id.contains(&q) || name.contains(&q)
                })
                .ok_or_else(|| anyhow!("no audio output matching \"{q}\""))?
        }
    };
    let device_name = device
        .description()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|_| "unknown".into());

    let supported = device.default_output_config().context("cannot query output config")?;
    let format = supported.sample_format();
    let mut config: StreamConfig = supported.config();
    // Everything downstream (samples, engine) is tuned for 48 kHz; keep the device default otherwise.
    if device
        .supported_output_configs()
        .is_ok_and(|mut c| c.any(|r| r.sample_format() == format && r.contains_rate(SAMPLE_RATE)))
    {
        config.sample_rate = SAMPLE_RATE;
    }
    config.buffer_size = buffer.map_or(BufferSize::Default, BufferSize::Fixed);

    let engine = Engine::new(config.sample_rate, instrument, cmds, garbage, stats.clone());
    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, &config, engine, stats),
        SampleFormat::I16 => build::<i16>(&device, &config, engine, stats),
        SampleFormat::I32 => build::<i32>(&device, &config, engine, stats),
        SampleFormat::U16 => build::<u16>(&device, &config, engine, stats),
        other => return Err(anyhow!("unsupported sample format {other:?}")),
    }
    .with_context(|| format!("cannot open \"{device_name}\""))?;
    stream.play().context("cannot start audio stream")?;

    Ok(AudioOut {
        _stream: stream,
        device_name,
        sample_rate: config.sample_rate,
        requested_buffer: buffer,
    })
}

fn build<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    mut engine: Engine,
    stats: Arc<Stats>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let mut block = [[0.0f32; 2]; BLOCK];
    device.build_output_stream::<T, _, _>(
        config.clone(),
        move |data: &mut [T], info| {
            let ts = info.timestamp();
            let output_delay_us = ts.playback.duration_since(ts.callback).as_micros() as u32;
            let frames = data.len() / channels;
            engine.begin_callback(frames, output_delay_us);

            for chunk in data.chunks_mut(BLOCK * channels) {
                let n = chunk.len() / channels;
                engine.render(&mut block[..n]);
                for (out, frame) in chunk.chunks_exact_mut(channels).zip(&block[..n]) {
                    for (c, sample) in out.iter_mut().enumerate() {
                        // Mono devices get the left channel; extra channels repeat L/R.
                        *sample = T::from_sample(frame[c % 2]);
                    }
                }
            }
        },
        move |_err| {
            stats.errors.fetch_add(1, Relaxed);
        },
        None,
    )
}
