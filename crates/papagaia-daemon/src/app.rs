use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use anyhow::{Context, Result, bail};
use papagaia_core::{
    ClientRequest, ClientResponse, Config, OverlayMessage, PickerEntry, PickerResult, PromptConfig,
    overlay_program, prompt_summary, template_needs_selection,
};
use tokio::{
    sync::{Mutex, mpsc},
    task::JoinHandle,
    time::{Duration, sleep},
};

use crate::{
    cancel::CancelToken,
    cleanup, clipboard,
    dictation::{self, MAX_RECORDING_SECS, Recorder},
    llm,
    overlay::OverlayHandle,
    stt,
    stt::WhisperServer,
};

macro_rules! log {
    ($config:expr, $($arg:tt)*) => {
        if $config.logging {
            eprintln!($($arg)*);
        }
    };
}

/// After releasing the overlay's keyboard grab, wait for the compositor to return
/// focus to the target window before driving it with wtype — else the paste lands
/// on the overlay.
const FOCUS_RETURN_SETTLE_MS: u64 = 80;

/// How long the overlay lingers on a result before hiding.
const RESULT_FLASH_MS: u64 = 900;
const ERROR_FLASH_MS: u64 = 3000;
const NOTICE_FLASH_MS: u64 = 1300;

/// An expected non-result (tap too short, no speech) — surfaced as a neutral
/// overlay notice rather than a red error, and reported to the client as success.
#[derive(Debug)]
struct SoftOutcome(String);

impl std::fmt::Display for SoftOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SoftOutcome {}

pub struct App {
    config: Arc<Config>,
    overlay: OverlayHandle,
    state: Mutex<State>,
    overlay_epoch: AtomicU64,
    /// True during a blocking phase (`State::Busy` or the open picker) — read by
    /// the keybind watcher to drop presses instead of queueing them. Left false
    /// during `Recording` so a quick push-to-talk tap is never seen as busy.
    busy: Arc<AtomicBool>,
    /// The warm whisper-server handle, when the `server` backend is configured.
    /// `None` means transcription uses the `whisper-cli` path directly.
    whisper_server: Option<Arc<WhisperServer>>,
}

enum State {
    Idle,
    Busy {
        label: String,
        cancel: CancelToken,
        overlay_epoch: u64,
    },
    Recording(RecordingSession),
}

struct RecordingSession {
    recorder: Recorder,
    overlay_epoch: u64,
    /// Forwards mic-level samples to the overlay. Aborted on every transition out
    /// of `Recording` so a trailing RMS sample can't land after the next state's
    /// message.
    level_forwarder: JoinHandle<()>,
    /// Stops a runaway recording after `MAX_RECORDING_SECS`. Aborted on a normal
    /// stop so no hour-long timer stays parked.
    auto_stop: JoinHandle<()>,
}

struct BusySession {
    cancel: CancelToken,
    overlay_epoch: u64,
}

impl App {
    pub async fn new(config: Config) -> Result<Self> {
        let overlay = OverlayHandle::spawn(config.overlay.enabled)?;
        // Launch (and supervise) the warm whisper-server up front so the model is
        // resident by the time the first dictation lands. Returns None for the
        // `cli` backend, in which case transcription shells out per call.
        let whisper_server = WhisperServer::launch(&config.whisper);
        Ok(Self {
            config: Arc::new(config),
            overlay,
            state: Mutex::new(State::Idle),
            overlay_epoch: AtomicU64::new(0),
            busy: Arc::new(AtomicBool::new(false)),
            whisper_server,
        })
    }

    fn config(&self) -> Arc<Config> {
        self.config.clone()
    }

    pub fn busy_flag(&self) -> Arc<AtomicBool> {
        self.busy.clone()
    }

    pub async fn handle(self: &Arc<Self>, request: ClientRequest) -> Result<ClientResponse> {
        let config = self.config();
        log!(config, "[papagaia] request: {}", request_label(&request));
        match request {
            ClientRequest::Status => {
                let state = self.state.lock().await;
                let message = match &*state {
                    State::Idle => "idle",
                    State::Busy { label, .. } => label.as_str(),
                    State::Recording(_) => "recording",
                };
                Ok(ClientResponse::ok(message))
            }
            ClientRequest::Transform { prompt } => self.transform(&prompt).await,
            ClientRequest::TransformRaw { template } => self.transform_raw(&template).await,
            ClientRequest::Pick => self.pick().await,
            ClientRequest::DictateStart => self.dictate_start().await,
            ClientRequest::DictateStop => self.dictate_stop().await,
            ClientRequest::DictateToggle => self.dictate_toggle().await,
            ClientRequest::Cancel => self.cancel().await,
        }
    }

