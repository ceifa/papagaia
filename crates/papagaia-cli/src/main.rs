mod systemd;

use std::{
    fs,
    io::ErrorKind,
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use papagaia_core::{ClientRequest, ClientResponse, Config, expand_home, socket_path};

#[derive(Debug, Parser)]
#[command(
    name = "papagaia",
    about = "Tiny CLI client for the papagaia daemon",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Status,
    Prompt {
        #[command(subcommand)]
        command: PromptCommands,
    },
    Init {
        #[arg(long)]
        force: bool,
        #[arg(long, requires = "force")]
        no_backup: bool,
    },
    Doctor,
    Restart,
    ConfigPath,
}

#[derive(Debug, Subcommand)]
#[command(disable_help_subcommand = true)]
enum PromptCommands {
    List,
    Run { name: String },
    Raw(RawPromptArgs),
}

#[derive(Debug, Args)]
struct RawPromptArgs {
    #[arg(long, conflicts_with = "stdin")]
    text: Option<String>,
    #[arg(long)]
    stdin: bool,
}

#[derive(Debug)]
struct DetectedEnvironment {
    wl_copy: bool,
    wl_paste: bool,
    wtype: bool,
    ydotool: bool,
    ydotoold: bool,
    whisper_cli: bool,
    whisper_server: bool,
    whisper_model: Option<PathBuf>,
    vad_model: Option<PathBuf>,
    engine_choices: Vec<EngineChoice>,
    niri: bool,
    hyprland: bool,
    /// Whether at least one `/dev/input/event*` device is readable, i.e. whether
    /// push-to-talk (which reads the keyboard via evdev) can work without setup.
    input_readable: bool,
}

#[derive(Debug, Clone)]
struct EngineChoice {
    name: &'static str,
    argv: Vec<String>,
}

struct InitOptions {
    chosen_engine: Option<EngineChoice>,
    language: String,
    /// Use the warm `whisper-server` backend (vs. per-call `whisper-cli`).
    use_server: bool,
    /// Enable hold-to-talk by default.
    push_to_talk: bool,
}

#[derive(Debug, Clone, Copy)]
enum CheckLevel {
    Required,
    Optional,
}

#[derive(Debug)]
struct DoctorCheck {
    level: CheckLevel,
    ok: bool,
    label: String,
    suggestion: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::ConfigPath => {
            println!("{}", papagaia_core::config_path()?.display());
            Ok(())
        }
        Commands::Prompt { command } => match command {
            PromptCommands::List => print_prompt_templates(),
            PromptCommands::Run { name } => {
                print_response(send_request(&ClientRequest::Transform { prompt: name })?)
            }
            PromptCommands::Raw(args) => {
                let template = resolve_raw_prompt_text(&args)?;
                print_response(send_request(&ClientRequest::TransformRaw { template })?)
            }
        },
        Commands::Init { force, no_backup } => run_init(force, no_backup),
        Commands::Doctor => run_doctor(),
        Commands::Status => print_response(status_request()?),
        Commands::Restart => run_restart(),
    }
}

fn print_prompt_templates() -> Result<()> {
    let config = Config::load()?;
    if config.prompts.is_empty() {
        println!(
            "No saved prompts found in {}",
            papagaia_core::config_path()?.display()
        );
        println!("Add one under [[prompts]] or run `papagaia init` to seed defaults.");
        return Ok(());
    }

    let name_width = config
        .prompts
        .iter()
        .map(|prompt| prompt.name.chars().count())
        .max()
        .unwrap_or(0);

    println!("Saved prompts ({}):", config.prompts.len());
    println!();
    for prompt in &config.prompts {
        let summary = papagaia_core::prompt_summary(&prompt.template);
        println!("  {:<width$}  {}", prompt.name, summary, width = name_width);
    }
    println!();
    println!("Run one with:   papagaia prompt run <name>");
    println!("Ad-hoc prompt:  papagaia prompt raw --text 'Rewrite this: {{{{text}}}}'");
    Ok(())
}

fn run_restart() -> Result<()> {
    if !systemd::unit_path()?.exists() {
        bail!(
            "no systemd user unit is installed, so there is nothing to restart. \
             If you started the daemon manually, stop it (e.g. `pkill papagaia-daemon`) \
             and launch `papagaia-daemon` again, or run `papagaia init` to install the service."
        );
    }
    systemd::restart()?;
    println!("daemon restarted");
    Ok(())
}

