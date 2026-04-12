use std::{fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub logging: bool,
    #[serde(default)]
    pub tools: ToolConfig,
    #[serde(default)]
    pub overlay: OverlayConfig,
    #[serde(default)]
    pub whisper: WhisperConfig,
    #[serde(default)]
    pub dictation: DictationConfig,
    pub engine: EngineConfig,
    #[serde(default)]
    pub prompts: Vec<PromptConfig>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            bail!(
                "no config found at {}. Run `papagaia init` to generate one.",
                path.display()
            );
        }
        Self::load_from_path(&path)
    }

    pub fn load_from_path(path: &std::path::Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let mut config: Config = toml::from_str(&text)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        config.normalize();
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.tools.read_clipboard_command.is_empty() {
            bail!("tools.read_clipboard_command cannot be empty");
        }
        if self.tools.write_clipboard_command.is_empty() {
            bail!("tools.write_clipboard_command cannot be empty");
        }
        if self.tools.copy_command.is_empty() {
            bail!("tools.copy_command cannot be empty");
        }
        if self.tools.paste_command.is_empty() {
            bail!("tools.paste_command cannot be empty");
        }
        if self.tools.type_command.is_empty() {
            bail!("tools.type_command cannot be empty");
        }

        if self.engine.argv.is_empty() {
            bail!("engine.argv cannot be empty");
        }

        for prompt in &self.prompts {
            if prompt.name.trim().is_empty() {
                bail!("prompt name cannot be empty");
            }
            validate_prompt_options(prompt)?;
        }

        Ok(())
    }

    pub fn prompt(&self, name: &str) -> Result<&PromptConfig> {
        self.prompts
            .iter()
            .find(|prompt| prompt.name == name)
            .with_context(|| format!("unknown prompt '{name}'"))
    }

    pub fn engine(&self) -> &EngineConfig {
        &self.engine
    }

    fn normalize(&mut self) {
        self.whisper.model = expand_home(&self.whisper.model);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolConfig {
    #[serde(default = "default_read_clipboard_command")]
    pub read_clipboard_command: Vec<String>,
    #[serde(default = "default_write_clipboard_command")]
    pub write_clipboard_command: Vec<String>,
    #[serde(default = "default_copy_command")]
    pub copy_command: Vec<String>,
    #[serde(default = "default_paste_command")]
    pub paste_command: Vec<String>,
    #[serde(default = "default_type_command")]
    pub type_command: Vec<String>,
    #[serde(default = "default_clipboard_settle_ms")]
    pub clipboard_settle_ms: u64,
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            read_clipboard_command: default_read_clipboard_command(),
            write_clipboard_command: default_write_clipboard_command(),
            copy_command: default_copy_command(),
            paste_command: default_paste_command(),
            type_command: default_type_command(),
            clipboard_settle_ms: default_clipboard_settle_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayConfig {
    #[serde(default = "default_overlay_enabled")]
    pub enabled: bool,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            enabled: default_overlay_enabled(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DictationConfig {
    /// Post-process the whisper transcript through the LLM engine.
    #[serde(default)]
    pub post_process: bool,
    /// Stream post-processed output incrementally as the engine produces it.
    #[serde(default = "default_true")]
    pub stream_post_process: bool,
    /// Prompt template for post-processing. Uses `{{text}}` for the transcript
    /// and `{{context}}` for an auto-generated context block.
    #[serde(default = "default_dictation_template")]
    pub post_process_template: String,
    /// Capture the focused window title before recording to provide context
    /// for post-processing.
    #[serde(default)]
    pub context_awareness: bool,
    /// Command that returns the focused window information (title, app id).
    /// Supports JSON output from niri, hyprctl, and sway.
    #[serde(default)]
    pub window_title_command: Vec<String>,
    /// Keep recorded WAV files in /tmp instead of deleting them after
    /// transcription. Useful for diagnosing audio capture issues.
    #[serde(default)]
    pub keep_audio_files: bool,
}

impl Default for DictationConfig {
    fn default() -> Self {
        Self {
            post_process: false,
            stream_post_process: true,
            post_process_template: default_dictation_template(),
            context_awareness: false,
            window_title_command: Vec::new(),
            keep_audio_files: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhisperConfig {
    #[serde(default = "default_whisper_model")]
    pub model: String,
    #[serde(default = "default_whisper_argv")]
    pub argv: Vec<String>,
    #[serde(default = "default_true")]
    pub capture_stdout: bool,
}

impl Default for WhisperConfig {
    fn default() -> Self {
        Self {
            model: default_whisper_model(),
            argv: default_whisper_argv(),
            capture_stdout: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub stdin: bool,
    #[serde(default = "default_true")]
    pub capture_stdout: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptConfig {
    pub name: String,
    pub template: String,
    #[serde(default)]
    pub strip_markdown_fences: bool,
    #[serde(default = "default_true")]
    pub trim_whitespace: bool,
    #[serde(default)]
    pub stream_output: bool,
}

impl PromptConfig {
    pub fn render(&self, selected_text: &str) -> String {
        render_prompt_template(&self.template, selected_text)
    }

    pub fn clean_output(&self, raw: &str) -> String {
        let mut text = if self.strip_markdown_fences {
            strip_outer_markdown_fence(raw)
        } else {
            raw.to_string()
        };

        if self.trim_whitespace {
            text = text.trim().to_string();
        }

        text
    }
}

pub fn validate_prompt_options(prompt: &PromptConfig) -> Result<()> {
    if prompt.stream_output && prompt.strip_markdown_fences {
        bail!(
            "prompt '{}' cannot use stream_output with strip_markdown_fences = true",
            prompt.name
        );
    }

    Ok(())
}

pub fn render_prompt_template(template: &str, selected_text: &str) -> String {
    if template.contains("{{text}}") || template.contains("{{selection}}") {
        return template
            .replace("{{text}}", selected_text)
            .replace("{{selection}}", selected_text);
    }

    let template = template.trim_end();
    if template.is_empty() {
        selected_text.to_string()
    } else {
        format!("{template}\n\n{selected_text}")
    }
}

pub fn expand_home(path: &str) -> String {
    if path == "~" {
        return dirs::home_dir()
            .map(|home| home.display().to_string())
            .unwrap_or_else(|| path.to_string());
    }

    if let Some(suffix) = path.strip_prefix("~/") {
        return dirs::home_dir()
            .map(|home| home.join(suffix).display().to_string())
            .unwrap_or_else(|| path.to_string());
    }

    path.to_string()
}

pub fn config_path() -> Result<PathBuf> {
    let root = dirs::config_dir().context("XDG config directory is unavailable")?;
    Ok(root.join("papagaia").join("config.toml"))
}

pub fn runtime_dir() -> Result<PathBuf> {
    let root = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("XDG_RUNTIME_DIR is unavailable")?;
    Ok(root.join("papagaia"))
}

pub fn socket_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("daemon.sock"))
}

fn strip_outer_markdown_fence(text: &str) -> String {
    let trimmed = text.trim();
    if !trimmed.starts_with("```") || !trimmed.ends_with("```") {
        return text.to_string();
    }

    let mut lines = trimmed.lines();
    let Some(first) = lines.next() else {
        return String::new();
    };
    if !first.starts_with("```") {
        return text.to_string();
    }

    let mut collected: Vec<&str> = lines.collect();
    if matches!(collected.last(), Some(line) if line.trim() == "```") {
        collected.pop();
        return collected.join("\n");
    }

    text.to_string()
}

fn default_true() -> bool {
    true
}

fn default_overlay_enabled() -> bool {
    true
}

fn default_dictation_template() -> String {
    r#"You are a voice-to-text post-processor. Your job is to turn raw speech transcription into clean, ready-to-use text.

Rules:
- Fix punctuation, capitalization, and grammar
- Remove filler words and speech artifacts (um, uh, like, you know, so, basically, I mean, right, well, tipo, né, então)
- Remove false starts and repeated words ("I want to I want to go" → "I want to go")
- Interpret voice commands literally: "new line" or "nova linha" → insert a line break, "new paragraph" or "novo parágrafo" → insert two line breaks, "period" or "ponto final" → ".", "comma" or "vírgula" → ","
- Preserve the original language — do not translate
- Preserve the speaker's meaning, intent, and tone
- Do not add, invent, or editorialize any content
- Output only the cleaned text, nothing else — no preamble, no quotes, no explanation
{{context}}
Transcription:
{{text}}"#
        .into()
}

fn default_clipboard_settle_ms() -> u64 {
    120
}

fn default_read_clipboard_command() -> Vec<String> {
    vec!["wl-paste".into(), "--no-newline".into()]
}

fn default_write_clipboard_command() -> Vec<String> {
    vec!["wl-copy".into()]
}

fn default_copy_command() -> Vec<String> {
    wtype_copy_command()
}

fn default_paste_command() -> Vec<String> {
    wtype_paste_command()
}

fn default_type_command() -> Vec<String> {
    wtype_type_command()
}

fn default_whisper_model() -> String {
    "~/.local/share/whisper.cpp/ggml-base.bin".into()
}

fn default_whisper_argv() -> Vec<String> {
    vec![
        "whisper-cli".into(),
        "-m".into(),
        "{{model}}".into(),
        "-f".into(),
        "{{audio_path}}".into(),
        "-l".into(),
        "auto".into(),
        "-np".into(),
        "-nt".into(),
        "--prompt".into(),
        "Natural spoken dictation with correct punctuation, natural sentences, and no filler words.".into(),
    ]
}

fn wtype_copy_command() -> Vec<String> {
    vec![
        "wtype".into(),
        "-M".into(),
        "ctrl".into(),
        "-k".into(),
        "c".into(),
        "-m".into(),
        "ctrl".into(),
    ]
}

fn wtype_paste_command() -> Vec<String> {
    vec![
        "wtype".into(),
        "-M".into(),
        "ctrl".into(),
        "-k".into(),
        "v".into(),
        "-m".into(),
        "ctrl".into(),
    ]
}

fn wtype_type_command() -> Vec<String> {
    vec!["wtype".into(), "{{text}}".into()]
}

#[cfg(test)]
mod tests {
    use super::{
        Config, DictationConfig, EngineConfig, OverlayConfig, PromptConfig, ToolConfig,
        WhisperConfig, expand_home, render_prompt_template, strip_outer_markdown_fence,
        validate_prompt_options,
    };

    #[test]
    fn strips_outer_markdown_fence() {
        let raw = "```rust\nfn main() {}\n```";
        assert_eq!(strip_outer_markdown_fence(raw), "fn main() {}");
    }

    #[test]
    fn prompt_render_replaces_selection() {
        let prompt = PromptConfig {
            name: "test".into(),
            template: "hello {{text}}".into(),
            strip_markdown_fences: false,
            trim_whitespace: true,
            stream_output: false,
        };

        assert_eq!(prompt.render("world"), "hello world");
    }

    #[test]
    fn loose_prompt_template_appends_selection_when_placeholder_is_missing() {
        assert_eq!(
            render_prompt_template("rewrite this nicely", "hello world"),
            "rewrite this nicely\n\nhello world"
        );
    }

    #[test]
    fn expand_home_keeps_non_home_paths() {
        assert_eq!(expand_home("/tmp/model.bin"), "/tmp/model.bin");
    }

    #[test]
    fn config_rejects_streaming_prompt_with_fence_stripping() {
        let config = Config {
            logging: false,
            tools: ToolConfig::default(),
            overlay: OverlayConfig::default(),
            whisper: WhisperConfig::default(),
            dictation: DictationConfig::default(),
            engine: EngineConfig {
                argv: vec!["engine".into()],
                stdin: false,
                capture_stdout: true,
            },
            prompts: vec![PromptConfig {
                name: "streaming".into(),
                template: "{{text}}".into(),
                strip_markdown_fences: true,
                trim_whitespace: true,
                stream_output: true,
            }],
        };

        let error = config.validate().expect_err("config should be invalid");
        assert!(
            error
                .to_string()
                .contains("cannot use stream_output with strip_markdown_fences = true")
        );
    }

    #[test]
    fn validate_prompt_options_accepts_streaming_without_fence_stripping() {
        let prompt = PromptConfig {
            name: "streaming".into(),
            template: "{{text}}".into(),
            strip_markdown_fences: false,
            trim_whitespace: true,
            stream_output: true,
        };

        validate_prompt_options(&prompt).expect("prompt should be valid");
    }
}
