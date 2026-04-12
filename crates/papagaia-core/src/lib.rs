pub mod config;
pub mod ipc;

pub use config::{
    Config, DictationConfig, EngineConfig, OverlayConfig, PromptConfig, ToolConfig, WhisperConfig,
    config_path, expand_home, runtime_dir, socket_path, validate_prompt_options,
};
pub use ipc::{ClientRequest, ClientResponse, OverlayMessage};
