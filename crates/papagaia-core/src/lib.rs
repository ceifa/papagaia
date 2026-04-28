pub mod config;
pub mod ipc;

pub use config::{
    Config, DictationConfig, EngineConfig, OverlayConfig, PromptConfig, ToolConfig, WhisperConfig,
    config_path, expand_home, overlay_program, runtime_dir, socket_path,
};
pub use ipc::{ClientRequest, ClientResponse, OverlayMessage};

pub(crate) fn default_true() -> bool {
    true
}
