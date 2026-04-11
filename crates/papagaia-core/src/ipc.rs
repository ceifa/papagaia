use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ClientRequest {
    Status,
    Transform { prompt: String },
    TransformRaw { template: String },
    DictateStart,
    DictateStop,
    DictateToggle,
    Reload,
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
