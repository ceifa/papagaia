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
                    push_f32_samples(data, &samples_for_callback, &level_tx_f32);
                },
                error_callback,
                None,
            )?,
            cpal::SampleFormat::I16 => device.build_input_stream(
                &stream_config,
                move |data: &[i16], _| {
                    push_i16_samples(data, &samples_for_callback, &level_tx_i16);
                },
                error_callback,
                None,
            )?,
            cpal::SampleFormat::U16 => device.build_input_stream(
                &stream_config,
                move |data: &[u16], _| {
                    push_u16_samples(data, &samples_for_callback, &level_tx_u16);
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

    pub fn finish(self) -> Result<PathBuf> {
        drop(self.stream);

        let audio_path = std::env::temp_dir().join(format!(
            "papagaia-{}.wav",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis()
        ));

        let spec = hound::WavSpec {
            channels: self.channels,
            sample_rate: self.sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        let mut writer = hound::WavWriter::create(&audio_path, spec)
            .with_context(|| format!("failed to create {}", audio_path.display()))?;

        let samples = self.samples.lock().expect("recorder sample lock poisoned");
        for sample in samples.iter().copied() {
            writer.write_sample(sample)?;
        }
        writer.finalize()?;

        Ok(audio_path)
    }
}

fn push_f32_samples(
    data: &[f32],
    samples: &Arc<Mutex<Vec<i16>>>,
    level_tx: &mpsc::UnboundedSender<f32>,
) {
    let converted: Vec<i16> = data
        .iter()
        .map(|sample| (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect();
    push_samples(&converted, samples, level_tx);
}

fn push_i16_samples(
    data: &[i16],
    samples: &Arc<Mutex<Vec<i16>>>,
    level_tx: &mpsc::UnboundedSender<f32>,
) {
    push_samples(data, samples, level_tx);
}

fn push_u16_samples(
    data: &[u16],
    samples: &Arc<Mutex<Vec<i16>>>,
    level_tx: &mpsc::UnboundedSender<f32>,
) {
    let converted: Vec<i16> = data
        .iter()
        .map(|sample| (*sample as i32 - i16::MAX as i32 - 1) as i16)
        .collect();
    push_samples(&converted, samples, level_tx);
}

fn push_samples(
    data: &[i16],
    samples: &Arc<Mutex<Vec<i16>>>,
    level_tx: &mpsc::UnboundedSender<f32>,
) {
    if let Ok(mut collected) = samples.lock() {
        collected.extend_from_slice(data);
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
