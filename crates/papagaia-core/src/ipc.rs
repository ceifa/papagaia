use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ClientRequest {
    Status,
    Transform {
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selected_text: Option<String>,
        #[serde(default)]
        preserve_selection: bool,
    },
    TransformRaw {
        template: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selected_text: Option<String>,
        #[serde(default)]
        preserve_selection: bool,
        #[serde(default = "crate::default_true")]
        strip_markdown_fences: bool,
        #[serde(default = "crate::default_true")]
        trim_whitespace: bool,
        #[serde(default)]
        stream_output: bool,
    },
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
        transcript: Option<String>,
    },
    Result {
        ok: bool,
        message: String,
    },
}
