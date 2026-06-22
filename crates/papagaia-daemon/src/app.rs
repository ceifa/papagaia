use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use anyhow::{Result, bail};
use papagaia_core::{
    ClientRequest, ClientResponse, Config, OverlayMessage, PromptConfig, template_needs_selection,
};
use tokio::{
    sync::{Mutex, mpsc},
    task::JoinHandle,
    time::{Duration, sleep},
};

use crate::{
    cancel::CancelToken, clipboard, dictation::MAX_RECORDING_SECS, dictation::Recorder, llm,
    overlay::OverlayHandle, stream::stream_prompt_output,
};

macro_rules! log {
    ($config:expr, $($arg:tt)*) => {
        if $config.logging {
            eprintln!($($arg)*);
        }
    };
}

/// Recordings shorter than this are treated as accidental key taps and dropped
/// rather than sent to whisper (which tends to hallucinate on near-silence).
const MIN_RECORDING_SECS: f64 = 2.0;

/// After releasing the overlay's exclusive keyboard grab, wait this long for
/// the compositor to return focus to the target window before driving it with
/// wtype/wl-paste — otherwise the paste shortcut lands on the overlay.
const FOCUS_RETURN_SETTLE_MS: u64 = 80;

/// How long the overlay lingers on a success vs. error result before hiding.
const RESULT_FLASH_MS: u64 = 900;
const ERROR_FLASH_MS: u64 = 3000;
/// Expected non-results (too-short tap, no speech) dismiss quickly and quietly.
const NOTICE_FLASH_MS: u64 = 1300;

/// A non-error outcome of a dictation: the user did something expected — a tap
/// too short to transcribe, or audio with no speech. It is surfaced as a neutral
/// overlay notice instead of the alarming red error flash, and reported to the
/// client as a successful (if uneventful) call.
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
    /// Window-title context captured concurrently with recording. Awaited only
    /// when recording stops, so fetching it never delays the mic from capturing
    /// the user's first words.
    context: JoinHandle<DictationContext>,
    overlay_epoch: u64,
    /// Forwards mic-level samples to the overlay. Aborted on every transition
    /// out of `Recording` so trailing samples (the capture thread can push one
    /// more RMS after the stop flag is set) don't land in the overlay channel
    /// after we've already sent the next state's message.
    level_forwarder: JoinHandle<()>,
    /// Fires after `MAX_RECORDING_SECS` to auto-stop a runaway recording.
    /// Aborted when the recording ends normally so a finished session doesn't
    /// leave an hour-long timer parked in the runtime.
    auto_stop: JoinHandle<()>,
}

#[derive(Default)]
struct DictationContext {
    window_title: String,
}

struct BusySession {
    cancel: CancelToken,
    overlay_epoch: u64,
}

impl DictationContext {
    fn render_context_block(&self) -> String {
        if !self.window_title.is_empty() {
            format!("Target application: {}", self.window_title)
        } else {
            String::new()
        }
    }
}

impl App {
    pub async fn new(config: Config) -> Result<Self> {
        let overlay = OverlayHandle::spawn(config.overlay.enabled)?;
        Ok(Self {
            config: Arc::new(config),
            overlay,
            state: Mutex::new(State::Idle),
            overlay_epoch: AtomicU64::new(0),
        })
    }

