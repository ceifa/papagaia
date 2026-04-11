use std::sync::{Arc, RwLock};

use anyhow::{Result, bail};
use papagaia_core::{ClientRequest, ClientResponse, Config, OverlayMessage, PromptConfig};
use tokio::{
    sync::{Mutex, mpsc},
    time::{Duration, sleep},
};

use crate::{cancel::CancelToken, clipboard, dictation::Recorder, llm, overlay::OverlayHandle};

pub struct App {
    config: RwLock<Arc<Config>>,
    overlay: OverlayHandle,
    state: Mutex<State>,
}

enum State {
    Idle,
    Busy { label: String, cancel: CancelToken },
    Recording(RecordingSession),
}

struct RecordingSession {
    recorder: Recorder,
}

impl App {
    pub async fn new(config: Config) -> Result<Self> {
        let overlay = OverlayHandle::spawn(config.overlay.enabled)?;
        Ok(Self {
            config: RwLock::new(Arc::new(config)),
            overlay,
            state: Mutex::new(State::Idle),
        })
    }

    fn config(&self) -> Arc<Config> {
        self.config.read().expect("config lock poisoned").clone()
    }

    pub async fn handle(&self, request: ClientRequest) -> Result<ClientResponse> {
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
            } => {
                self.transform_raw(&template, selected_text, preserve_selection)
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
        let cancel = self
            .enter_busy(format!("running prompt '{prompt_name}'"))
            .await?;
        let outcome = self
            .transform_inner(
                prompt_name,
                selected_text.as_deref(),
                preserve_selection,
                &cancel,
            )
            .await;
        self.leave_busy().await;

        match outcome {
            Ok((text, had_selection)) => {
                let msg = if had_selection {
                    format!("Replaced selection with {prompt_name}")
                } else {
                    format!("Pasted {prompt_name} output")
                };
                self.flash_result(true, msg).await;
                Ok(ClientResponse::with_text("transform complete", text))
            }
            Err(error) => {
                self.finish_error(&cancel, &error).await;
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
        // Capture phase: wtype needs the original window focused — no grab.
        self.overlay
            .send(OverlayMessage::Busy {
                label: format!("Running {prompt_name}"),
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
            cancel,
        )
        .await?;

        let engine = config.engine().clone();
        let rendered_prompt = match &selected {
            Some(text) => prompt.render(text),
            None => prompt.template.clone(),
        };
        let raw = llm::run_engine(&engine, &rendered_prompt, cancel).await?;
        let cleaned = prompt.clean_output(&raw);

        clipboard::paste_text(&config.tools, &cleaned, cancel).await?;
        Ok((cleaned, selected.is_some()))
    }

    async fn transform_raw(
        &self,
        template: &str,
        selected_text: Option<String>,
        preserve_selection: bool,
    ) -> Result<ClientResponse> {
        let cancel = self.enter_busy("running ad-hoc prompt".into()).await?;
        let outcome = self
            .transform_raw_inner(
                template,
                selected_text.as_deref(),
                preserve_selection,
                &cancel,
            )
            .await;
        self.leave_busy().await;

        match outcome {
            Ok((text, had_selection)) => {
                let msg = if had_selection {
                    "Replaced selection with engine output"
                } else {
                    "Pasted engine output"
                };
                self.flash_result(true, msg).await;
                Ok(ClientResponse::with_text("transform complete", text))
            }
            Err(error) => {
                self.finish_error(&cancel, &error).await;
                Err(error)
            }
        }
    }

    async fn transform_raw_inner(
        &self,
        template: &str,
        selected_text: Option<&str>,
        preserve_selection: bool,
        cancel: &CancelToken,
    ) -> Result<(String, bool)> {
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
            cancel,
        )
        .await?;

        let prompt = PromptConfig {
            name: "ad-hoc".into(),
            template: template.into(),
            strip_markdown_fences: true,
            trim_whitespace: true,
        };
        let engine = config.engine().clone();
        let rendered_prompt = match &selected {
            Some(text) => prompt.render(text),
            None => template.to_string(),
        };
        let raw = llm::run_engine(&engine, &rendered_prompt, cancel).await?;
        let cleaned = prompt.clean_output(&raw);

        clipboard::paste_text(&config.tools, &cleaned, cancel).await?;
        Ok((cleaned, selected.is_some()))
    }

    async fn dictate_start(&self) -> Result<ClientResponse> {
        {
            let mut state = self.state.lock().await;
            if !matches!(*state, State::Idle) {
                bail!("papagaia is already busy");
            }

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

            *state = State::Recording(RecordingSession { recorder });
        }

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
        let recorder = {
            let mut state = self.state.lock().await;
            match std::mem::replace(
                &mut *state,
                State::Busy {
                    label: "transcribing".into(),
                    cancel: cancel.clone(),
                },
            ) {
                State::Recording(session) => session.recorder,
                other => {
                    *state = other;
                    bail!("papagaia is not recording");
                }
            }
        };

        // Transcribe phase: whisper reads the WAV file, no foreign focus needed.
        self.overlay
            .send(OverlayMessage::Busy {
                label: "Transcribing".into(),
                grab_keyboard: false,
            })
            .await;

        let config = self.config();
        let outcome = async {
            let audio_path = recorder.finish()?;
            let transcript = llm::run_whisper(&config.whisper, &audio_path, &cancel).await?;
            let cleaned = transcript.trim().to_string();
            if cleaned.is_empty() {
                bail!("whisper returned an empty transcript");
            }

            clipboard::type_text(&config.tools, &cleaned, &cancel).await?;
            std::fs::remove_file(&audio_path).ok();
            Ok::<String, anyhow::Error>(cleaned)
        }
        .await;

        self.leave_busy().await;

        match outcome {
            Ok(text) => {
                self.flash_result(true, "Dictation inserted").await;
                Ok(ClientResponse::with_text("dictation complete", text))
            }
            Err(error) => {
                self.finish_error(&cancel, &error).await;
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
            State::Busy { label, cancel } => {
                // Leave the Busy state in place so the in-flight operation can
                // unwind normally (leave_busy + flash_result). We just flip the
                // cancellation flag — the subprocess wait loop will notice and
                // kill the child.
                *state = State::Busy {
                    label,
                    cancel: cancel.clone(),
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

    async fn dictate_toggle(&self) -> Result<ClientResponse> {
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

    async fn enter_busy(&self, label: String) -> Result<CancelToken> {
        let mut state = self.state.lock().await;
        if !matches!(*state, State::Idle) {
            bail!("papagaia is already busy");
        }
        let cancel = CancelToken::new();
        *state = State::Busy {
            label,
            cancel: cancel.clone(),
        };
        Ok(cancel)
    }

    async fn leave_busy(&self) {
        let mut state = self.state.lock().await;
        *state = State::Idle;
    }

    async fn finish_error(&self, cancel: &CancelToken, error: &anyhow::Error) {
        if cancel.is_cancelled() {
            self.overlay.send(OverlayMessage::Hidden).await;
        } else {
            self.flash_result(false, error.to_string()).await;
        }
    }

    async fn flash_result(&self, ok: bool, message: impl Into<String>) {
        let message = message.into();
        self.overlay
            .send(OverlayMessage::Result {
                ok,
                message: message.clone(),
            })
            .await;
        sleep(Duration::from_millis(900)).await;
        self.overlay.send(OverlayMessage::Hidden).await;
    }
}

fn template_needs_selection(template: &str) -> bool {
    template.contains("{{text}}") || template.contains("{{selection}}")
}

async fn resolve_selected_text(
    tools: &papagaia_core::ToolConfig,
    template: &str,
    selected_text: Option<&str>,
    preserve_selection: bool,
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

    match clipboard::capture_selection(tools, cancel).await {
        Ok(text) => Ok(Some(text)),
        Err(_) if !needs_selection => Ok(None),
        Err(error) => Err(error),
    }
}
