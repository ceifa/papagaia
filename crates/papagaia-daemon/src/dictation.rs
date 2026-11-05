use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use alsa::{
    Direction, ValueOr,
    pcm::{Access, Format, HwParams, PCM},
};
use anyhow::{Context, Result};
use papagaia_core::Config;
use tokio::sync::watch;

const WHISPER_SAMPLE_RATE: u32 = 16000;
pub const MAX_RECORDING_SECS: u64 = 3600; // 1 hour
/// 20 ms: the release tail is counted from the audio read so far, so a read
/// still in flight at the release would shorten it by up to its own length.
const FRAMES_PER_READ: usize = 320;

/// Audio kept past the key release: people let go while still finishing the last
/// word, and cutting it mid-syllable makes whisper guess another one.
const RELEASE_TAIL_MS: usize = 200;
/// While speech runs into the tail it grows, up to this much past the release.
const MAX_RELEASE_TAIL_MS: usize = 600;
const FRAME_MS: usize = 20;
/// Speech must fill this much of the audio right before the tail's end to grow it,
/// so a key click never does.
const SPEECH_RUN_MS: usize = 100;
/// A grown tail ends after this much quiet.
const QUIET_END_MS: usize = 160;
/// Only this much audio before the release estimates the noise floor.
const NOISE_FLOOR_WINDOW_MS: usize = 10_000;
/// Speech is 12 dB (4x amplitude) over the noise floor, and never quieter than this.
const SPEECH_OVER_FLOOR: f32 = 4.0;
const MIN_SPEECH_RMS: f32 = 300.0;

pub struct Recorder {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    samples: Option<Arc<Mutex<Vec<i16>>>>,
}