    fn config(&self) -> Arc<Config> {
        self.config.clone()
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
            ClientRequest::Transform {
                prompt,
                selected_text,
                preserve_selection,
            } => {
                self.transform(&prompt, selected_text, preserve_selection)
                    .await
            }
            ClientRequest::TransformRaw {
                template,
                selected_text,
                preserve_selection,
                stream_output,
            } => {
                self.transform_raw(&template, selected_text, preserve_selection, stream_output)
                    .await
            }
            ClientRequest::DictateStart => self.dictate_start().await,
            ClientRequest::DictateStop => self.dictate_stop().await,
            ClientRequest::DictateToggle => self.dictate_toggle().await,
            ClientRequest::Cancel => self.cancel().await,
        }
    }

    async fn transform(
        &self,
        prompt_name: &str,
        selected_text: Option<String>,
        preserve_selection: bool,
    ) -> Result<ClientResponse> {
        let config = self.config();
        let prompt = config.prompt(prompt_name)?.clone();
        let label = format!("running prompt '{prompt_name}'");
        let success_label = prompt_name.to_string();
        self.run_transform(
            prompt,
            label,
            &success_label,
            selected_text,
            preserve_selection,
        )
        .await
    }

    async fn transform_raw(
        &self,
        template: &str,
        selected_text: Option<String>,
        preserve_selection: bool,
        stream_output: bool,
    ) -> Result<ClientResponse> {
        let prompt = PromptConfig {
            name: "ad-hoc".into(),
            template: template.into(),
            stream_output,
        };
        self.run_transform(
            prompt,
            "running ad-hoc prompt".into(),
            "engine output",
            selected_text,
            preserve_selection,
        )
        .await
    }

    async fn run_transform(
        &self,
        prompt: PromptConfig,
        busy_label: String,
        success_label: &str,
        selected_text: Option<String>,
        preserve_selection: bool,
    ) -> Result<ClientResponse> {
        let session = self.enter_busy(busy_label).await?;
        let outcome = self
            .run_transform_inner(
                &prompt,
                selected_text.as_deref(),
                preserve_selection,
                &session.cancel,
            )
            .await;
        self.leave_busy(session.overlay_epoch).await;

        let config = self.config();
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

    async fn run_transform_inner(
        &self,
        prompt: &PromptConfig,
        selected_text: Option<&str>,
        preserve_selection: bool,
        cancel: &CancelToken,
    ) -> Result<(String, bool)> {
        let label = format!("Running {}", prompt.name);

        // Capture phase: wtype needs the original window focused — no grab.
        self.overlay
            .send(OverlayMessage::Busy {
                label: label.clone(),
                grab_keyboard: false,
            })
            .await;

        let config = self.config();
        let selected = resolve_selected_text(
            &config.tools,
            &prompt.template,
            selected_text,
            preserve_selection,
            true,
            cancel,
        )
        .await?;

        // Engine phase: non-streaming prompts can grab the keyboard so Esc
        // cancels the engine. Streaming prompts must keep focus in the target
        // window because output is typed there incrementally.
        self.overlay
            .send(OverlayMessage::Busy {
                label: label.clone(),
                grab_keyboard: !prompt.stream_output,
            })
            .await;

        let engine = config.engine.clone();
        let rendered_prompt = match &selected {
            Some(text) => prompt.render(text),
            None => prompt.template.clone(),
        };
        log!(
            config,
            "[transform] stream={} selected={}",
            prompt.stream_output,
            selected.is_some()
        );
        log!(config, "[transform] rendered prompt: {rendered_prompt}");
        let cleaned = if prompt.stream_output {
            let (emitted, result) =
                stream_prompt_output(&config.tools, &engine, &rendered_prompt, cancel).await;
            result?;
            emitted
        } else {
            let raw = llm::run_engine(&engine, &rendered_prompt, cancel).await?;
            log!(config, "[transform] engine output: {raw}");
            let cleaned = prompt.clean_output(&raw);

            // Paste phase: release the grab so focus returns to the target window
            // before wtype fires the paste shortcut.
            self.overlay
                .send(OverlayMessage::Busy {
                    label,
                    grab_keyboard: false,
                })
                .await;
            sleep(Duration::from_millis(FOCUS_RETURN_SETTLE_MS)).await;

            clipboard::paste_text(&config.tools, &cleaned, cancel).await?;
            cleaned
        };
        log!(config, "[transform] final output: {cleaned}");
        Ok((cleaned, selected.is_some()))
    }

    async fn dictate_start(self: &Arc<Self>) -> Result<ClientResponse> {
        let config = self.config();

        let overlay_epoch;
        {
            let mut state = self.state.lock().await;
            if !matches!(*state, State::Idle) {
                bail!("papagaia is already busy");
            }
            overlay_epoch = self.next_overlay_epoch();

            // Start capturing audio immediately. The mic must not wait on the
            // compositor subprocess that fetches window-title context — that
            // context is only consumed when recording stops, so we kick it off
            // concurrently and await it there instead.
            let (level_tx, mut level_rx) = mpsc::unbounded_channel();
            let recorder = Recorder::start(level_tx)?;
            let overlay = self.overlay.clone();
            let level_forwarder = tokio::spawn(async move {
                while let Some(level) = level_rx.recv().await {
                    overlay
                        .send(OverlayMessage::Recording {
                            level,
                            transcript: None,
                        })
                        .await;
                }
            });

            let context = if config.dictation.context_awareness {
                let app = self.clone();
                let config = config.clone();
                tokio::spawn(async move {
                    let ctx = app.capture_dictation_context(&config).await;
                    if !ctx.window_title.is_empty() {
                        log!(config, "[dictate] context: {}", ctx.window_title);
                    }
                    ctx
                })
            } else {
                tokio::spawn(async { DictationContext::default() })
            };

            // Auto-cancel recording after the maximum duration to prevent
            // runaway memory usage and enormous WAV files. The handle is kept on
            // the session so a normal stop aborts it instead of leaving an
            // hour-long timer parked until it fires on a stale epoch.
            let app = self.clone();
            let auto_stop = tokio::spawn(async move {
                sleep(Duration::from_secs(MAX_RECORDING_SECS)).await;
                app.auto_stop_recording(overlay_epoch).await;
            });

            *state = State::Recording(RecordingSession {
                recorder,
                context,
                overlay_epoch,
                level_forwarder,
                auto_stop,
            });
        }

        // Show the recording HUD right away so feedback is instant.
        self.overlay
            .send(OverlayMessage::Recording {
                level: 0.0,
                transcript: None,
            })
            .await;

        Ok(ClientResponse::ok("dictation started"))
    }

    async fn dictate_stop(&self) -> Result<ClientResponse> {
        let cancel = CancelToken::new();
        let (recorder, context, overlay_epoch) = {
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
            (session.recorder, session.context, overlay_epoch)
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
        let outcome = async {
            // `recorder.finish()` joins the capture thread and writes the WAV to
            // disk — both blocking. On the current-thread runtime that would
            // stall every other task (overlay forwarder, signals, new
            // connections), so run it on the blocking pool instead.
            let (audio_path, duration_secs) =
                tokio::task::spawn_blocking(move || recorder.finish()).await??;
            if duration_secs < MIN_RECORDING_SECS {
                maybe_remove_audio(&config, &audio_path);
                return Err(
                    SoftOutcome(format!("Too short ({duration_secs:.1}s), ignored")).into(),
                );
            }
            let transcript = llm::run_whisper(&config.whisper, &audio_path, &cancel).await?;
            let cleaned = transcript.trim().to_string();
            log!(config, "[dictate] whisper transcript: {cleaned}");
            if cleaned.is_empty() {
                maybe_remove_audio(&config, &audio_path);
                return Err(SoftOutcome("No speech detected".into()).into());
            }

            // Post-process the transcript through the LLM engine if enabled.
            // If post-processing fails we fall back to pasting the raw
            // transcript and carry a warning message so the user is told the
            // post-processing step errored out.
            let (final_text, warning) = if config.dictation.post_process {
                let context = context.await.unwrap_or_default();
                let rendered = render_dictation_prompt(
                    &config.dictation.post_process_template,
                    &cleaned,
                    &context,
                );

                if config.dictation.stream_post_process {
                    overlay
                        .send(OverlayMessage::Busy {
                            label: "Processing".into(),
                            grab_keyboard: false,
                        })
                        .await;
                    let (emitted, result) =
                        stream_prompt_output(&config.tools, &config.engine, &rendered, &cancel)
                            .await;
                    match result {
                        Ok(()) => {
                            log!(config, "[dictate] post-processed (streamed): {emitted}");
                            if !emitted.is_empty() {
                                // Output already landed in the target window via
                                // the streaming engine — no type phase needed.
                                maybe_remove_audio(&config, &audio_path);
                                return Ok((emitted, None));
                            }
                            // Nothing was streamed; fall through to the type
                            // phase and paste the raw transcript.
                            (cleaned, None)
                        }
                        Err(error) => {
                            // A cancellation isn't something to recover from.
                            if cancel.is_cancelled() {
                                return Err(error);
                            }
                            // If part of the streamed output already landed in
                            // the target window a raw paste would duplicate it,
                            // so we can only fall back when nothing was emitted.
                            if !emitted.is_empty() {
                                return Err(error);
                            }
                            log!(
                                config,
                                "[dictate] post-process failed, pasting raw transcript: {error:#}"
                            );
                            // Fall through to the type phase to paste the raw
                            // transcript, carrying a warning.
                            (
                                cleaned,
                                Some(format!("Post-processing failed, pasted raw text: {error}")),
                            )
                        }
                    }
                } else {
                    overlay
                        .send(OverlayMessage::Busy {
                            label: "Processing".into(),
                            grab_keyboard: true,
                        })
                        .await;
                    match llm::run_engine(&config.engine, &rendered, &cancel).await {
                        Ok(raw) => {
                            let processed = raw.trim().to_string();
                            log!(config, "[dictate] post-processed: {processed}");
                            if processed.is_empty() {
                                (cleaned, None)
                            } else {
                                (processed, None)
                            }
                        }
                        Err(error) => {
                            // A cancellation isn't something to recover from.
                            if cancel.is_cancelled() {
                                return Err(error);
                            }
                            log!(
                                config,
                                "[dictate] post-process failed, pasting raw transcript: {error:#}"
                            );
                            (
                                cleaned,
                                Some(format!("Post-processing failed, pasted raw text: {error}")),
                            )
                        }
                    }
                }
            } else {
                (cleaned, None)
            };

            // Type phase: release the grab so focus returns to the target
            // window before wtype types the transcript into it.
            overlay
                .send(OverlayMessage::Busy {
                    label: "Typing".into(),
                    grab_keyboard: false,
                })
                .await;
            sleep(Duration::from_millis(FOCUS_RETURN_SETTLE_MS)).await;

            clipboard::paste_text(&config.tools, &final_text, &cancel).await?;
            maybe_remove_audio(&config, &audio_path);
            Ok::<(String, Option<String>), anyhow::Error>((final_text, warning))
        }
        .await;

        self.leave_busy(overlay_epoch).await;

        match outcome {
            Ok((text, warning)) => {
                log!(config, "[dictate] inserted: {text}");
                match warning {
                    Some(message) => {
                        log!(config, "[dictate] inserted with warning: {message}");
                        self.flash_result(overlay_epoch, false, message).await;
                    }
                    None => {
                        self.flash_result(overlay_epoch, true, "Dictation inserted")
                            .await;
                    }
                }
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
                // Abort the level forwarder before doing anything else: the
                // capture thread can push one final RMS sample between when
                // we set the stop flag (inside Recorder::drop) and when its
                // current `readi` call returns. Without this abort, that
                // trailing Recording message lands in the overlay channel
                // after our Hidden, and the overlay sticks at "Listening…".
                session.context.abort();
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
                // Leave the Busy state in place so the in-flight operation can
                // unwind normally (leave_busy + flash_result). We just flip the
                // cancellation flag — the subprocess wait loop will notice and
                // kill the child.
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
                        session.context.abort();
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

    async fn capture_dictation_context(&self, config: &Config) -> DictationContext {
        let cancel = CancelToken::new();

        let window_title = if !config.dictation.window_title_command.is_empty() {
            match clipboard::run_command(&config.dictation.window_title_command, None, &cancel)
                .await
            {
                Ok(output) => {
                    let raw = String::from_utf8_lossy(&output.stdout).to_string();
                    extract_window_title(&raw)
                }
                Err(_) => String::new(),
            }
        } else {
            String::new()
        };

        DictationContext { window_title }
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

fn render_dictation_prompt(template: &str, transcript: &str, context: &DictationContext) -> String {
    template
        .replace("{{text}}", transcript)
        .replace("{{context}}", &context.render_context_block())
}

fn maybe_remove_audio(config: &Config, path: &std::path::Path) {
    if config.dictation.keep_audio_files {
        log!(config, "[dictate] keeping audio file: {}", path.display());
    } else {
        std::fs::remove_file(path).ok();
    }
}

fn request_label(request: &ClientRequest) -> &'static str {
    match request {
        ClientRequest::Status => "status",
        ClientRequest::Transform { .. } => "transform",
        ClientRequest::TransformRaw { .. } => "transform-raw",
        ClientRequest::DictateStart => "dictate-start",
        ClientRequest::DictateStop => "dictate-stop",
        ClientRequest::DictateToggle => "dictate-toggle",
        ClientRequest::Cancel => "cancel",
    }
}

/// Extract a human-readable window title from the output of a compositor command.
/// Handles JSON output from niri (`niri msg -j focused-window`) and hyprland
/// (`hyprctl activewindow -j`), falling back to raw text.
fn extract_window_title(output: &str) -> String {
    let trimmed = output.trim();
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) {
        let mut parts = Vec::new();
        if let Some(title) = json.get("title").and_then(|v| v.as_str()) {
            parts.push(title.to_string());
        }
        if let Some(app_id) = json
            .get("app_id")
            .or_else(|| json.get("class"))
            .and_then(|v| v.as_str())
        {
            parts.push(format!("({app_id})"));
        }
        if !parts.is_empty() {
            return parts.join(" ");
        }
    }
    trimmed.to_string()
}

async fn resolve_selected_text(
    tools: &papagaia_core::ToolConfig,
    template: &str,
    selected_text: Option<&str>,
    preserve_selection: bool,
    capture_optional_selection: bool,
    cancel: &CancelToken,
) -> Result<Option<String>> {
    let needs_selection = template_needs_selection(template);

    if preserve_selection {
        return match selected_text {
            Some(text) => Ok(Some(text.to_string())),
            None if needs_selection => bail!("no text was selected before opening the picker"),
            None => Ok(None),
        };
    }

    if !needs_selection && !capture_optional_selection {
        return Ok(None);
    }

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

    use super::{
        DictationContext, extract_window_title, render_dictation_prompt, resolve_selected_text,
    };

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
            None,
            false,
            true,
            &CancelToken::new(),
        )
        .await
        .expect("raw prompt without placeholder should gracefully handle failed capture");

        assert_eq!(selected, None);
    }
    #[test]
    fn extract_window_title_parses_niri_json() {
        let json = r#"{"title":"main.rs — papagaia","app_id":"org.wezfurlong.wezterm"}"#;
        assert_eq!(
            extract_window_title(json),
            "main.rs — papagaia (org.wezfurlong.wezterm)"
        );
    }

    #[test]
    fn extract_window_title_parses_hyprland_json() {
        let json = r#"{"title":"Firefox","class":"firefox"}"#;
        assert_eq!(extract_window_title(json), "Firefox (firefox)");
    }

    #[test]
    fn extract_window_title_falls_back_to_raw_text() {
        assert_eq!(
            extract_window_title("  My Window Title  "),
            "My Window Title"
        );
    }

    #[test]
    fn render_dictation_prompt_replaces_placeholders() {
        let template = "Context: {{context}}\nText: {{text}}";
        let context = DictationContext {
            window_title: "VS Code (code)".into(),
        };
        let result = render_dictation_prompt(template, "hello world", &context);
        assert!(result.contains("hello world"));
        assert!(result.contains("VS Code (code)"));
    }

    #[test]
    fn render_dictation_prompt_empty_context() {
        let template = "{{context}}\n{{text}}";
        let context = DictationContext::default();
        let result = render_dictation_prompt(template, "hello", &context);
        assert_eq!(result, "\nhello");
    }

    #[test]
    fn dictation_context_renders_window_title() {
        let context = DictationContext {
            window_title: "Firefox".into(),
        };
        let block = context.render_context_block();
        assert_eq!(block, "Target application: Firefox");
    }

    #[test]
    fn dictation_context_empty_renders_empty() {
        let context = DictationContext::default();
        assert_eq!(context.render_context_block(), "");
    }
}
