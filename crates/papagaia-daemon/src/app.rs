use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, bail};
use papagaia_core::{ClientRequest, ClientResponse, Config, OverlayMessage, PromptConfig};
use tokio::{
    sync::{Mutex, mpsc},
    time::{Duration, sleep},
};

use crate::{clipboard, dictation::Recorder, llm, overlay::OverlayHandle};

pub struct App {
    config: RwLock<Arc<Config>>,
    overlay: OverlayHandle,
    state: Mutex<State>,
}

enum State {
    Idle,
    Busy { label: String },
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
                    State::Busy { label } => label.as_str(),
                    State::Recording(_) => "recording",
                };
                Ok(ClientResponse::ok(message))
            }
            ClientRequest::Transform { prompt } => self.transform(&prompt).await,
            ClientRequest::TransformRaw { template } => self.transform_raw(&template).await,
            ClientRequest::DictateStart => self.dictate_start().await,
            ClientRequest::DictateStop => self.dictate_stop().await,
            ClientRequest::DictateToggle => self.dictate_toggle().await,
            ClientRequest::Reload => self.reload().await,
        }
    }

    async fn reload(&self) -> Result<ClientResponse> {
        {
            let state = self.state.lock().await;
            if !matches!(*state, State::Idle) {
                bail!("papagaia is busy; try again when idle");
            }
        }

        let new_config = Config::load().context("failed to reload config")?;
        *self.config.write().expect("config lock poisoned") = Arc::new(new_config);
        Ok(ClientResponse::ok("config reloaded"))
    }

    async fn transform(&self, prompt_name: &str) -> Result<ClientResponse> {
        self.enter_busy(format!("running prompt '{prompt_name}'"))
            .await?;
        let outcome = self.transform_inner(prompt_name).await;
        self.leave_busy().await;

        match outcome {
            Ok(text) => {
                self.flash_result(true, format!("Replaced selection with {prompt_name}"))
                    .await;
                Ok(ClientResponse::with_text("transform complete", text))
            }
            Err(error) => {
                self.flash_result(false, error.to_string()).await;
                Err(error)
            }
        }
    }

    async fn transform_inner(&self, prompt_name: &str) -> Result<String> {
        self.overlay
            .send(OverlayMessage::Busy {
                label: format!("Running {prompt_name}"),
            })
            .await;

        let config = self.config();
        let selected = clipboard::capture_selection(&config.tools).await?;
        let prompt = config.prompt(prompt_name)?.clone();
        let engine = config.engine().clone();
        let rendered_prompt = prompt.render(&selected);
        let raw = llm::run_engine(&engine, &rendered_prompt).await?;
        let cleaned = prompt.clean_output(&raw);

        clipboard::replace_selection(&config.tools, &cleaned).await?;
        Ok(cleaned)
    }

    async fn transform_raw(&self, template: &str) -> Result<ClientResponse> {
        self.enter_busy("running ad-hoc prompt".into()).await?;
        let outcome = self.transform_raw_inner(template).await;
        self.leave_busy().await;

        match outcome {
            Ok(text) => {
                self.flash_result(true, "Replaced selection with engine output")
                    .await;
                Ok(ClientResponse::with_text("transform complete", text))
            }
            Err(error) => {
                self.flash_result(false, error.to_string()).await;
                Err(error)
            }
        }
    }

    async fn transform_raw_inner(&self, template: &str) -> Result<String> {
        self.overlay
            .send(OverlayMessage::Busy {
                label: "Running prompt".into(),
            })
            .await;

        let config = self.config();
        let selected = clipboard::capture_selection(&config.tools).await?;
        let prompt = PromptConfig {
            name: "ad-hoc".into(),
            template: template.into(),
            strip_markdown_fences: true,
            trim_whitespace: true,
        };
        let engine = config.engine().clone();
        let rendered_prompt = prompt.render(&selected);
        let raw = llm::run_engine(&engine, &rendered_prompt).await?;
        let cleaned = prompt.clean_output(&raw);

        clipboard::replace_selection(&config.tools, &cleaned).await?;
        Ok(cleaned)
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
        let recorder = {
            let mut state = self.state.lock().await;
            match std::mem::replace(
                &mut *state,
                State::Busy {
                    label: "transcribing".into(),
                },
            ) {
                State::Recording(session) => session.recorder,
                other => {
                    *state = other;
                    bail!("papagaia is not recording");
                }
            }
        };

        self.overlay
            .send(OverlayMessage::Busy {
                label: "Transcribing".into(),
            })
            .await;

        let config = self.config();
        let outcome = async {
            let audio_path = recorder.finish()?;
            let transcript = llm::run_whisper(&config.whisper, &audio_path).await?;
            let cleaned = transcript.trim().to_string();
            if cleaned.is_empty() {
                bail!("whisper returned an empty transcript");
            }
            clipboard::type_text(&config.tools, &cleaned).await?;
            std::fs::remove_file(&audio_path).ok();
            Ok::<String, anyhow::Error>(cleaned)
        }
        .await;

        {
            let mut state = self.state.lock().await;
            *state = State::Idle;
        }

        match outcome {
            Ok(text) => {
                self.flash_result(true, "Dictation inserted").await;
                Ok(ClientResponse::with_text("dictation complete", text))
            }
            Err(error) => {
                self.flash_result(false, error.to_string()).await;
                Err(error)
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

    async fn enter_busy(&self, label: String) -> Result<()> {
        let mut state = self.state.lock().await;
        if !matches!(*state, State::Idle) {
            bail!("papagaia is already busy");
        }
        *state = State::Busy { label };
        Ok(())
    }

    async fn leave_busy(&self) {
        let mut state = self.state.lock().await;
        *state = State::Idle;
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