fn run_init(force: bool, no_backup: bool) -> Result<()> {
    let config_path = papagaia_core::config_path()?;

    if config_path.exists() && !force {
        bail!(
            "config already exists at {}. Re-run with `papagaia init --force` to overwrite it.",
            config_path.display()
        );
    }

    let environment = detect_environment();
    print_detection_summary(&environment);

    // Non-interactive: pick sensible defaults from what's detected. Edit the
    // generated config afterwards to taste.
    let options = InitOptions {
        chosen_engine: environment.engine_choices.first().cloned(),
        language: "auto".to_string(),
        use_server: environment.whisper_server && environment.whisper_model.is_some(),
        push_to_talk: environment.input_readable,
    };

    let config_text = render_init_config(&environment, &options);

    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    if config_path.exists() && !no_backup {
        let backup = config_backup_path(&config_path)?;
        fs::copy(&config_path, &backup).with_context(|| {
            format!(
                "failed to create config backup from {} to {}",
                config_path.display(),
                backup.display()
            )
        })?;
        println!("Backed up existing config to {}", backup.display());
    }

    fs::write(&config_path, config_text)
        .with_context(|| format!("failed to write {}", config_path.display()))?;
    println!("\nWrote {}", config_path.display());

    if options
        .chosen_engine
        .as_ref()
        .is_some_and(|e| e.name == "codex")
    {
        let instructions_path = config_path
            .parent()
            .expect("config path has a parent")
            .join("codex_instructions.md");
        fs::write(
            &instructions_path,
            "You transform text. Output only the transformed text, no preamble or explanation.\n",
        )
        .with_context(|| format!("failed to write {}", instructions_path.display()))?;
        println!("Wrote {}", instructions_path.display());
    }

    match systemd::install() {
        Ok(unit_path) => {
            println!("Installed systemd user unit at {}", unit_path.display());
            println!("Daemon enabled and started via `systemctl --user`.");
        }
        Err(error) => {
            println!("Skipped systemd install: {error:#}");
            println!(
                "You can start the daemon manually with `papagaia-daemon` or retry after building it."
            );
        }
    }

    println!("\nRun `papagaia doctor` next to verify commands and paths.");
    Ok(())
}

fn print_detection_summary(env: &DetectedEnvironment) {
    println!("Detected environment:");
    println!(
        "  clipboard:     wl-copy={}, wl-paste={}",
        yes_no(env.wl_copy),
        yes_no(env.wl_paste)
    );
    println!(
        "  input:         wtype={}, ydotool={}",
        yes_no(env.wtype),
        yes_no(env.ydotool)
    );
    println!(
        "  whisper:       cli={}, server={}",
        yes_no(env.whisper_cli),
        yes_no(env.whisper_server)
    );
    if let Some(model) = &env.whisper_model {
        println!("  whisper model: {}", model.display());
    }
    println!("  keybinds:      input readable={}", yes_no(env.input_readable));
    println!(
        "  compositor:    {}",
        if env.niri {
            "niri"
        } else if env.hyprland {
            "hyprland"
        } else {
            "unknown"
        }
    );
    if env.engine_choices.is_empty() {
        println!("  engines:       none");
    } else {
        let names: Vec<&str> = env.engine_choices.iter().map(|c| c.name).collect();
        println!("  engines:       {}", names.join(", "));
    }
    println!();
}