    async fn transform(&self, prompt_name: &str) -> Result<ClientResponse> {
        let prompt = self.config().prompt(prompt_name)?.clone();
        self.run_transform(prompt, prompt_name).await
    }

    async fn transform_raw(&self, template: &str) -> Result<ClientResponse> {
        let prompt = PromptConfig {
            name: "ad-hoc".into(),
            template: template.into(),
        };
        self.run_transform(prompt, "engine output").await
    }

    /// Show the prompt picker and run whatever the user chooses. The daemon owns
    /// the whole flow now: it builds the entries from its own config, spawns the
    /// picker overlay, reads the choice, and dispatches the transform — so the CLI
    /// is just a thin `Pick` request (no more CLI→overlay→CLI→daemon round-trip).
    async fn pick(&self) -> Result<ClientResponse> {
        // Stay busy for the whole picker flow (the inner `enter_busy`/`leave_busy`
        // only cover the transform) so pick-key spam while it's open is swallowed.
        self.busy.store(true, Ordering::SeqCst);
        let result = self.pick_inner().await;
        self.busy.store(false, Ordering::SeqCst);
        result
    }

    async fn pick_inner(&self) -> Result<ClientResponse> {
        let config = self.config();
        let entries: Vec<PickerEntry> = config
            .prompts
            .iter()
            .map(|prompt| PickerEntry {
                name: prompt.name.clone(),
                summary: prompt_summary(&prompt.template),
            })
            .collect();
        let entries_json = serde_json::to_string(&entries)?;

        let Some(choice) = run_picker(&entries_json).await? else {
            return Ok(ClientResponse::ok("picker cancelled"));
        };

        // Let the compositor return focus to the previous window before the
        // transform grabs the selection / pastes into it.
        sleep(Duration::from_millis(FOCUS_RETURN_SETTLE_MS)).await;

        match choice {
            PickerResult::Template { name } => self.transform(&name).await,
            PickerResult::Raw { template } => self.transform_raw(&template).await,
        }
    }

    /// Run one prompt through the engine and insert the result. Owns the whole
    /// pipeline: busy state, selection capture, engine, paste, and overlay
    /// feedback — one linear flow, like `dictate_stop`.
    async fn run_transform(
        &self,
        prompt: PromptConfig,
        success_label: &str,
    ) -> Result<ClientResponse> {
        let session = self
            .enter_busy(format!("running prompt '{}'", prompt.name))
            .await?;
        let config = self.config();
        let overlay = self.overlay.clone();
        let cancel = session.cancel.clone();

        let outcome: Result<(String, bool)> = async {
            let label = format!("Running {}", prompt.name);

            // Capture phase: wtype needs the original window focused — no grab.
            overlay
                .send(OverlayMessage::Busy {
                    label: label.clone(),
                    grab_keyboard: false,
                })
                .await;
            let selected = resolve_selected_text(&config.tools, &prompt.template, &cancel).await?;

            // Engine phase: grab the keyboard so Esc cancels the engine.
            overlay
                .send(OverlayMessage::Busy {
                    label: label.clone(),
                    grab_keyboard: true,
                })
                .await;

            let rendered = match &selected {
                Some(text) => prompt.render(text),
                None => prompt.template.clone(),
            };
            log!(config, "[transform] selected={}", selected.is_some());

            let raw = llm::run_engine(&config.engine, &rendered, &cancel).await?;
            let output = prompt.clean_output(&raw);

            // Paste phase: release the grab so focus returns before wtype pastes.
            overlay
                .send(OverlayMessage::Busy {
                    label,
                    grab_keyboard: false,
                })
                .await;
            sleep(Duration::from_millis(FOCUS_RETURN_SETTLE_MS)).await;
            clipboard::paste_text(&config.tools, &output, &cancel).await?;

            log!(config, "[transform] final output: {output}");
            Ok((output, selected.is_some()))
        }
        .await;

        self.leave_busy(session.overlay_epoch).await;

        match outcome {
            Ok((text, had_selection)) => {
                let msg = if had_selection {
                    format!("Replaced selection with {success_label}")
                } else {
                    format!("Pasted {success_label}")
                };
                log!(config, "[transform] {msg}");
                self.flash_result(session.overlay_epoch, true, msg).await;
                Ok(ClientResponse::with_text("transform complete", text))
            }
            Err(error) => {
                log!(config, "[transform] error: {error:#}");
                self.finish_error(session.overlay_epoch, &session.cancel, &error)
                    .await;
                Err(error)
            }
        }
    }