impl Recorder {
    pub fn start(level_tx: watch::Sender<f32>) -> Result<Self> {
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

    /// Captures the release tail, stops recording and writes a WAV file. Returns the
    /// path and duration in seconds.
    pub fn finish(mut self) -> Result<(PathBuf, f64)> {
        let samples_arc = self.samples.take().expect("samples taken twice");
        let end = self.wait_for_tail(&samples_arc);

        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }

        let mut samples = Arc::try_unwrap(samples_arc)
            .map(|mutex| mutex.into_inner().expect("recorder sample lock poisoned"))
            .unwrap_or_else(|arc| arc.lock().expect("recorder sample lock poisoned").clone());
        samples.truncate(end);

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

    /// Keep capturing past the release until `release_tail_end` says the clip is
    /// complete, and return where it ends. A stalled or dead capture can't hold the
    /// dictation hostage: it finishes with whatever arrived.
    fn wait_for_tail(&self, samples: &Mutex<Vec<i16>>) -> usize {
        let release = samples.lock().expect("recorder sample lock poisoned").len();
        let start = release.saturating_sub(ms_to_samples(NOISE_FLOOR_WINDOW_MS));
        // The pre-release audio never changes. Classify its noise floor once,
        // rather than rescanning and sorting it every 20 ms while waiting.
        let threshold = speech_threshold(
            &samples.lock().expect("recorder sample lock poisoned")[start..release],
        );
        let deadline = Instant::now() + Duration::from_millis(MAX_RELEASE_TAIL_MS as u64 + 100);
        loop {
            let captured = samples.lock().expect("recorder sample lock poisoned");
            if let Some(end) = release_tail_end(&captured[start..], release - start, threshold) {
                return start + end;
            }
            let capturing = self.handle.as_ref().is_some_and(|h| !h.is_finished());
            if !capturing || Instant::now() >= deadline {
                return captured.len();
            }
            drop(captured);
            thread::sleep(Duration::from_millis(FRAME_MS as u64));
        }
    }
}

fn ms_to_samples(ms: usize) -> usize {
    ms * WHISPER_SAMPLE_RATE as usize / 1000
}

/// Where a clip released at sample `release` should end, or `None` while more
/// audio is needed to decide. The tail is `RELEASE_TAIL_MS`, unless speech is
/// still running at its end: then it lasts until `QUIET_END_MS` of quiet, capped
/// at `MAX_RELEASE_TAIL_MS`.
fn release_tail_end(samples: &[i16], release: usize, threshold: f32) -> Option<usize> {
    let frame = ms_to_samples(FRAME_MS);
    let base = release + ms_to_samples(RELEASE_TAIL_MS);
    let cap = release + ms_to_samples(MAX_RELEASE_TAIL_MS);
    if samples.len() < base {
        return None;
    }

    let is_speech = |end: usize| frame_rms(&samples[end - frame..end]) > threshold;

    let run_frames = SPEECH_RUN_MS / FRAME_MS;
    let speech_in_run = (0..run_frames)
        .filter(|k| is_speech(base - k * frame))
        .count();
    // One quiet frame is a gap between syllables, not the end of the word.
    if speech_in_run + 1 < run_frames {
        return Some(base);
    }

    let quiet_frames = QUIET_END_MS / FRAME_MS;
    let mut quiet = 0;
    let mut end = base + frame;
    while end <= samples.len().min(cap) {
        quiet = if is_speech(end) { 0 } else { quiet + 1 };
        if quiet == quiet_frames {
            return Some(end);
        }
        end += frame;
    }
    (samples.len() >= cap).then_some(cap)
}

fn speech_threshold(samples: &[i16]) -> f32 {
    (noise_floor(samples, ms_to_samples(FRAME_MS)) * SPEECH_OVER_FLOOR).max(MIN_SPEECH_RMS)
}

/// The room's level: the quietest tenth of the frames before the release.
fn noise_floor(samples: &[i16], frame: usize) -> f32 {
    let mut levels: Vec<f32> = samples.chunks_exact(frame).map(frame_rms).collect();
    if levels.is_empty() {
        return 0.0;
    }
    let index = levels.len() / 10;
    *levels.select_nth_unstable_by(index, f32::total_cmp).1
}

fn frame_rms(frame: &[i16]) -> f32 {
    let sum_sq: f64 = frame.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (sum_sq / frame.len() as f64).sqrt() as f32
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
    level_tx: watch::Sender<f32>,
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
        if !level_tx.is_closed()
            && let Some(rms) = compute_rms(chunk)
        {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn release_tail_end(samples: &[i16], release: usize) -> Option<usize> {
        super::release_tail_end(samples, release, speech_threshold(&samples[..release]))
    }

    const ROOM: i16 = 40;
    const VOICE: i16 = 4000;

    /// A square wave at `amplitude`: steady RMS, enough for the energy classifier.
    fn tone(ms: usize, amplitude: i16) -> Vec<i16> {
        (0..ms_to_samples(ms))
            .map(|i| if i % 32 < 16 { amplitude } else { -amplitude })
            .collect()
    }

    fn clip(parts: &[(usize, i16)]) -> Vec<i16> {
        parts.iter().flat_map(|&(ms, amp)| tone(ms, amp)).collect()
    }

    #[test]
    fn waits_until_the_base_tail_has_arrived() {
        let samples = clip(&[(1000, ROOM), (150, ROOM)]);
        assert_eq!(release_tail_end(&samples, ms_to_samples(1000)), None);
    }

    #[test]
    fn a_release_after_the_speech_keeps_just_the_base_tail() {
        let samples = clip(&[(500, ROOM), (500, VOICE), (300, ROOM), (700, ROOM)]);
        let release = ms_to_samples(1300);
        assert_eq!(
            release_tail_end(&samples, release),
            Some(release + ms_to_samples(RELEASE_TAIL_MS))
        );
    }

    #[test]
    fn a_word_said_through_the_release_keeps_the_tail_open_until_it_ends() {
        // Released at 1000 ms while the word runs on to 1360 ms.
        let samples = clip(&[(500, ROOM), (860, VOICE), (700, ROOM)]);
        let release = ms_to_samples(1000);
        let end = ms_to_samples(1360 + QUIET_END_MS);
        assert_eq!(release_tail_end(&samples, release), Some(end));
        // Until that quiet has arrived, the clip isn't done.
        assert_eq!(release_tail_end(&samples[..end - 1], release), None);
    }

    #[test]
    fn speech_that_carries_on_stops_the_tail_at_its_cap() {
        let samples = clip(&[(500, ROOM), (2000, VOICE)]);
        let release = ms_to_samples(1000);
        assert_eq!(
            release_tail_end(&samples, release),
            Some(release + ms_to_samples(MAX_RELEASE_TAIL_MS))
        );
    }

    #[test]
    fn a_key_click_never_grows_the_tail() {
        // A 40 ms click right at the base tail's end.
        let samples = clip(&[(1000, ROOM), (160, ROOM), (40, VOICE), (800, ROOM)]);
        let release = ms_to_samples(1000);
        assert_eq!(
            release_tail_end(&samples, release),
            Some(release + ms_to_samples(RELEASE_TAIL_MS))
        );
    }
}
