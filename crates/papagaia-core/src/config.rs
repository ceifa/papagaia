use std::{fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
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
    #[serde(default)]
    pub keybinds: KeybindsConfig,
    /// The engine chain used for text transformation. Accepts either a single
    /// `[engine]` table or a sequence of `[[engine]]` tables tried in order:
    /// when one fails, the next is attempted as a fallback.
    #[serde(deserialize_with = "deserialize_engine_chain")]
    pub engine: Vec<EngineConfig>,
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

        if self.engine.is_empty() {
            bail!("at least one [engine] must be configured");
        }
        for engine in &self.engine {
            if engine.argv.is_empty() {
                bail!("engine.argv cannot be empty");
            }
        }

        for prompt in &self.prompts {
            if prompt.name.trim().is_empty() {
                bail!("prompt name cannot be empty");
            }
        }

        Ok(())
    }

    pub fn prompt(&self, name: &str) -> Result<&PromptConfig> {
        self.prompts
            .iter()
            .find(|prompt| prompt.name == name)
            .with_context(|| format!("unknown prompt '{name}'"))
    }

    fn normalize(&mut self) {
        self.whisper.model = expand_home(&self.whisper.model);
        let argv_fields = [
            &mut self.tools.read_clipboard_command,
            &mut self.tools.write_clipboard_command,
            &mut self.tools.copy_command,
            &mut self.tools.paste_command,
            &mut self.whisper.argv,
            &mut self.whisper.server_argv,
        ];
        for argv in argv_fields {
            for arg in argv.iter_mut() {
                *arg = expand_home(arg);
            }
        }
        for engine in &mut self.engine {
            for arg in engine.argv.iter_mut() {
                *arg = expand_home(arg);
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolConfig {
    #[serde(default = "default_read_clipboard_command")]
    pub read_clipboard_command: Vec<String>,
    #[serde(default = "default_write_clipboard_command")]
    pub write_clipboard_command: Vec<String>,
    #[serde(default = "default_copy_command")]
    pub copy_command: Vec<String>,
    #[serde(default = "default_paste_command")]
    pub paste_command: Vec<String>,
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
            clipboard_settle_ms: default_clipboard_settle_ms(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DictationConfig {
    /// Local, rule-based cleanup applied to every transcript before it is typed.
    /// Fast and deterministic — no LLM involved.
    #[serde(default)]
    pub cleanup: CleanupConfig,
}

/// Global hotkeys papagaia watches itself (via evdev), so you don't configure
/// them in your compositor. Each value is a key name ("RightCtrl", "F13", "Menu",
/// or a raw evdev keycode); empty means that action has no hotkey.
#[derive(Debug, Clone, Deserialize)]
pub struct KeybindsConfig {
    /// Hold to dictate: records while held, transcribes and inserts on release.
    #[serde(default = "default_push_to_talk_key")]
    pub push_to_talk: String,
    /// Tap to toggle hands-free dictation on/off.
    #[serde(default)]
    pub toggle: String,
    /// Tap to open the prompt picker.
    #[serde(default)]
    pub pick: String,
}

impl Default for KeybindsConfig {
    fn default() -> Self {
        Self {
            push_to_talk: default_push_to_talk_key(),
            toggle: String::new(),
            pick: String::new(),
        }
    }
}

/// Toggles for the local transcript cleanup pass. Every transform is individually
/// switchable; conservative by default so meaning is never lost.
#[derive(Debug, Clone, Deserialize)]
pub struct CleanupConfig {
    /// Interpret literal voice commands ("new line"/"nova linha" → line break,
    /// "period"/"ponto final" → ".", "comma"/"vírgula" → ",", …). Applied first.
    #[serde(default = "crate::default_true")]
    pub voice_commands: bool,
    /// Collapse immediately-repeated words ("the the" → "the").
    #[serde(default = "crate::default_true")]
    pub dedupe_repeated_words: bool,
    /// Collapse runs of spaces and trim trailing whitespace (keeps line breaks).
    #[serde(default = "crate::default_true")]
    pub collapse_whitespace: bool,
    /// Capitalize the first letter of each sentence.
    #[serde(default = "crate::default_true")]
    pub capitalize_sentences: bool,
    /// Remove standalone filler words. Off by default — it can change meaning.
    #[serde(default)]
    pub remove_fillers: bool,
    /// Filler words removed when `remove_fillers` is enabled.
    #[serde(default = "default_filler_words")]
    pub filler_words: Vec<String>,
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            voice_commands: true,
            dedupe_repeated_words: true,
            collapse_whitespace: true,
            capitalize_sentences: true,
            remove_fillers: false,
            filler_words: default_filler_words(),
        }
    }
}

/// Which transcription backend powers dictation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WhisperBackend {
    /// Shell out to `whisper-cli` once per recording. Simple, but reloads the
    /// model on every call (a noticeable cold start even with GPU acceleration).
    #[default]
    Cli,
    /// POST audio to a warm `whisper-server` that keeps the model resident in
    /// RAM/VRAM. Far lower per-call latency. Falls back to `cli` when the server
    /// is unreachable.
    Server,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WhisperConfig {
    #[serde(default)]
    pub backend: WhisperBackend,
    #[serde(default = "default_whisper_model")]
    pub model: String,
    /// Initial prompt shared by both backends; inject it into `argv`/`server_argv`
    /// via the `{{prompt}}` placeholder so the text lives in one place.
    #[serde(default = "default_whisper_prompt")]
    pub prompt: String,
    #[serde(default = "default_whisper_argv")]
    pub argv: Vec<String>,
    /// Base URL of the whisper-server used by the `server` backend.
    #[serde(default = "default_whisper_server_url")]
    pub server_url: String,
    /// When true (and backend = server), the daemon spawns and supervises the
    /// whisper-server child itself, killing it when the daemon exits.
    #[serde(default = "crate::default_true")]
    pub manage_server: bool,
    /// Command used to launch the managed whisper-server. `{{model}}` expands to
    /// `model`. Only consulted when `manage_server` is true.
    #[serde(default = "default_whisper_server_argv")]
    pub server_argv: Vec<String>,
}

impl Default for WhisperConfig {
    fn default() -> Self {
        Self {
            backend: WhisperBackend::default(),
            model: default_whisper_model(),
            prompt: default_whisper_prompt(),
            argv: default_whisper_argv(),
            server_url: default_whisper_server_url(),
            manage_server: true,
            server_argv: default_whisper_server_argv(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct EngineConfig {
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub stdin: bool,
}

/// Deserialize the `engine` field from either a single `[engine]` table or a
/// sequence of `[[engine]]` tables. A single table becomes a one-element chain,
/// preserving backward compatibility with configs that predate engine fallback.
fn deserialize_engine_chain<'de, D>(deserializer: D) -> Result<Vec<EngineConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(EngineConfig),
        Many(Vec<EngineConfig>),
    }

    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(engine) => vec![engine],
        OneOrMany::Many(engines) => engines,
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct PromptConfig {
    pub name: String,
    pub template: String,
}

impl PromptConfig {
    pub fn render(&self, selected_text: &str) -> String {
        render_prompt_template(&self.template, selected_text)
    }

    pub fn clean_output(&self, raw: &str) -> String {
        strip_outer_markdown_fence(raw).trim().to_string()
    }
}

/// Whether a prompt template references the captured selection via a
/// `{{text}}` or `{{selection}}` placeholder. When false, callers append the
/// selection (if any) to the end of the template instead.
pub fn template_needs_selection(template: &str) -> bool {
    template.contains("{{text}}") || template.contains("{{selection}}")
}

/// One-line summary of a prompt template — its first non-empty line, truncated.
/// Used by both the CLI's `prompt list` and the daemon-driven picker.
pub fn prompt_summary(template: &str) -> String {
    const MAX_LEN: usize = 72;
    let first_line = template
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    if first_line.chars().count() <= MAX_LEN {
        return first_line.to_string();
    }
    let truncated: String = first_line.chars().take(MAX_LEN - 1).collect();
    format!("{truncated}…")
}

pub fn render_prompt_template(template: &str, selected_text: &str) -> String {
    if template_needs_selection(template) {
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

pub fn overlay_program() -> PathBuf {
    if let Ok(current_exe) = std::env::current_exe()
        && let Some(parent) = current_exe.parent()
    {
        let sibling = parent.join("papagaia-overlay");
        if sibling.exists() {
            return sibling;
        }
    }
    PathBuf::from("papagaia-overlay")
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

fn default_overlay_enabled() -> bool {
    true
}

fn default_clipboard_settle_ms() -> u64 {
    120
}

fn default_whisper_server_url() -> String {
    "http://127.0.0.1:8080".into()
}

fn default_whisper_server_argv() -> Vec<String> {
    vec![
        "whisper-server".into(),
        "-m".into(),
        "{{model}}".into(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        "8080".into(),
        "-l".into(),
        "auto".into(),
        "--prompt".into(),
        "{{prompt}}".into(),
    ]
}

fn default_push_to_talk_key() -> String {
    "RightCtrl".into()
}

fn default_filler_words() -> Vec<String> {
    ["um", "uh", "uhm", "hmm", "er", "ah", "né", "tipo", "então"]
        .iter()
        .map(|word| word.to_string())
        .collect()
}

/// Map a key name ("RightCtrl", "F13", "Menu") or a raw evdev keycode to its
/// Linux input-event code (case-insensitive; ignores a `key_` prefix and any
/// spaces/underscores/hyphens). In core, so it stays free of the `evdev` dep.
pub fn parse_key_name(name: &str) -> Option<u16> {
    let mut norm = name.trim().to_lowercase();
    if let Some(stripped) = norm.strip_prefix("key_") {
        norm = stripped.to_string();
    }
    norm.retain(|c| c != ' ' && c != '_' && c != '-');

    // Raw numeric escape hatch (a literal evdev keycode).
    if let Ok(code) = norm.parse::<u16>() {
        return Some(code);
    }

    // Function keys F1..F24 (the Linux codes are not contiguous across the range).
    if let Some(rest) = norm.strip_prefix('f')
        && let Ok(n) = rest.parse::<u16>()
    {
        return match n {
            1..=10 => Some(59 + (n - 1)), // F1=59 .. F10=68
            11 => Some(87),
            12 => Some(88),
            13..=24 => Some(183 + (n - 13)), // F13=183 .. F24=194
            _ => None,
        };
    }

    Some(match norm.as_str() {
        "rightctrl" | "rctrl" | "ctrlr" => 97,
        "leftctrl" | "lctrl" | "ctrl" | "ctrll" => 29,
        "rightalt" | "ralt" | "altr" | "altgr" => 100,
        "leftalt" | "lalt" | "alt" | "altl" => 56,
        "rightshift" | "rshift" | "shiftr" => 54,
        "leftshift" | "lshift" | "shift" | "shiftl" => 42,
        "rightmeta" | "rmeta" | "rightsuper" | "rsuper" | "rightwin" | "rwin" => 126,
        "leftmeta" | "lmeta" | "meta" | "super" | "leftsuper" | "lsuper" | "win" | "leftwin"
        | "lwin" => 125,
        "capslock" | "caps" => 58,
        "menu" | "compose" | "apps" => 127,
        "scrolllock" | "scroll" => 70,
        "pause" => 119,
        "insert" | "ins" => 110,
        "home" => 102,
        "end" => 107,
        "pageup" | "pgup" => 104,
        "pagedown" | "pgdn" => 109,
        "space" => 57,
        "tab" => 15,
        "grave" | "backtick" => 41,
        _ => return None,
    })
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

fn default_whisper_model() -> String {
    "~/.local/share/whisper.cpp/ggml-base.bin".into()
}

fn default_whisper_prompt() -> String {
    "Natural spoken dictation with correct punctuation, natural sentences, and no filler words."
        .into()
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
        "-bo".into(),
        "3".into(),
        "-bs".into(),
        "3".into(),
        "--prompt".into(),
        "{{prompt}}".into(),
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

#[cfg(test)]
mod tests {
    use super::{
        PromptConfig, WhisperConfig, expand_home, parse_key_name, render_prompt_template,
        strip_outer_markdown_fence,
    };

    #[test]
    fn whisper_prompt_is_shared_by_both_backends_via_placeholder() {
        let whisper = WhisperConfig::default();
        assert!(!whisper.prompt.is_empty());
        for (name, argv) in [
            ("argv", &whisper.argv),
            ("server_argv", &whisper.server_argv),
        ] {
            assert!(
                argv.iter().any(|arg| arg == "{{prompt}}"),
                "{name} should reference the prompt placeholder"
            );
            assert!(
                !argv.iter().any(|arg| arg.contains(&whisper.prompt)),
                "{name} should not duplicate the literal prompt text"
            );
        }
    }

    #[test]
    fn parse_key_name_handles_names_aliases_and_codes() {
        assert_eq!(parse_key_name("RightCtrl"), Some(97));
        assert_eq!(parse_key_name("right ctrl"), Some(97));
        assert_eq!(parse_key_name("KEY_RIGHTCTRL"), Some(97));
        assert_eq!(parse_key_name("rctrl"), Some(97));
        assert_eq!(parse_key_name("F13"), Some(183));
        assert_eq!(parse_key_name("f1"), Some(59));
        assert_eq!(parse_key_name("menu"), Some(127));
        assert_eq!(parse_key_name("97"), Some(97));
        assert_eq!(parse_key_name("not-a-key"), None);
    }

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
    fn single_engine_table_parses_as_one_element_chain() {
        let config: super::Config = toml::from_str(
            r#"
[engine]
argv = ["codex", "{{prompt}}"]
"#,
        )
        .expect("single [engine] table should parse");

        assert_eq!(config.engine.len(), 1);
        assert_eq!(config.engine[0].argv, vec!["codex", "{{prompt}}"]);
        config.validate().expect("single engine is valid");
    }

    #[test]
    fn repeated_engine_tables_parse_as_ordered_chain() {
        let config: super::Config = toml::from_str(
            r#"
[[engine]]
argv = ["codex", "{{prompt}}"]
stdin = false

[[engine]]
argv = ["ollama", "run", "llama3.2"]
stdin = true
"#,
        )
        .expect("repeated [[engine]] tables should parse");

        assert_eq!(config.engine.len(), 2);
        assert_eq!(config.engine[0].argv[0], "codex");
        assert!(!config.engine[0].stdin);
        assert_eq!(config.engine[1].argv[0], "ollama");
        assert!(config.engine[1].stdin);
        config.validate().expect("engine chain is valid");
    }
}
