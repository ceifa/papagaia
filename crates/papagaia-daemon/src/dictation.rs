use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::mpsc;

pub struct Recorder {
    stream: cpal::Stream,
    sample_rate: u32,
    channels: u16,
    samples: Arc<Mutex<Vec<i16>>>,
}

impl Recorder {
    pub fn start(level_tx: mpsc::UnboundedSender<f32>) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .context("no default input device available")?;
        let supported_config = device
            .default_input_config()
            .context("failed to load default input config")?;

        let sample_rate = supported_config.sample_rate().0;
        let channels = supported_config.channels();
        let max_samples =
            sample_rate as usize * channels as usize * MAX_RECORDING_SECS as usize;
        let samples = Arc::new(Mutex::new(Vec::new()));
        let samples_for_callback = samples.clone();
        let level_tx_f32 = level_tx.clone();
        let level_tx_i16 = level_tx.clone();
        let level_tx_u16 = level_tx;
        let error_callback = |error| eprintln!("papagaia recorder error: {error}");

        let stream_config = supported_config.config();
        let stream = match supported_config.sample_format() {
            cpal::SampleFormat::F32 => device.build_input_stream(
                &stream_config,
                move |data: &[f32], _| {
                    push_f32_samples(data, &samples_for_callback, &level_tx_f32, max_samples);
                },
                error_callback,
                None,
            )?,
            cpal::SampleFormat::I16 => device.build_input_stream(
                &stream_config,
                move |data: &[i16], _| {
                    push_i16_samples(data, &samples_for_callback, &level_tx_i16, max_samples);
                },
                error_callback,
                None,
            )?,
            cpal::SampleFormat::U16 => device.build_input_stream(
                &stream_config,
                move |data: &[u16], _| {
                    push_u16_samples(data, &samples_for_callback, &level_tx_u16, max_samples);
                },
                error_callback,
                None,
            )?,
            sample_format => bail!("unsupported input sample format {sample_format:?}"),
        };

        stream.play().context("failed to start input stream")?;

        Ok(Self {
            stream,
            sample_rate,
            channels,
            samples,
        })
    }

    /// Stops recording and writes a WAV file. Returns the path and duration in seconds.
    pub fn finish(self) -> Result<(PathBuf, f64)> {
        drop(self.stream);

        let audio_path = std::env::temp_dir().join(format!(
            "papagaia-{}.wav",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis()
        ));

        let samples = {
            let guard = self.samples.lock().expect("recorder sample lock poisoned");
            guard.clone()
        };

        let prepared = prepare_for_whisper(&samples, self.channels, self.sample_rate);
        let duration_secs = prepared.len() as f64 / WHISPER_SAMPLE_RATE as f64;

        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: WHISPER_SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        let mut writer = hound::WavWriter::create(&audio_path, spec)
            .with_context(|| format!("failed to create {}", audio_path.display()))?;

        for sample in prepared {
            writer.write_sample(sample)?;
        }
        writer.finalize()?;

        Ok((audio_path, duration_secs))
    }
}

const WHISPER_SAMPLE_RATE: u32 = 16000;
pub const MAX_RECORDING_SECS: u64 = 3600; // 1 hour

fn prepare_for_whisper(interleaved: &[i16], channels: u16, sample_rate: u32) -> Vec<i16> {
    let mono = downmix_to_mono(interleaved, channels);
    let resampled = if sample_rate == WHISPER_SAMPLE_RATE {
        mono
    } else {
        resample(&mono, sample_rate, WHISPER_SAMPLE_RATE)
    };
    resampled
        .into_iter()
        .map(|sample| (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect()
}

fn downmix_to_mono(interleaved: &[i16], channels: u16) -> Vec<f32> {
    let channels = channels.max(1) as usize;
    let scale = 1.0 / (i16::MAX as f32 * channels as f32);
    interleaved
        .chunks_exact(channels)
        .map(|frame| frame.iter().map(|sample| *sample as f32).sum::<f32>() * scale)
        .collect()
}

// Resamples mono audio using a box-filter kernel. The averaging window acts as
// a cheap anti-aliasing filter when downsampling, which is the common case
// (device rates of 44.1/48 kHz down to 16 kHz). For the rare upsampling case
// the window collapses to a single source sample (nearest neighbour).
fn resample(samples: &[f32], input_rate: u32, output_rate: u32) -> Vec<f32> {
    if samples.is_empty() || input_rate == output_rate {
        return samples.to_vec();
    }

    let ratio = input_rate as f64 / output_rate as f64;
    let output_len = ((samples.len() as f64) / ratio).floor() as usize;
    let mut output = Vec::with_capacity(output_len);

    for i in 0..output_len {
        let start = (i as f64 * ratio) as usize;
        let end = (((i + 1) as f64 * ratio) as usize)
            .max(start + 1)
            .min(samples.len());
        let slice = &samples[start..end];
        let avg = slice.iter().sum::<f32>() / slice.len() as f32;
        output.push(avg);
    }

    output
}

fn push_f32_samples(
    data: &[f32],
    samples: &Arc<Mutex<Vec<i16>>>,
    level_tx: &mpsc::UnboundedSender<f32>,
    max_samples: usize,
) {
    let converted: Vec<i16> = data
        .iter()
        .map(|sample| (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect();
    push_samples(&converted, samples, level_tx, max_samples);
}

fn push_i16_samples(
    data: &[i16],
    samples: &Arc<Mutex<Vec<i16>>>,
    level_tx: &mpsc::UnboundedSender<f32>,
    max_samples: usize,
) {
    push_samples(data, samples, level_tx, max_samples);
}

fn push_u16_samples(
    data: &[u16],
    samples: &Arc<Mutex<Vec<i16>>>,
    level_tx: &mpsc::UnboundedSender<f32>,
    max_samples: usize,
) {
    let converted: Vec<i16> = data
        .iter()
        .map(|sample| (*sample as i32 - i16::MAX as i32 - 1) as i16)
        .collect();
    push_samples(&converted, samples, level_tx, max_samples);
}

fn push_samples(
    data: &[i16],
    samples: &Arc<Mutex<Vec<i16>>>,
    level_tx: &mpsc::UnboundedSender<f32>,
    max_samples: usize,
) {
    if let Ok(mut collected) = samples.lock() {
        let current = collected.len();
        if current < max_samples {
            let remaining = max_samples - current;
            if data.len() <= remaining {
                collected.extend_from_slice(data);
            } else {
                collected.extend_from_slice(&data[..remaining]);
            }
        }
    }

    if !data.is_empty() {
        let rms = (data
            .iter()
            .map(|sample| {
                let sample = *sample as f32 / i16::MAX as f32;
                sample * sample
            })
            .sum::<f32>()
            / data.len() as f32)
            .sqrt();
        let _ = level_tx.send(rms.clamp(0.0, 1.0));
    }
}