    async fn dictate_start(self: &Arc<Self>) -> Result<ClientResponse> {
        let overlay_epoch;
        {
            let mut state = self.state.lock().await;
            if !matches!(*state, State::Idle) {
                bail!("papagaia is already busy");
            }
            overlay_epoch = self.next_overlay_epoch();

            // Start capturing audio immediately so the mic catches the user's
            // first words without waiting on anything else.
            let (level_tx, mut level_rx) = mpsc::unbounded_channel();
            let recorder = Recorder::start(level_tx)?;
            let overlay = self.overlay.clone();
            let level_forwarder = tokio::spawn(async move {
                while let Some(level) = level_rx.recv().await {
                    overlay.send(OverlayMessage::Recording { level }).await;
                }
            });

            // Auto-stop after the max duration to bound memory/WAV size. The
            // handle is kept so a normal stop aborts it (no stale parked timer).
            let app = self.clone();
            let auto_stop = tokio::spawn(async move {
                sleep(Duration::from_secs(MAX_RECORDING_SECS)).await;
                app.auto_stop_recording(overlay_epoch).await;
            });

            *state = State::Recording(RecordingSession {
                recorder,
                overlay_epoch,
                level_forwarder,
                auto_stop,
            });
        }

        // Show the recording HUD right away so feedback is instant.
        self.overlay
            .send(OverlayMessage::Recording { level: 0.0 })
            .await;

        Ok(ClientResponse::ok("dictation started"))
    }

    async fn dictate_stop(&self) -> Result<ClientResponse> {
        let cancel = CancelToken::new();
        let (recorder, overlay_epoch) = {
            let mut state = self.state.lock().await;
            if !matches!(*state, State::Recording(_)) {
                bail!("papagaia is not recording");
            }
            let State::Recording(session) = std::mem::replace(&mut *state, State::Idle) else {
                unreachable!()
            };
            // Same race as in `cancel`: kill the forwarder before we send the
            // Busy(Transcribing) message so trailing Recording samples can't
            // overwrite it.
            session.level_forwarder.abort();
            session.auto_stop.abort();
            let overlay_epoch = session.overlay_epoch;
            *state = State::Busy {
                label: "transcribing".into(),
                cancel: cancel.clone(),
                overlay_epoch,
            };
            self.busy.store(true, Ordering::SeqCst);
            (session.recorder, overlay_epoch)
        };

        // Transcribe phase: whisper reads the WAV file, no foreign focus needed,
        // so grab the keyboard exclusively to let the user press Esc to cancel.
        self.overlay
            .send(OverlayMessage::Busy {
                label: "Transcribing".into(),
                grab_keyboard: true,
            })
            .await;

        let config = self.config();
        let overlay = self.overlay.clone();
        let whisper_server = self.whisper_server.clone();
        let outcome = async {
            // recorder.finish() joins the capture thread and writes the WAV —
            // both blocking, so keep it off the current-thread runtime.
            let (audio_path, _duration_secs) =
                tokio::task::spawn_blocking(move || recorder.finish()).await??;
            // No minimum-duration floor — whisper's VAD and the empty-transcript
            // guard below already drop accidental near-silent taps.
            let transcript =
                stt::transcribe(&config.whisper, &whisper_server, &audio_path, &cancel).await?;
            let transcript = transcript.trim().to_string();
            log!(config, "[dictate] transcript: {transcript}");
            if transcript.is_empty() {
                dictation::retire_recording(&config, audio_path, &transcript);
                return Err(SoftOutcome("No speech detected".into()).into());
            }

            // Instant local cleanup — no LLM. (Run a transform prompt afterwards
            // for LLM rewriting.)
            let cleaned = cleanup::clean(&config.dictation.cleanup, &transcript);
            log!(config, "[dictate] cleaned: {cleaned}");

            // Type phase: release the grab so focus returns to the target
            // window before wtype types the transcript into it.
            overlay
                .send(OverlayMessage::Busy {
                    label: "Typing".into(),
                    grab_keyboard: false,
                })
                .await;
            sleep(Duration::from_millis(FOCUS_RETURN_SETTLE_MS)).await;

            clipboard::paste_text(&config.tools, &cleaned, &cancel).await?;
            dictation::retire_recording(&config, audio_path, &transcript);
            Ok::<String, anyhow::Error>(cleaned)
        }
        .await;

        self.leave_busy(overlay_epoch).await;

        match outcome {
            Ok(text) => {
                log!(config, "[dictate] inserted: {text}");
                self.flash_result(overlay_epoch, true, "Dictation inserted")
                    .await;
                Ok(ClientResponse::with_text("dictation complete", text))
            }
            Err(error) => {
                if let Some(soft) = error.downcast_ref::<SoftOutcome>() {
                    let message = soft.0.clone();
                    log!(config, "[dictate] {message}");
                    self.flash_notice(overlay_epoch, message.clone()).await;
                    return Ok(ClientResponse::ok(message));
                }
                log!(config, "[dictate] error: {error:#}");
                self.finish_error(overlay_epoch, &cancel, &error).await;
                Err(error)
            }
        }
    }