fn run_doctor() -> Result<()> {
    let config_path = papagaia_core::config_path()?;
    let config = Config::load()?;
    let environment = detect_environment();
    let daemon_socket = papagaia_core::socket_path()?;

    let mut checks = Vec::new();
    command_check(
        &mut checks,
        "clipboard read command",
        &config.tools.read_clipboard_command,
        CheckLevel::Required,
        "install `wl-clipboard`",
    );
    command_check(
        &mut checks,
        "clipboard write command",
        &config.tools.write_clipboard_command,
        CheckLevel::Required,
        "install `wl-clipboard`",
    );
    command_check(
        &mut checks,
        "copy key injection",
        &config.tools.copy_command,
        CheckLevel::Required,
        "install `wtype` or point `copy_command` to another compatible tool",
    );
    command_check(
        &mut checks,
        "paste key injection",
        &config.tools.paste_command,
        CheckLevel::Required,
        "install `wtype` or point `paste_command` to another compatible tool",
    );
    if uses_command(&config.tools.copy_command, "ydotool")
        || uses_command(&config.tools.paste_command, "ydotool")
    {
        command_check(
            &mut checks,
            "ydotool daemon binary",
            &["ydotoold".to_string()],
            CheckLevel::Required,
            "install `ydotool` and make sure `ydotoold` is available",
        );
    }
    command_check(
        &mut checks,
        "whisper command",
        &config.whisper.argv,
        CheckLevel::Optional,
        "install `whisper.cpp` if you want dictation",
    );

    let multiple_engines = config.engine.len() > 1;
    for (index, engine) in config.engine.iter().enumerate() {
        let label = if multiple_engines {
            format!("configured engine #{}", index + 1)
        } else {
            "configured engine".to_string()
        };
        command_check(
            &mut checks,
            &label,
            &engine.argv,
            CheckLevel::Optional,
            &format!(
                "install the configured engine command or edit [engine] in {}",
                config_path.display()
            ),
        );
    }

    let systemd_unit_path = systemd::unit_path()?;
    checks.push(DoctorCheck {
        level: CheckLevel::Optional,
        ok: systemd_unit_path.exists(),
        label: "systemd user unit".into(),
        suggestion: Some(format!(
            "run `papagaia init` to install {}",
            systemd_unit_path.display()
        )),
    });

    let systemd_active = systemd::is_active();
    checks.push(DoctorCheck {
        level: CheckLevel::Optional,
        ok: systemd_active,
        label: "systemd service active".into(),
        suggestion: Some(
            "start it with `systemctl --user enable --now papagaia-daemon.service`".into(),
        ),
    });

    checks.push(DoctorCheck {
        level: CheckLevel::Optional,
        ok: daemon_socket.exists(),
        label: "daemon socket".into(),
        suggestion: Some(
            "start the daemon with `systemctl --user start papagaia-daemon.service` or `papagaia-daemon`".into(),
        ),
    });

    checks.push(DoctorCheck {
        level: CheckLevel::Optional,
        ok: environment.whisper_model.is_some()
            || Path::new(&config.whisper.model).exists(),
        label: "whisper model path".into(),
        suggestion: Some(
            "set `[whisper].model` to a local ggml model file, or run `papagaia init --force` after placing one in ~/.local/share/whisper.cpp/".into(),
        ),
    });

    checks.push(DoctorCheck {
        level: CheckLevel::Optional,
        ok: environment.vad_model.is_some(),
        label: "VAD model (silero-vad.onnx)".into(),
        suggestion: Some(
            "download silero-vad.onnx to ~/.local/share/whisper-models/ for voice activity detection (reduces hallucinations on silent audio)".into(),
        ),
    });

    if matches!(config.whisper.backend, papagaia_core::WhisperBackend::Server) {
        checks.push(DoctorCheck {
            level: CheckLevel::Optional,
            ok: environment.whisper_server,
            label: "whisper-server (server backend)".into(),
            suggestion: Some(
                "install whisper.cpp's `whisper-server`, or set [whisper].backend = \"cli\"".into(),
            ),
        });
    }

    let keybinds = [
        ("push_to_talk", &config.keybinds.push_to_talk),
        ("toggle", &config.keybinds.toggle),
        ("pick", &config.keybinds.pick),
    ];
    let configured_keybinds: Vec<(&str, &String)> = keybinds
        .iter()
        .filter(|(_, key)| !key.trim().is_empty())
        .map(|(name, key)| (*name, *key))
        .collect();

    if !configured_keybinds.is_empty() {
        checks.push(DoctorCheck {
            level: CheckLevel::Optional,
            ok: environment.input_readable,
            label: "keybinds: /dev/input readable".into(),
            suggestion: Some(
                "add your user to the 'input' group: `sudo usermod -aG input $USER`, then re-login"
                    .into(),
            ),
        });
        for (name, key) in &configured_keybinds {
            checks.push(DoctorCheck {
                level: CheckLevel::Optional,
                ok: papagaia_core::parse_key_name(key).is_some(),
                label: format!("keybind [{name}] valid"),
                suggestion: Some(format!(
                    "'{key}' is not a recognized key name; try RightCtrl, F13, or Menu"
                )),
            });
        }
    }

    let required_total = checks
        .iter()
        .filter(|check| matches!(check.level, CheckLevel::Required))
        .count();
    let required_missing = checks
        .iter()
        .filter(|check| matches!(check.level, CheckLevel::Required) && !check.ok)
        .count();
    let optional_missing = checks
        .iter()
        .filter(|check| matches!(check.level, CheckLevel::Optional) && !check.ok)
        .count();

    let overall_ok = required_missing == 0;
    let status = if overall_ok {
        "ready"
    } else {
        "needs attention"
    };

    println!("papagaia doctor: {status}");
    println!("config: {}", config_path.display());
    println!(
        "required: {}/{} ok",
        required_total.saturating_sub(required_missing),
        required_total
    );
    println!("optional missing: {optional_missing}");

    let missing_checks: Vec<&DoctorCheck> = checks.iter().filter(|check| !check.ok).collect();
    if missing_checks.is_empty() {
        println!();
        println!("action items: none");
    } else {
        println!();
        println!("action items:");
        for check in missing_checks {
            if let Some(suggestion) = &check.suggestion {
                println!("- {}: {}", check.label, suggestion);
            } else {
                println!("- {}", check.label);
            }
        }
    }

    println!();
    println!("environment:");
    println!(
        "- input: wl-copy={}, wl-paste={}, wtype={}, ydotool={}, ydotoold={}",
        yes_no(environment.wl_copy),
        yes_no(environment.wl_paste),
        yes_no(environment.wtype),
        yes_no(environment.ydotool),
        yes_no(environment.ydotoold)
    );
    println!("- whisper-cli: {}", yes_no(environment.whisper_cli));
    println!("- whisper-server: {}", yes_no(environment.whisper_server));
    println!(
        "- whisper backend: {}",
        match config.whisper.backend {
            papagaia_core::WhisperBackend::Server => "server",
            papagaia_core::WhisperBackend::Cli => "cli",
        }
    );
    let keybind = |key: &str| if key.is_empty() { "—".to_string() } else { key.to_string() };
    println!(
        "- keybinds: push_to_talk={}, toggle={}, pick={} (input readable={})",
        keybind(&config.keybinds.push_to_talk),
        keybind(&config.keybinds.toggle),
        keybind(&config.keybinds.pick),
        yes_no(environment.input_readable),
    );
    println!(
        "- whisper model: {}",
        environment
            .whisper_model
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| config.whisper.model.clone())
    );
    println!(
        "- vad model: {}",
        environment
            .vad_model
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "not found".into())
    );
    println!(
        "- detected engines: {}",
        if environment.engine_choices.is_empty() {
            "none".into()
        } else {
            environment
                .engine_choices
                .iter()
                .map(|choice| choice.name)
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    println!(
        "- configured engine: {}",
        if config.engine.is_empty() {
            "<unset>".to_string()
        } else {
            config
                .engine
                .iter()
                .map(|engine| engine.argv.first().map(String::as_str).unwrap_or("<unset>"))
                .collect::<Vec<_>>()
                .join(" → ")
        }
    );
    println!(
        "- daemon: {}",
        if daemon_socket.exists() {
            "running"
        } else {
            "not running"
        }
    );
    println!(
        "- systemd unit: {} ({}, {})",
        if systemd_unit_path.exists() {
            "installed"
        } else {
            "missing"
        },
        if systemd::is_enabled() {
            "enabled"
        } else {
            "disabled"
        },
        if systemd_active { "active" } else { "inactive" },
    );
    Ok(())
}

fn command_check(
    checks: &mut Vec<DoctorCheck>,
    label: &str,
    argv: &[String],
    level: CheckLevel,
    suggestion: &str,
) {
    let Some(program) = argv.first() else {
        checks.push(DoctorCheck {
            level,
            ok: false,
            label: label.into(),
            suggestion: Some(suggestion.into()),
        });
        return;
    };

    checks.push(DoctorCheck {
        level,
        ok: command_exists(program),
        label: label.into(),
        suggestion: Some(suggestion.into()),
    });
}

fn detect_environment() -> DetectedEnvironment {
    DetectedEnvironment {
        wl_copy: command_exists("wl-copy"),
        wl_paste: command_exists("wl-paste"),
        wtype: command_exists("wtype"),
        ydotool: command_exists("ydotool"),
        ydotoold: command_exists("ydotoold"),
        whisper_cli: command_exists("whisper-cli"),
        whisper_server: command_exists("whisper-server"),
        whisper_model: find_whisper_model(),
        vad_model: find_vad_model(),
        engine_choices: detect_engine_choices(),
        niri: command_exists("niri"),
        hyprland: command_exists("hyprctl"),
        input_readable: input_devices_readable(),
    }
}

/// Whether any `/dev/input/event*` device can be opened for reading. Push-to-talk
/// needs this (it reads the keyboard via evdev); it's true when the user is in the
/// `input` group.
fn input_devices_readable() -> bool {
    let Ok(entries) = fs::read_dir("/dev/input") else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_event_node = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("event"));
        if is_event_node && fs::File::open(&path).is_ok() {
            return true;
        }
    }
    false
}

