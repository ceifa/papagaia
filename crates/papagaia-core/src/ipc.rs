use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ClientRequest {
    Status,
    Transform {
        prompt: String,
    },
    TransformRaw {
        template: String,
    },
    /// Show the prompt picker (orchestrated entirely by the daemon) and run the
    /// chosen prompt. Replaces the old CLI-driven picker that spawned the overlay
    /// itself and then sent a separate Transform back.
    Pick,
    DictateStart,
    DictateStop,
    DictateToggle,
    Cancel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientResponse {
    pub ok: bool,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl ClientResponse {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
            text: None,
        }
    }

    pub fn with_text(message: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
            text: Some(text.into()),
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: message.into(),
            text: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum OverlayMessage {
    Hidden,
    Busy {
        label: String,
        /// When true, the overlay grabs keyboard focus exclusively so the user
        /// can press Esc to cancel. Must only be set during phases where the
        /// daemon is not about to drive a foreign window via wtype/wl-copy
        /// (those need keyboard focus in the target application, not the HUD).
        #[serde(default)]
        grab_keyboard: bool,
    },
    Recording {
        level: f32,
    },
    Result {
        ok: bool,
        message: String,
    },
    /// A neutral, expected non-result (e.g. a tap too short to transcribe, or
    /// audio with no speech). Rendered muted and dismissed quickly — it isn't a
    /// failure, so it must not blare the red error styling.
    Notice {
        message: String,
    },
}

/// One row in the prompt picker, sent by the daemon to the picker overlay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickerEntry {
    pub name: String,
    pub summary: String,
}

/// What the picker overlay sends back to the daemon: either a saved prompt
/// chosen by name, or ad-hoc text typed into the search box.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum PickerResult {
    Template { name: String },
    Raw { template: String },
}