    async fn cancel(&self) -> Result<ClientResponse> {
        let mut state = self.state.lock().await;
        match std::mem::replace(&mut *state, State::Idle) {
            State::Recording(session) => {
                // Abort the forwarder first: a trailing RMS sample landing after
                // our Hidden would leave the overlay stuck at "Listening…".
                session.level_forwarder.abort();
                session.auto_stop.abort();
                drop(session.recorder);
                drop(state);
                self.overlay.send(OverlayMessage::Hidden).await;
                Ok(ClientResponse::ok("dictation cancelled"))
            }
            State::Busy {
                label,
                cancel,
                overlay_epoch,
            } => {
                // Leave Busy in place so the in-flight op unwinds normally; just
                // flip the cancel flag, which its subprocess wait loop notices.
                *state = State::Busy {
                    label,
                    cancel: cancel.clone(),
                    overlay_epoch,
                };
                drop(state);
                cancel.cancel();
                Ok(ClientResponse::ok("cancellation requested"))
            }
            State::Idle => {
                *state = State::Idle;
                Ok(ClientResponse::ok("nothing to cancel"))
            }
        }
    }

    async fn auto_stop_recording(&self, recording_epoch: u64) {
        let was_recording = {
            let mut state = self.state.lock().await;
            match &*state {
                State::Recording(session) if session.overlay_epoch == recording_epoch => {
                    let old = std::mem::replace(&mut *state, State::Idle);
                    if let State::Recording(session) = old {
                        session.level_forwarder.abort();
                        drop(session.recorder);
                    }
                    true
                }
                _ => false,
            }
        };

        if was_recording {
            let config = self.config();
            log!(
                config,
                "[dictate] auto-stopped: maximum recording duration reached"
            );
            self.flash_result(
                recording_epoch,
                false,
                "Recording stopped: maximum duration reached",
            )
            .await;
        }
    }

    async fn dictate_toggle(self: &Arc<Self>) -> Result<ClientResponse> {
        let is_recording = {
            let state = self.state.lock().await;
            matches!(*state, State::Recording(_))
        };

        if is_recording {
            self.dictate_stop().await
        } else {
            self.dictate_start().await
        }
    }

    async fn enter_busy(&self, label: String) -> Result<BusySession> {
        let mut state = self.state.lock().await;
        if !matches!(*state, State::Idle) {
            bail!("papagaia is already busy");
        }
        let cancel = CancelToken::new();
        let overlay_epoch = self.next_overlay_epoch();
        *state = State::Busy {
            label,
            cancel: cancel.clone(),
            overlay_epoch,
        };
        self.busy.store(true, Ordering::SeqCst);
        Ok(BusySession {
            cancel,
            overlay_epoch,
        })
    }

    async fn leave_busy(&self, overlay_epoch: u64) {
        let mut state = self.state.lock().await;
        if matches!(
            &*state,
            State::Busy {
                overlay_epoch: current,
                ..
            } if *current == overlay_epoch
        ) {
            *state = State::Idle;
            self.busy.store(false, Ordering::SeqCst);
        }
    }