fn render_init_config(environment: &DetectedEnvironment, options: &InitOptions) -> String {
    let whisper_model = environment
        .whisper_model
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "~/.local/share/whisper-models/ggml-base.bin".into());
    let (copy_command, paste_command) = preferred_input_commands(environment);
    let engine_command = options
        .chosen_engine
        .as_ref()
        .map(|choice| toml_array_owned(&choice.argv))
        .unwrap_or_else(|| toml_array(&["your-llm-cli", "--prompt", "{{prompt}}"]));
    let engine_comment = options
        .chosen_engine
        .as_ref()
        .map(|choice| format!("# Auto-detected engine: {}\n", choice.name))
        .unwrap_or_else(|| {
            "# Configure this to whichever CLI you want to use for text transformation.\n".into()
        });
    let vad_args = environment
        .vad_model
        .as_ref()
        .map(|path| format!(r#", "--vad", "-vm", "{}""#, path.display()))
        .unwrap_or_default();
    let language = options.language.as_str();
    let whisper_backend = if options.use_server { "server" } else { "cli" };
    // Default the push-to-talk hotkey on only when evdev can actually read the
    // keyboard; otherwise leave it empty so nothing silently fails.
    let push_to_talk_key = if options.push_to_talk { "RightCtrl" } else { "" };

    format!(
        r#"logging = true

[tools]
read_clipboard_command = ["wl-paste", "--no-newline"]
write_clipboard_command = ["wl-copy"]
copy_command = {copy_command}
paste_command = {paste_command}
clipboard_settle_ms = 120

[overlay]
enabled = true

[whisper]
# backend = "server" keeps the model resident in a warm whisper-server (fast).
# "cli" shells out to whisper-cli per call. The server path automatically falls
# back to the cli path below when the server is unreachable.
backend = "{whisper_backend}"
model = "{whisper_model}"
argv = ["whisper-cli", "-m", "{{{{model}}}}", "-f", "{{{{audio_path}}}}", "-np", "-nt", "-l", "{language}"{vad_args}, "--prompt", "Natural spoken dictation with correct punctuation, natural sentences, and no filler words."]
# Warm-server backend: the daemon launches and supervises whisper-server itself.
server_url = "http://127.0.0.1:8080"
manage_server = true
server_argv = ["whisper-server", "-m", "{{{{model}}}}", "--host", "127.0.0.1", "--port", "8080", "-l", "{language}"{vad_args}]

[dictation.cleanup]
# Fast, local, rule-based polish applied to every transcript (no LLM).
voice_commands = true         # "new line"/"nova linha" -> break, "period"/"ponto final" -> "."
dedupe_repeated_words = true  # "the the" -> "the"
collapse_whitespace = true
capitalize_sentences = true
remove_fillers = false        # off by default: removing filler words can change meaning
# filler_words = ["um", "uh", "né", "tipo"]

[keybinds]
# Global hotkeys papagaia watches itself via evdev — no compositor config needed.
# Needs read access to /dev/input (be in the 'input' group). Empty = no hotkey.
# Pick keys whose passthrough is harmless (a dead key like F13, or RightCtrl).
push_to_talk = "{push_to_talk_key}"   # hold to dictate, release to insert
toggle = ""                           # tap to toggle hands-free dictation
pick = ""                             # tap to open the prompt picker

{engine_comment}# Engine fallback: repeat this section as [[engine]] tables to define a
# chain. Each engine is tried in order; if one fails (missing binary, non-zero
# exit, network error) the next is used. A single [engine] table also works.
#
#   [[engine]]
#   argv = {engine_command}
#   [[engine]]
#   argv = ["ollama", "run", "llama3.2"]
#   stdin = true
[engine]
argv = {engine_command}
stdin = false

[[prompts]]
name = "shorten"
template = """
Rewrite the following text so it is shorter but keeps the original meaning.
Return only the rewritten text.

{{{{text}}}}
"""

[[prompts]]
name = "fix-grammar"
template = """
Correct grammar, spelling, and punctuation in the following text.
Return only the corrected text.

{{{{text}}}}
"""
"#
    )
}

fn preferred_input_commands(environment: &DetectedEnvironment) -> (String, String) {
    if environment.wtype || !environment.ydotool {
        return (
            toml_array(&["wtype", "-M", "ctrl", "-k", "c", "-m", "ctrl"]),
            toml_array(&["wtype", "-M", "ctrl", "-k", "v", "-m", "ctrl"]),
        );
    }

    (
        toml_array(&["ydotool", "key", "29:1", "46:1", "46:0", "29:0"]),
        toml_array(&["ydotool", "key", "29:1", "47:1", "47:0", "29:0"]),
    )
}

// NOTE: the model names below are baked-in `init` defaults that date quickly —
// they only seed a fresh config; revisit when a vendor renames/retires a model.
fn detect_engine_choices() -> Vec<EngineChoice> {
    let mut choices = Vec::new();

    if command_exists("codex") {
        choices.push(EngineChoice {
            name: "codex",
            argv: vec![
                "codex".into(),
                "exec".into(),
                "-m".into(),
                "gpt-5.4-mini".into(),
                "--ephemeral".into(),
                "--skip-git-repo-check".into(),
                "-c".into(),
                "model_reasoning_effort=none".into(),
                "-c".into(),
                "model_verbosity=low".into(),
                "-c".into(),
                "model_reasoning_summary=none".into(),
                "-c".into(),
                "hide_agent_reasoning=true".into(),
                "-c".into(),
                "model_instructions_file=\"~/.config/papagaia/codex_instructions.md\"".into(),
                "-c".into(),
                "sandbox_mode=read-only".into(),
                "-c".into(),
                "approval_policy=never".into(),
                "-c".into(),
                "include_environment_context=false".into(),
                "-c".into(),
                "skills.bundled.enabled=false".into(),
                "--disable".into(),
                "shell_tool".into(),
                "--disable".into(),
                "plugins".into(),
                "--disable".into(),
                "multi_agent".into(),
                "--disable".into(),
                "tool_suggest".into(),
                "--disable".into(),
                "fast_mode".into(),
                "--disable".into(),
                "undo".into(),
                "{{prompt}}".into(),
            ],
        });
    }

    if command_exists("claude") {
        choices.push(EngineChoice {
            name: "claude",
            argv: vec![
                "claude".into(),
                "--disable-slash-commands".into(),
                "--effort".into(),
                "low".into(),
                "--tools".into(),
                "".into(),
                "--system-prompt".into(),
                "You transform text. Output only the transformed text, no preamble or explanation."
                    .into(),
                "--no-session-persistence".into(),
                "--exclude-dynamic-system-prompt-sections".into(),
                "--setting-sources".into(),
                "".into(),
                "-p".into(),
                "--model".into(),
                "haiku".into(),
                "{{prompt}}".into(),
            ],
        });
    }

    if gh_copilot_exists() {
        choices.push(EngineChoice {
            name: "github-copilot",
            argv: vec![
                "gh".into(),
                "copilot".into(),
                "-s".into(),
                "--model".into(),
                "gpt-4.1".into(),
                "--disable-builtin-mcps".into(),
                "--no-custom-instructions".into(),
                "--no-auto-update".into(),
                "--no-ask-user".into(),
                "--no-remote".into(),
                "--no-color".into(),
                "-p".into(),
                "{{prompt}}".into(),
            ],
        });
    }

    if command_exists("llama-cli") {
        choices.push(EngineChoice {
            name: "llama.cpp",
            argv: vec!["llama-cli".into(), "-p".into(), "{{prompt}}".into()],
        });
    }

    if command_exists("gemini") {
        choices.push(EngineChoice {
            name: "gemini",
            argv: vec![
                "gemini".into(),
                "-m".into(),
                "gemini-3.1-flash-lite-preview".into(),
                "-p".into(),
                "{{prompt}}".into(),
            ],
        });
    }

    choices
}

fn config_backup_path(config_path: &Path) -> Result<PathBuf> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock appears to be before the Unix epoch")?
        .as_secs();
    let backup_name = format!(
        "{}.bak.{timestamp}",
        config_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config.toml")
    );
    Ok(config_path.with_file_name(backup_name))
}

