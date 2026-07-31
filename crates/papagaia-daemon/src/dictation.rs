use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

use alsa::{
    Direction, ValueOr,
    pcm::{Access, Format, HwParams, PCM},
};
use anyhow::{Context, Result};
use papagaia_core::Config;
use tokio::sync::mpsc;

const WHISPER_SAMPLE_RATE: u32 = 16000;
pub const MAX_RECORDING_SECS: u64 = 3600; // 1 hour
const FRAMES_PER_READ: usize = 1024;

pub struct Recorder {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    samples: Option<Arc<Mutex<Vec<i16>>>>,
}

impl Recorder {
    pub fn start(level_tx: mpsc::UnboundedSender<f32>) -> Result<Self> {
        // The "default" ALSA device on a normal Linux desktop is the plug
        // device — it transparently performs sample-rate conversion, channel
        // downmix, and format conversion in libasound. We can therefore ask it
        // directly for whisper's expected format (16 kHz, mono, S16) and skip
        // the in-process resampling entirely.
        let pcm = PCM::new("default", Direction::Capture, false)
            .context("failed to open the default ALSA capture device")?;

        {
            let hwp = HwParams::any(&pcm).context("failed to read ALSA hw params")?;
            hwp.set_access(Access::RWInterleaved)
                .context("ALSA: failed to set RWInterleaved access")?;
            hwp.set_format(Format::s16())
                .context("ALSA: failed to set S16 format")?;
            hwp.set_channels(1)
                .context("ALSA: failed to set mono channel layout")?;
            hwp.set_rate(WHISPER_SAMPLE_RATE, ValueOr::Nearest)
                .context("ALSA: failed to set 16 kHz sample rate")?;
            pcm.hw_params(&hwp)
                .context("ALSA: failed to apply hw params")?;
        }
        pcm.start().context("ALSA: failed to start capture")?;

        let stop = Arc::new(AtomicBool::new(false));
        // Reserve a few seconds up front so the first handful of capture reads
        // don't each trigger a reallocation. Capped growth (max_samples) keeps
        // the worst case bounded; this only avoids the early churn.
        let samples: Arc<Mutex<Vec<i16>>> = Arc::new(Mutex::new(Vec::with_capacity(
            WHISPER_SAMPLE_RATE as usize * 4,
        )));
        let max_samples = WHISPER_SAMPLE_RATE as usize * MAX_RECORDING_SECS as usize;

        let stop_for_thread = stop.clone();
        let samples_for_thread = samples.clone();
        let handle = thread::spawn(move || {
            capture_loop(
                pcm,
                stop_for_thread,
                samples_for_thread,
                level_tx,
                max_samples,
            );
        });

        Ok(Self {
            stop,
            handle: Some(handle),
            samples: Some(samples),
        })
    }

    /// Stops recording and writes a WAV file. Returns the path and duration in seconds.
    pub fn finish(mut self) -> Result<(PathBuf, f64)> {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }

        let samples_arc = self.samples.take().expect("samples taken twice");
        let samples = Arc::try_unwrap(samples_arc)
            .map(|mutex| mutex.into_inner().expect("recorder sample lock poisoned"))
            .unwrap_or_else(|arc| arc.lock().expect("recorder sample lock poisoned").clone());

        let audio_path = std::env::temp_dir().join(format!(
            "papagaia-{}.wav",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis()
        ));

        let duration_secs = samples.len() as f64 / WHISPER_SAMPLE_RATE as f64;

        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: WHISPER_SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        let mut writer = hound::WavWriter::create(&audio_path, spec)
            .with_context(|| format!("failed to create {}", audio_path.display()))?;

        for sample in samples {
            writer.write_sample(sample)?;
        }
        writer.finalize()?;

        Ok((audio_path, duration_secs))
    }
}

/// Dispose of a finished recording: delete it, unless `dictation.recordings_dir`
/// is set — then archive it there first, next to a `.txt` holding the raw
/// (pre-cleanup) transcript, so a real utterance can be replayed later against
/// different whisper settings.
///
/// Archiving copies up to `MAX_RECORDING_SECS` of audio across filesystems, which
/// would stall the current-thread runtime, so it runs on the blocking pool.
/// Nothing awaits it: a debug aid must never delay or fail a dictation.
pub fn retire_recording(config: &Arc<Config>, audio_path: PathBuf, transcript: &str) {
    if config.dictation.recordings_dir.trim().is_empty() {
        std::fs::remove_file(&audio_path).ok();
        return;
    }
    let config = config.clone();
    let transcript = transcript.to_string();
    tokio::task::spawn_blocking(move || {
        archive_recording(&config, &audio_path, &transcript);
        // A successful rename already moved it; this covers the copy fallback.
        std::fs::remove_file(&audio_path).ok();
    });
}