    async fn finish_error(&self, overlay_epoch: u64, cancel: &CancelToken, error: &anyhow::Error) {
        if cancel.is_cancelled() {
            if self.is_current_overlay_epoch(overlay_epoch) {
                self.overlay.send(OverlayMessage::Hidden).await;
            }
        } else {
            self.flash_result(overlay_epoch, false, error.to_string())
                .await;
        }
    }

    async fn flash_result(&self, overlay_epoch: u64, ok: bool, message: impl Into<String>) {
        if !self.is_current_overlay_epoch(overlay_epoch) {
            return;
        }
        let message = message.into();
        self.overlay
            .send(OverlayMessage::Result {
                ok,
                message: message.clone(),
            })
            .await;
        let duration = if ok { RESULT_FLASH_MS } else { ERROR_FLASH_MS };
        sleep(Duration::from_millis(duration)).await;
        if self.is_current_overlay_epoch(overlay_epoch) {
            self.overlay.send(OverlayMessage::Hidden).await;
        }
    }

    /// Briefly show a neutral notice for an expected non-result, then hide.
    async fn flash_notice(&self, overlay_epoch: u64, message: impl Into<String>) {
        if !self.is_current_overlay_epoch(overlay_epoch) {
            return;
        }
        self.overlay
            .send(OverlayMessage::Notice {
                message: message.into(),
            })
            .await;
        sleep(Duration::from_millis(NOTICE_FLASH_MS)).await;
        if self.is_current_overlay_epoch(overlay_epoch) {
            self.overlay.send(OverlayMessage::Hidden).await;
        }
    }

    fn next_overlay_epoch(&self) -> u64 {
        self.overlay_epoch.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn is_current_overlay_epoch(&self, overlay_epoch: u64) -> bool {
        self.overlay_epoch.load(Ordering::Acquire) == overlay_epoch
    }
}

fn request_label(request: &ClientRequest) -> &'static str {
    match request {
        ClientRequest::Status => "status",
        ClientRequest::Transform { .. } => "transform",
        ClientRequest::TransformRaw { .. } => "transform-raw",
        ClientRequest::Pick => "pick",
        ClientRequest::DictateStart => "dictate-start",
        ClientRequest::DictateStop => "dictate-stop",
        ClientRequest::DictateToggle => "dictate-toggle",
        ClientRequest::Cancel => "cancel",
    }
}

/// Spawn the picker overlay, feed it the prompt entries, and read back the user's
/// choice. Async process I/O keeps the current-thread runtime responsive. Returns
/// `None` when the user dismisses the picker without choosing.
async fn run_picker(entries_json: &str) -> Result<Option<PickerResult>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut child = tokio::process::Command::new(overlay_program())
        .arg("--pick")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("failed to launch the prompt picker")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(entries_json.as_bytes()).await?;
        // Dropping stdin closes the pipe so the picker's stdin read reaches EOF.
    }

    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        stdout.read_to_string(&mut output).await?;
    }
    let _ = child.wait().await;

    let output = output.trim();
    if output.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(output)
        .map(Some)
        .context("failed to parse picker result")
}

/// Capture the current selection for a transform. The daemon always grabs it
/// itself (copy probe via the clipboard). If the template has no `{{text}}`
/// placeholder the selection is optional — a failed grab just yields `None` and
/// the template runs as-is; if it *does* need text, a failed grab is an error.
async fn resolve_selected_text(
    tools: &papagaia_core::ToolConfig,
    template: &str,
    cancel: &CancelToken,
) -> Result<Option<String>> {
    let needs_selection = template_needs_selection(template);
    match clipboard::capture_selection(tools, cancel).await {
        Ok(text) => Ok(Some(text)),
        Err(_) if !needs_selection => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use papagaia_core::ToolConfig;

    use crate::cancel::CancelToken;

    use super::resolve_selected_text;

    #[tokio::test]
    async fn raw_prompt_without_placeholder_still_attempts_optional_selection_capture() {
        // When clipboard capture fails, raw prompt without placeholder gracefully returns None
        let selected = resolve_selected_text(
            &ToolConfig {
                read_clipboard_command: vec!["false".into()],
                write_clipboard_command: vec!["false".into()],
                copy_command: vec!["false".into()],
                paste_command: vec!["false".into()],
                clipboard_settle_ms: 0,
            },
            "say hello",
            &CancelToken::new(),
        )
        .await
        .expect("raw prompt without placeholder should gracefully handle failed capture");

        assert_eq!(selected, None);
    }
}