const MODEL_SEARCH_DIRS: &[&str] = &[
    "~/.local/share/whisper-models/",
    "~/.local/share/whisper.cpp/",
    "~/.local/share/whisper.cpp/models/",
    "~/.cache/whisper.cpp/",
    "~/.local/share/papagaia/whisper/",
];

fn find_whisper_model() -> Option<PathBuf> {
    MODEL_SEARCH_DIRS.iter().find_map(|directory| {
        let directory = PathBuf::from(expand_home(directory));
        find_first_whisper_model_in_dir(&directory)
    })
}

fn gh_copilot_exists() -> bool {
    command_exists("gh")
        && std::process::Command::new("gh")
            .args(["copilot", "--version"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
}

fn find_vad_model() -> Option<PathBuf> {
    let names = ["silero-vad.bin", "silero-vad.onnx"];
    MODEL_SEARCH_DIRS.iter().find_map(|directory| {
        let directory = PathBuf::from(expand_home(directory));
        names.iter().find_map(|name| {
            let path = directory.join(name);
            path.is_file().then_some(path)
        })
    })
}

fn find_first_whisper_model_in_dir(directory: &Path) -> Option<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(directory)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_file())
        .filter(|path| {
            matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("bin") | Some("gguf")
            ) || path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("ggml") || name.contains("whisper"))
        })
        .collect();

    files.sort();
    files.into_iter().next()
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn toml_array(items: &[&str]) -> String {
    let quoted: Vec<String> = items.iter().map(|item| format!("{item:?}")).collect();
    format!("[{}]", quoted.join(", "))
}

