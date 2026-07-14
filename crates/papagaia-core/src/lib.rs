pub mod config;
pub mod ipc;

pub use config::{
    CleanupConfig, Config, DictationConfig, EngineConfig, KeybindsConfig, OverlayConfig,
    PromptConfig, ToolConfig, WhisperBackend, WhisperConfig, config_path, expand_home,
    overlay_program, parse_key_name, prompt_summary, runtime_dir, socket_path,
    template_needs_selection,
};
pub use ipc::{ClientRequest, ClientResponse, OverlayMessage, PickerEntry, PickerResult};

pub(crate) fn default_true() -> bool {
    true
}