fn archive_recording(config: &Config, audio_path: &Path, transcript: &str) {
    let dir = Path::new(config.dictation.recordings_dir.trim());
    if let Err(error) = std::fs::create_dir_all(dir) {
        if config.logging {
            eprintln!(
                "papagaia: could not create recordings dir {}: {error}",
                dir.display()
            );
        }
        return;
    }

    // Reuse the recording's own file name, so the WAV and its transcript share a
    // stem and `prune_recordings` can sort them chronologically.
    let name = audio_path
        .file_name()
        .unwrap_or_else(|| OsStr::new("recording.wav"));
    let wav_dest = dir.join(name);
    // `rename` fails across filesystems (the recording lives in /tmp, usually a
    // tmpfs), so fall back to a copy.
    if std::fs::rename(audio_path, &wav_dest).is_err()
        && std::fs::copy(audio_path, &wav_dest).is_err()
    {
        if config.logging {
            eprintln!("papagaia: could not archive {}", audio_path.display());
        }
        return;
    }
    std::fs::write(wav_dest.with_extension("txt"), format!("{transcript}\n")).ok();

    prune_recordings(dir, config.dictation.recordings_keep);
}

/// Delete the oldest recordings (WAV plus its transcript) past `keep`, so an
/// always-on debug directory can't grow without bound. `keep == 0` means no cap.
fn prune_recordings(dir: &Path, keep: usize) {
    if keep == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut wavs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "wav"))
        .collect();
    if wavs.len() <= keep {
        return;
    }
    // `finish` names recordings `papagaia-<millis>.wav`, so a lexicographic sort
    // is chronological (millisecond stamps stay the same width for centuries).
    wavs.sort();
    let excess = wavs.len() - keep;
    for wav in wavs.into_iter().take(excess) {
        std::fs::remove_file(&wav).ok();
        std::fs::remove_file(wav.with_extension("txt")).ok();
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn capture_loop(
    pcm: PCM,
    stop: Arc<AtomicBool>,
    samples: Arc<Mutex<Vec<i16>>>,
    level_tx: mpsc::UnboundedSender<f32>,
    max_samples: usize,
) {
    let io = match pcm.io_i16() {
        Ok(io) => io,
        Err(error) => {
            eprintln!("papagaia recorder: failed to acquire ALSA i16 io handle: {error}");
            return;
        }
    };

    let mut buf = [0_i16; FRAMES_PER_READ];
    while !stop.load(Ordering::Acquire) {
        let frames = match io.readi(&mut buf) {
            Ok(n) => n,
            Err(error) => {
                // Recover from underrun/overrun (xrun) and try again. Other
                // errors abort the loop — there's no useful retry.
                if pcm.recover(error.errno(), true).is_err() {
                    eprintln!("papagaia recorder error: {error}");
                    break;
                }
                continue;
            }
        };

        if frames == 0 {
            continue;
        }

        let chunk = &buf[..frames];
        push_samples(chunk, &samples, max_samples);
        if let Some(rms) = compute_rms(chunk) {
            let _ = level_tx.send(rms);
        }
    }

    let _ = pcm.drop();
}

fn push_samples(data: &[i16], samples: &Arc<Mutex<Vec<i16>>>, max_samples: usize) {
    let Ok(mut collected) = samples.lock() else {
        return;
    };
    let current = collected.len();
    if current >= max_samples {
        return;
    }
    let remaining = max_samples - current;
    let take = data.len().min(remaining);
    collected.extend_from_slice(&data[..take]);
}

fn compute_rms(data: &[i16]) -> Option<f32> {
    if data.is_empty() {
        return None;
    }
    let sum_sq: f32 = data
        .iter()
        .map(|sample| {
            let normalized = *sample as f32 / i16::MAX as f32;
            normalized * normalized
        })
        .sum();
    let rms = (sum_sq / data.len() as f32).sqrt();
    Some(rms.clamp(0.0, 1.0))
}
