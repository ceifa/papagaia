pub mod config;
pub mod ipc;

pub use config::{
    Config, EngineConfig, OverlayConfig, PromptConfig, ToolConfig, WhisperConfig, config_path,
    expand_home, runtime_dir, socket_path,
};
pub use ipc::{ClientRequest, ClientResponse, OverlayMessage};