fn toml_array_owned(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|item| format!("{item:?}")).collect();
    format!("[{}]", quoted.join(", "))
}

fn command_exists(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).exists();
    }

    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join(program))
        .any(|path| path.exists())
}

fn uses_command(argv: &[String], program: &str) -> bool {
    matches!(argv.first().map(String::as_str), Some(found) if found == program)
}

fn resolve_raw_prompt_text(args: &RawPromptArgs) -> Result<String> {
    match (&args.text, args.stdin) {
        (Some(text), false) => Ok(text.clone()),
        (None, true) => {
            let mut buffer = String::new();
            io::stdin().read_to_string(&mut buffer)?;
            let text = buffer.trim().to_string();
            if text.is_empty() {
                bail!("stdin prompt text was empty");
            }
            Ok(text)
        }
        (Some(_), true) => bail!("use either --text or --stdin, not both"),
        (None, false) => bail!("provide --text or --stdin for an ad-hoc prompt"),
    }
}

fn print_response(response: ClientResponse) -> Result<()> {
    if response.ok {
        if let Some(text) = response.text {
            println!("{text}");
        } else {
            println!("{}", response.message);
        }
        Ok(())
    } else {
        bail!("{}", response.message)
    }
}

fn send_request(request: &ClientRequest) -> Result<ClientResponse> {
    let socket = socket_path()?;
    let stream = UnixStream::connect(&socket)
        .with_context(|| format!("failed to connect to daemon at {}", socket.display()))?;
    send_on_stream(stream, request)
}

