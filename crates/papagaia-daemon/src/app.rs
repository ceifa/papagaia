use std::sync::{
    Arc, Mutex as StdMutex, RwLock,
    atomic::{AtomicU64, Ordering},
};

use anyhow::{Result, bail};
use papagaia_core::{
    ClientRequest, ClientResponse, Config, OverlayMessage, PromptConfig, validate_prompt_options,
};
use tokio::{
    sync::{Mutex, mpsc},
    time::{Duration, sleep},
};

use crate::{
    cancel::CancelToken, clipboard, dictation::Recorder, dictation::MAX_RECORDING_SECS, llm,
    overlay::OverlayHandle,
};

macro_rules! log {
    ($config:expr, $($arg:tt)*) => {
        if $config.logging {
            eprintln!($($arg)*);
        }
    };
}

pub struct App {
    config: RwLock<Arc<Config>>,
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
    context: DictationContext,
    overlay_epoch: u64,
}

#[derive(Default)]
struct DictationContext {
    window_title: String,
}

struct BusySession {
    cancel: CancelToken,
    overlay_epoch: u64,
}

struct RawPromptOptions {
    strip_markdown_fences: bool,
    trim_whitespace: bool,
    stream_output: bool,
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
            config: RwLock::new(Arc::new(config)),
            overlay,
            state: Mutex::new(State::Idle),
            overlay_epoch: AtomicU64::new(0),
        })
    }

    fn config(&self) -> Arc<Config> {
        self.config.read().expect("config lock poisoned").clone()
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
                strip_markdown_fences,
                trim_whitespace,
                stream_output,
            } => {
                self.transform_raw(
                    &template,
                    selected_text,
                    preserve_selection,
                    strip_markdown_fences,
                    trim_whitespace,
                    stream_output,
                )
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
        let session = self
            .enter_busy(format!("running prompt '{prompt_name}'"))
            .await?;
        let outcome = self
            .transform_inner(
                prompt_name,
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
                    format!("Replaced selection with {prompt_name}")
                } else {
                    format!("Pasted {prompt_name} output")
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

    async fn transform_inner(
        &self,
        prompt_name: &str,
        selected_text: Option<&str>,
        preserve_selection: bool,
        cancel: &CancelToken,
    ) -> Result<(String, bool)> {
        let label = format!("Running {prompt_name}");

        // Capture phase: wtype needs the original window focused — no grab.
        self.overlay
            .send(OverlayMessage::Busy {
                label: label.clone(),
                grab_keyboard: false,
            })
            .await;

        let config = self.config();
        let prompt = config.prompt(prompt_name)?.clone();
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

        let engine = config.engine().clone();
        let rendered_prompt = match &selected {
            Some(text) => prompt.render(text),
            None => prompt.template.clone(),
        };
        log!(
            config,
            "[transform] stream={} strip_fences={} trim_ws={} selected={}",
            prompt.stream_output,
            prompt.strip_markdown_fences,
            prompt.trim_whitespace,
            selected.is_some()
        );
        log!(config, "[transform] rendered prompt: {rendered_prompt}");
        let cleaned = if prompt.stream_output {
            stream_prompt_output(
                &self.overlay,
                &config.tools,
                &prompt,
                &engine,
                &rendered_prompt,
                cancel,
            )
            .await?
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
            sleep(Duration::from_millis(80)).await;

            clipboard::paste_text(&config.tools, &cleaned, cancel).await?;
            cleaned
        };
        log!(config, "[transform] final output: {cleaned}");
        Ok((cleaned, selected.is_some()))
    }

    async fn transform_raw(
        &self,
        template: &str,
        selected_text: Option<String>,
        preserve_selection: bool,
        strip_markdown_fences: bool,
        trim_whitespace: bool,
        stream_output: bool,
    ) -> Result<ClientResponse> {
        let session = self.enter_busy("running ad-hoc prompt".into()).await?;
        let options = RawPromptOptions {
            strip_markdown_fences,
            trim_whitespace,
            stream_output,
        };
        let outcome = self
            .transform_raw_inner(
                template,
                selected_text.as_deref(),
                preserve_selection,
                options,
                &session.cancel,
            )
            .await;
        self.leave_busy(session.overlay_epoch).await;

        let config = self.config();
        match outcome {
            Ok((text, had_selection)) => {
                let msg = if had_selection {
                    "Replaced selection with engine output"
                } else {
                    "Pasted engine output"
                };
                log!(config, "[transform-raw] {msg}");
                self.flash_result(session.overlay_epoch, true, msg).await;
                Ok(ClientResponse::with_text("transform complete", text))
            }
            Err(error) => {
                log!(config, "[transform-raw] error: {error:#}");
                self.finish_error(session.overlay_epoch, &session.cancel, &error)
                    .await;
                Err(error)
            }
        }
    }

    async fn transform_raw_inner(
        &self,
        template: &str,
        selected_text: Option<&str>,
        preserve_selection: bool,
        options: RawPromptOptions,
        cancel: &CancelToken,
    ) -> Result<(String, bool)> {
        // Capture phase: wtype needs the original window focused — no grab.
        self.overlay
            .send(OverlayMessage::Busy {
                label: "Running prompt".into(),
                grab_keyboard: false,
            })
            .await;

        let config = self.config();
        let selected = resolve_selected_text(
            &config.tools,
            template,
            selected_text,
            preserve_selection,
            true,
            cancel,
        )
        .await?;

        let prompt = PromptConfig {
            name: "ad-hoc".into(),
            template: template.into(),
            strip_markdown_fences: options.strip_markdown_fences,
            trim_whitespace: options.trim_whitespace,
            stream_output: options.stream_output,
        };
        validate_prompt_options(&prompt)?;

        // Engine phase: non-streaming prompts can grab the keyboard so Esc
        // cancels the engine. Streaming prompts must keep focus in the target
        // window because output is typed there incrementally.
        self.overlay
            .send(OverlayMessage::Busy {
                label: "Running prompt".into(),
                grab_keyboard: !prompt.stream_output,
            })
            .await;

        let engine = config.engine().clone();
        let rendered_prompt = match &selected {
            Some(text) => prompt.render(text),
            None => template.to_string(),
        };
        log!(
            config,
            "[transform-raw] stream={} strip_fences={} trim_ws={} selected={}",
            options.stream_output,
            options.strip_markdown_fences,
            options.trim_whitespace,
            selected.is_some()
        );
        log!(config, "[transform-raw] rendered prompt: {rendered_prompt}");
        let cleaned = if prompt.stream_output {
            stream_prompt_output(
                &self.overlay,
                &config.tools,
                &prompt,
                &engine,
                &rendered_prompt,
                cancel,
            )
            .await?
        } else {
            let raw = llm::run_engine(&engine, &rendered_prompt, cancel).await?;
            log!(config, "[transform-raw] engine output: {raw}");
            let cleaned = prompt.clean_output(&raw);

            // Paste phase: release the grab so focus returns to the target window
            // before wtype fires the paste shortcut.
            self.overlay
                .send(OverlayMessage::Busy {
                    label: "Running prompt".into(),
                    grab_keyboard: false,
                })
                .await;
            sleep(Duration::from_millis(80)).await;

            clipboard::paste_text(&config.tools, &cleaned, cancel).await?;
            cleaned
        };
        log!(config, "[transform-raw] final output: {cleaned}");
        Ok((cleaned, selected.is_some()))
    }

    async fn dictate_start(self: &Arc<Self>) -> Result<ClientResponse> {
        let config = self.config();

        // Capture context before starting recording (while the target window
        // still has focus and the clipboard reflects the user's recent activity).
        let context = if config.dictation.context_awareness {
            let ctx = self.capture_dictation_context(&config).await;
            if !ctx.window_title.is_empty() {
                log!(config, "[dictate] context: {}", ctx.window_title);
            }
            ctx
        } else {
            DictationContext::default()
        };

        let overlay_epoch;
        {
            let mut state = self.state.lock().await;
            if !matches!(*state, State::Idle) {
                bail!("papagaia is already busy");
            }
            overlay_epoch = self.next_overlay_epoch();

            let (level_tx, mut level_rx) = mpsc::unbounded_channel();
            let recorder = Recorder::start(level_tx)?;
            let overlay = self.overlay.clone();
            tokio::spawn(async move {
                while let Some(level) = level_rx.recv().await {
                    overlay
                        .send(OverlayMessage::Recording {
                            level,
                            transcript: None,
                        })
                        .await;
                }
            });

            *state = State::Recording(RecordingSession {
                recorder,
                context,
                overlay_epoch,
            });
        }

        // Auto-cancel recording after the maximum duration to prevent
        // runaway memory usage and enormous WAV files.
        let app = self.clone();
        tokio::spawn(async move {
            sleep(Duration::from_secs(MAX_RECORDING_SECS)).await;
            app.auto_stop_recording(overlay_epoch).await;
        });

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
            match std::mem::replace(
                &mut *state,
                State::Busy {
                    label: "transcribing".into(),
                    cancel: cancel.clone(),
                    overlay_epoch: 0,
                },
            ) {
                State::Recording(session) => {
                    let overlay_epoch = session.overlay_epoch;
                    *state = State::Busy {
                        label: "transcribing".into(),
                        cancel: cancel.clone(),
                        overlay_epoch,
                    };
                    (session.recorder, session.context, overlay_epoch)
                }
                other => {
                    *state = other;
                    bail!("papagaia is not recording");
                }
            }
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
            let (audio_path, duration_secs) = recorder.finish()?;
            if duration_secs < 2.0 {
                maybe_remove_audio(&config, &audio_path);
                bail!("recording too short ({duration_secs:.1}s), ignoring");
            }
            let transcript = llm::run_whisper(&config.whisper, &audio_path, &cancel).await?;
            let cleaned = transcript.trim().to_string();
            log!(config, "[dictate] whisper transcript: {cleaned}");
            if cleaned.is_empty() {
                bail!("whisper returned an empty transcript");
            }

            // Post-process the transcript through the LLM engine if enabled.
            let final_text = if config.dictation.post_process {
                let rendered = render_dictation_prompt(
                    &config.dictation.post_process_template,
                    &cleaned,
                    &context,
                );

                if config.dictation.stream_post_process {
                    let prompt_cfg = PromptConfig {
                        name: "dictation-post-process".into(),
                        template: String::new(),
                        strip_markdown_fences: false,
                        trim_whitespace: true,
                        stream_output: true,
                    };
                    overlay
                        .send(OverlayMessage::Busy {
                            label: "Processing".into(),
                            grab_keyboard: false,
                        })
                        .await;
                    let processed = stream_prompt_output(
                        &overlay,
                        &config.tools,
                        &prompt_cfg,
                        config.engine(),
                        &rendered,
                        &cancel,
                    )
                    .await?;
                    log!(config, "[dictate] post-processed (streamed): {processed}");
                    if processed.is_empty() {
                        overlay
                            .send(OverlayMessage::Busy {
                                label: "Typing".into(),
                                grab_keyboard: false,
                            })
                            .await;
                        sleep(Duration::from_millis(80)).await;
                        clipboard::paste_text(&config.tools, &cleaned, &cancel).await?;
                        maybe_remove_audio(&config, &audio_path);
                        return Ok(cleaned);
                    }

                    maybe_remove_audio(&config, &audio_path);
                    return Ok(processed);
                }

                overlay
                    .send(OverlayMessage::Busy {
                        label: "Processing".into(),
                        grab_keyboard: true,
                    })
                    .await;
                let raw = llm::run_engine(config.engine(), &rendered, &cancel).await?;
                let processed = raw.trim().to_string();
                log!(config, "[dictate] post-processed: {processed}");
                if processed.is_empty() {
                    cleaned
                } else {
                    processed
                }
            } else {
                cleaned
            };

            // Type phase: release the grab so focus returns to the target
            // window before wtype types the transcript into it.
            overlay
                .send(OverlayMessage::Busy {
                    label: "Typing".into(),
                    grab_keyboard: false,
                })
                .await;
            sleep(Duration::from_millis(80)).await;

            clipboard::paste_text(&config.tools, &final_text, &cancel).await?;
            maybe_remove_audio(&config, &audio_path);
            Ok::<String, anyhow::Error>(final_text)
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
        let duration = if ok { 900 } else { 3000 };
        sleep(Duration::from_millis(duration)).await;
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

fn template_needs_selection(template: &str) -> bool {
    template.contains("{{text}}") || template.contains("{{selection}}")
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

/// Extract a human-readable window title from the output of a compositor command.
/// Handles JSON output from niri (`niri msg -j focused-window`) and hyprland
/// (`hyprctl activewindow -j`), falling back to raw text.
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

async fn stream_prompt_output(
    overlay: &OverlayHandle,
    tools: &papagaia_core::ToolConfig,
    prompt: &PromptConfig,
    engine: &papagaia_core::EngineConfig,
    rendered_prompt: &str,
    cancel: &CancelToken,
) -> Result<String> {
    let state = Arc::new(StdMutex::new(StreamOutputState::new(
        prompt.trim_whitespace,
    )));
    let overlay_for_tail = overlay.clone();
    let overlay_for_chunks = overlay.clone();
    let tools_for_tail = tools.clone();
    let cancel_for_tail = cancel.clone();
    let callback_tools = tools.clone();
    let callback_cancel = cancel.clone();
    let callback_state = state.clone();
    let stream_started = Arc::new(StdMutex::new(false));
    let callback_started = stream_started.clone();

    llm::run_engine_streaming(engine, rendered_prompt, &cancel_for_tail, move |chunk| {
        let overlay = overlay_for_chunks.clone();
        let tools = callback_tools.clone();
        let cancel = callback_cancel.clone();
        let callback_state = callback_state.clone();
        let callback_started = callback_started.clone();
        async move {
            let flushed = {
                let mut state = callback_state
                    .lock()
                    .expect("streaming output state lock poisoned");
                state.push(&chunk)
            };
            if !flushed.is_empty() {
                let first_flush = {
                    let mut started = callback_started
                        .lock()
                        .expect("stream started lock poisoned");
                    if *started {
                        false
                    } else {
                        *started = true;
                        true
                    }
                };
                if first_flush {
                    overlay.send(OverlayMessage::Hidden).await;
                    sleep(Duration::from_millis(80)).await;
                }
                // Clipboard paste (not direct wtype) — wtype relies on
                // virtual-keyboard keysyms which don't cover codepoints above
                // the BMP, so emojis and other astral-plane chars get dropped
                // or substituted. Clipboard paste round-trips raw UTF-8.
                clipboard::paste_text(&tools, &flushed, &cancel).await?;
                sleep(Duration::from_millis(28)).await;
            }
            Ok(())
        }
    })
    .await?;

    let (tail, emitted) = {
        let mut state = state.lock().expect("streaming output state lock poisoned");
        let tail = state.finish();
        let emitted = state.emitted.clone();
        (tail, emitted)
    };

    if !tail.is_empty() {
        let first_flush = {
            let mut started = stream_started.lock().expect("stream started lock poisoned");
            if *started {
                false
            } else {
                *started = true;
                true
            }
        };
        if first_flush {
            overlay_for_tail.send(OverlayMessage::Hidden).await;
            sleep(Duration::from_millis(80)).await;
        }
        clipboard::paste_text(&tools_for_tail, &tail, &cancel_for_tail).await?;
    }

    Ok(emitted)
}

struct StreamOutputState {
    trim_whitespace: bool,
    saw_non_whitespace: bool,
    pending_whitespace: String,
    escape_state: EscapeState,
    observed_sanitized: String,
    pending_flush: String,
    emitted: String,
}

impl StreamOutputState {
    fn new(trim_whitespace: bool) -> Self {
        Self {
            trim_whitespace,
            saw_non_whitespace: false,
            pending_whitespace: String::new(),
            escape_state: EscapeState::None,
            observed_sanitized: String::new(),
            pending_flush: String::new(),
            emitted: String::new(),
        }
    }

    fn push(&mut self, chunk: &str) -> String {
        let sanitized = self.sanitize_chunk(chunk);
        let raw_delta = compute_stream_delta(&self.observed_sanitized, &sanitized);
        if raw_delta.is_empty() {
            return String::new();
        }
        self.observed_sanitized.push_str(&raw_delta);

        let cleaned = if self.trim_whitespace {
            self.trimmed_chunk(&raw_delta)
        } else {
            raw_delta
        };
        if cleaned.is_empty() {
            return String::new();
        }

        self.emitted.push_str(&cleaned);
        self.pending_flush.push_str(&cleaned);
        if self.should_flush() {
            return std::mem::take(&mut self.pending_flush);
        }

        String::new()
    }

    fn finish(&mut self) -> String {
        self.pending_whitespace.clear();
        std::mem::take(&mut self.pending_flush)
    }

    fn trimmed_chunk(&mut self, chunk: &str) -> String {
        let mut out = String::new();

        for ch in chunk.chars() {
            if !self.saw_non_whitespace {
                if ch.is_whitespace() {
                    continue;
                }
                self.saw_non_whitespace = true;
            }

            if ch.is_whitespace() {
                self.pending_whitespace.push(ch);
            } else {
                if !self.pending_whitespace.is_empty() {
                    out.push_str(&self.pending_whitespace);
                    self.pending_whitespace.clear();
                }
                out.push(ch);
            }
        }

        out
    }

    fn sanitize_chunk(&mut self, chunk: &str) -> String {
        let mut out = String::new();

        for ch in chunk.chars() {
            match self.escape_state {
                EscapeState::None => {}
                EscapeState::Started => {
                    self.escape_state = if ch == '[' {
                        EscapeState::Csi
                    } else {
                        EscapeState::None
                    };
                    continue;
                }
                EscapeState::Csi => {
                    if ('@'..='~').contains(&ch) {
                        self.escape_state = EscapeState::None;
                    }
                    continue;
                }
            }

            match ch {
                '\u{1b}' => {
                    self.escape_state = EscapeState::Started;
                }
                '\r' => {}
                '\u{8}' | '\u{7f}' => {
                    out.pop();
                }
                _ if ch.is_control() && ch != '\n' && ch != '\t' => {}
                _ => out.push(ch),
            }
        }

        out
    }

    fn should_flush(&self) -> bool {
        self.pending_flush.contains('\n')
            || self.pending_flush.len() >= 64
            || (self.pending_flush.len() >= 24
                && self
                    .pending_flush
                    .chars()
                    .last()
                    .is_some_and(|ch| ch.is_whitespace() || ",.;:!?)]}".contains(ch)))
    }
}

#[derive(Clone, Copy)]
enum EscapeState {
    None,
    Started,
    Csi,
}

fn compute_stream_delta(emitted: &str, chunk: &str) -> String {
    if chunk.is_empty() {
        return String::new();
    }
    if emitted.is_empty() {
        return chunk.to_string();
    }
    if emitted.starts_with(chunk) || emitted.ends_with(chunk) {
        return String::new();
    }
    if let Some(rest) = chunk.strip_prefix(emitted) {
        return rest.to_string();
    }

    let mut overlap = 0;
    for (idx, _) in chunk.char_indices().skip(1) {
        if emitted.ends_with(&chunk[..idx]) {
            overlap = idx;
        }
    }
    if emitted.ends_with(chunk) {
        overlap = chunk.len();
    }

    chunk[overlap..].to_string()
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
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use papagaia_core::{EngineConfig, PromptConfig, ToolConfig};

    use crate::{cancel::CancelToken, overlay::OverlayHandle};

    use super::{
        DictationContext, StreamOutputState, compute_stream_delta, extract_window_title,
        render_dictation_prompt, resolve_selected_text, stream_prompt_output,
    };

    #[test]
    fn streaming_trim_drops_outer_whitespace() {
        let mut state = StreamOutputState::new(true);

        assert_eq!(state.push("  hello"), "");
        assert_eq!(state.push(" world  "), "");
        assert_eq!(state.finish(), "hello world");
        assert_eq!(state.emitted, "hello world");
    }

    #[test]
    fn streaming_without_trim_preserves_whitespace() {
        let mut state = StreamOutputState::new(false);

        assert_eq!(state.push(" hi"), "");
        assert_eq!(state.push(" there "), "");
        assert_eq!(state.finish(), " hi there ");
        assert_eq!(state.emitted, " hi there ");
    }

    #[test]
    fn compute_stream_delta_handles_cumulative_chunks() {
        assert_eq!(compute_stream_delta("h", "he"), "e");
        assert_eq!(compute_stream_delta("hello", "hello world"), " world");
    }

    #[test]
    fn compute_stream_delta_handles_overlapping_chunks() {
        assert_eq!(compute_stream_delta("hello", "lo world"), " world");
        assert_eq!(compute_stream_delta("hello world", "world"), "");
    }

    #[test]
    fn streaming_state_strips_ansi_sequences() {
        let mut state = StreamOutputState::new(false);
        assert_eq!(state.push("\u{1b}[2Khello"), "");
        assert_eq!(state.finish(), "hello");
    }

    #[tokio::test]
    async fn stream_prompt_output_types_exact_delta_once() {
        let dir = make_test_dir("stream-delta");
        let clipboard_script = dir.join("clipboard.sh");
        let engine_script = dir.join("engine.sh");
        let out_path = dir.join("typed.txt");
        fs::write(&out_path, "").expect("output file should be created");

        write_executable(
            &clipboard_script,
            r#"#!/usr/bin/env bash
set -euo pipefail
cat >> "$1"
"#,
        );
        write_executable(
            &engine_script,
            r#"#!/usr/bin/env bash
set -euo pipefail
printf '\033[2K'
printf 'The quick '
sleep 0.05
printf 'The quick brown '
sleep 0.05
printf 'own fox'
"#,
        );

        let tools = fake_tools(&clipboard_script, &out_path);
        let prompt = PromptConfig {
            name: "test".into(),
            template: "{{text}}".into(),
            strip_markdown_fences: false,
            trim_whitespace: true,
            stream_output: true,
        };
        let engine = EngineConfig {
            argv: vec![engine_script.display().to_string()],
            stdin: false,
            capture_stdout: true,
        };

        let overlay = OverlayHandle::spawn(false).expect("overlay should be disabled in tests");
        let emitted = stream_prompt_output(
            &overlay,
            &tools,
            &prompt,
            &engine,
            "ignored",
            &CancelToken::new(),
        )
        .await
        .expect("streaming should succeed");
        let typed = fs::read_to_string(&out_path).expect("typed output should exist");

        assert_eq!(emitted, "The quick brown fox");
        assert_eq!(typed, "The quick brown fox");
    }

    #[tokio::test]
    async fn stream_prompt_output_handles_backspace_and_overlap() {
        let dir = make_test_dir("stream-backspace");
        let clipboard_script = dir.join("clipboard.sh");
        let engine_script = dir.join("engine.sh");
        let out_path = dir.join("typed.txt");
        fs::write(&out_path, "").expect("output file should be created");

        write_executable(
            &clipboard_script,
            r#"#!/usr/bin/env bash
set -euo pipefail
cat >> "$1"
"#,
        );
        write_executable(
            &engine_script,
            r#"#!/usr/bin/env bash
set -euo pipefail
printf 'hel'
sleep 0.05
printf 'hello '
sleep 0.05
printf 'o worl'
sleep 0.05
printf 'world!\b!'
"#,
        );

        let tools = fake_tools(&clipboard_script, &out_path);
        let prompt = PromptConfig {
            name: "test".into(),
            template: "{{text}}".into(),
            strip_markdown_fences: false,
            trim_whitespace: false,
            stream_output: true,
        };
        let engine = EngineConfig {
            argv: vec![engine_script.display().to_string()],
            stdin: false,
            capture_stdout: true,
        };

        let overlay = OverlayHandle::spawn(false).expect("overlay should be disabled in tests");
        let emitted = stream_prompt_output(
            &overlay,
            &tools,
            &prompt,
            &engine,
            "ignored",
            &CancelToken::new(),
        )
        .await
        .expect("streaming should succeed");
        let typed = fs::read_to_string(&out_path).expect("typed output should exist");

        assert_eq!(emitted, "hello world!");
        assert_eq!(typed, "hello world!");
    }

    #[tokio::test]
    async fn raw_prompt_without_placeholder_still_attempts_optional_selection_capture() {
        // When clipboard capture fails, raw prompt without placeholder gracefully returns None
        let selected = resolve_selected_text(
            &ToolConfig {
                read_clipboard_command: vec!["false".into()],
                write_clipboard_command: vec!["false".into()],
                copy_command: vec!["false".into()],
                paste_command: vec!["false".into()],
                type_command: vec!["false".into()],
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

    fn fake_tools(clipboard_script: &Path, out_path: &Path) -> ToolConfig {
        ToolConfig {
            read_clipboard_command: vec!["true".into()],
            write_clipboard_command: vec![
                clipboard_script.display().to_string(),
                out_path.display().to_string(),
            ],
            copy_command: vec!["true".into()],
            paste_command: vec!["true".into()],
            type_command: vec!["true".into()],
            clipboard_settle_ms: 0,
        }
    }

    fn make_test_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be monotonic enough for tests")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("papagaia-{label}-{nonce}"));
        fs::create_dir_all(&dir).expect("test dir should be created");
        dir
    }

    fn write_executable(path: &Path, script: &str) {
        fs::write(path, script).expect("script should be written");
        let mut perms = fs::metadata(path)
            .expect("script metadata should exist")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).expect("script should be executable");
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