fn status_request() -> Result<ClientResponse> {
    let socket = socket_path()?;
    let stream = match UnixStream::connect(&socket) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(ClientResponse::ok("stopped"));
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to connect to daemon at {}", socket.display()));
        }
    };
    send_on_stream(stream, &ClientRequest::Status)
}

fn send_on_stream(mut stream: UnixStream, request: &ClientRequest) -> Result<ClientResponse> {
    let request = serde_json::to_string(request)?;
    stream.write_all(request.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    serde_json::from_str(&response).context("failed to decode daemon response")
}

#[cfg(test)]
mod tests {
    use super::{DetectedEnvironment, EngineChoice, InitOptions, render_init_config};

    fn test_engine() -> EngineChoice {
        EngineChoice {
            name: "codex",
            argv: vec![
                "codex".into(),
                "exec".into(),
                "-m".into(),
                "gpt-5.4-mini".into(),
                "--ephemeral".into(),
                "--skip-git-repo-check".into(),
                "-c".into(),
                "model_reasoning_effort=none".into(),
                "-c".into(),
                "model_verbosity=low".into(),
                "-c".into(),
                "model_reasoning_summary=none".into(),
                "-c".into(),
                "hide_agent_reasoning=true".into(),
                "-c".into(),
                "model_instructions_file=\"~/.config/papagaia/codex_instructions.md\"".into(),
                "-c".into(),
                "sandbox_mode=read-only".into(),
                "-c".into(),
                "approval_policy=never".into(),
                "-c".into(),
                "include_environment_context=false".into(),
                "-c".into(),
                "skills.bundled.enabled=false".into(),
                "--disable".into(),
                "shell_tool".into(),
                "--disable".into(),
                "plugins".into(),
                "--disable".into(),
                "multi_agent".into(),
                "--disable".into(),
                "tool_suggest".into(),
                "--disable".into(),
                "fast_mode".into(),
                "--disable".into(),
                "undo".into(),
                "{{prompt}}".into(),
            ],
        }
    }

    fn test_environment() -> DetectedEnvironment {
        DetectedEnvironment {
            wl_copy: true,
            wl_paste: true,
            wtype: true,
            ydotool: true,
            ydotoold: true,
            whisper_cli: true,
            whisper_server: true,
            whisper_model: Some("/tmp/model.bin".into()),
            vad_model: None,
            engine_choices: vec![test_engine()],
            niri: true,
            hyprland: false,
            input_readable: true,
        }
    }

    #[test]
    fn init_config_uses_detected_whisper_model() {
        let environment = test_environment();
        let options = InitOptions {
            chosen_engine: Some(test_engine()),
            language: "auto".into(),
            use_server: true,
            push_to_talk: false,
        };

        let config = render_init_config(&environment, &options);
        assert!(config.contains("model = \"/tmp/model.bin\""));
        assert!(config.contains("backend = \"server\""));
        assert!(config.contains("server_argv = [\"whisper-server\""));
        assert!(config.contains("[dictation.cleanup]"));
        // push_to_talk: false -> no hotkey written.
        assert!(config.contains("[keybinds]"));
        assert!(config.contains("push_to_talk = \"\""));
        assert!(config.contains(
            "argv = [\"codex\", \"exec\", \"-m\", \"gpt-5.4-mini\", \"--ephemeral\", \"--skip-git-repo-check\", \"-c\", \"model_reasoning_effort=none\", \"-c\", \"model_verbosity=low\", \"-c\", \"model_reasoning_summary=none\", \"-c\", \"hide_agent_reasoning=true\", \"-c\", \"model_instructions_file=\\\"~/.config/papagaia/codex_instructions.md\\\"\", \"-c\", \"sandbox_mode=read-only\", \"-c\", \"approval_policy=never\", \"-c\", \"include_environment_context=false\", \"-c\", \"skills.bundled.enabled=false\", \"--disable\", \"shell_tool\", \"--disable\", \"plugins\", \"--disable\", \"multi_agent\", \"--disable\", \"tool_suggest\", \"--disable\", \"fast_mode\", \"--disable\", \"undo\", \"{{prompt}}\"]"
        ));
        // The dictation LLM post-processing config was removed.
        assert!(!config.contains("post_process"));
        assert!(!config.contains("window_title_command"));
    }

    #[test]
    fn init_config_no_engine_uses_placeholder() {
        let environment = test_environment();
        let options = InitOptions {
            chosen_engine: None,
            language: "auto".into(),
            use_server: true,
            push_to_talk: false,
        };
        let config = render_init_config(&environment, &options);
        assert!(config.contains("\"your-llm-cli\""));
    }

    #[test]
    fn init_config_round_trips_into_a_valid_config() {
        let environment = test_environment();
        let options = InitOptions {
            chosen_engine: Some(test_engine()),
            language: "pt".into(),
            use_server: true,
            push_to_talk: true,
        };
        let rendered = render_init_config(&environment, &options);

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("papagaia-init-roundtrip-{nonce}.toml"));
        std::fs::write(&path, &rendered).expect("write rendered config");

        let parsed = papagaia_core::Config::load_from_path(&path)
            .expect("generated init config must parse and validate");
        std::fs::remove_file(&path).ok();

        assert!(matches!(
            parsed.whisper.backend,
            papagaia_core::WhisperBackend::Server
        ));
        assert_eq!(parsed.keybinds.push_to_talk, "RightCtrl");
        assert!(parsed.dictation.cleanup.voice_commands);
        assert!(!parsed.dictation.cleanup.remove_fillers);
        assert!(parsed.whisper.server_argv.iter().any(|a| a == "whisper-server"));
    }
}
