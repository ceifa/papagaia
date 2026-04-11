mod systemd;

use std::{
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    io::ErrorKind,
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
    },
    Doctor,
    Dictate {
        #[command(subcommand)]
        command: DictateCommands,
    },
    Reload,
    ConfigPath,
}

#[derive(Debug, Subcommand)]
#[command(disable_help_subcommand = true)]
enum PromptCommands {
    List,
    Run { name: String },
    Raw(RawPromptArgs),
}

#[derive(Debug, Subcommand)]
#[command(disable_help_subcommand = true)]
enum DictateCommands {
    Start,
    Stop,
    Toggle,
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
    whisper_model: Option<PathBuf>,
    engine_choices: Vec<EngineChoice>,
}

#[derive(Debug, Clone)]
struct EngineChoice {
    name: &'static str,
    argv: Vec<String>,
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
                let template = resolve_raw_prompt_text(args)?;
                print_response(send_request(&ClientRequest::TransformRaw { template })?)
            }
        },
        Commands::Init { force } => run_init(force),
        Commands::Doctor => run_doctor(),
        Commands::Status => print_response(status_request()?),
        Commands::Dictate { command } => match command {
            DictateCommands::Start => print_response(send_request(&ClientRequest::DictateStart)?),
            DictateCommands::Stop => print_response(send_request(&ClientRequest::DictateStop)?),
            DictateCommands::Toggle => print_response(send_request(&ClientRequest::DictateToggle)?),
        },
        Commands::Reload => print_response(send_request(&ClientRequest::Reload)?),
    }
}

fn print_prompt_templates() -> Result<()> {
    let config = Config::load()?;
    if config.prompts.is_empty() {
        println!(
            "No saved prompts found in {}",
            papagaia_core::config_path()?.display()
        );
        return Ok(());
    }

    for prompt in &config.prompts {
        println!("{}", prompt.name);
        println!("{}", prompt.template.trim());
        println!();
    }

    println!("Run one with: papagaia prompt run <name>");
    println!("Or use an ad-hoc prompt: papagaia prompt raw --text 'Rewrite this: {{{{text}}}}'");
    Ok(())
}

fn run_init(force: bool) -> Result<()> {
    let config_path = papagaia_core::config_path()?;
    let environment = detect_environment();
    let config_text = render_init_config(&environment);

    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    if config_path.exists() {
        if !force {
            bail!(
                "config already exists at {}. Re-run with `papagaia init --force` to overwrite it.",
                config_path.display()
            );
        }

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
    println!("Wrote {}", config_path.display());

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

    println!("Run `papagaia doctor` next to verify commands and paths.");
    Ok(())
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
    command_check(
        &mut checks,
        "type text injection",
        &config.tools.type_command,
        CheckLevel::Required,
        "install `wtype` or point `type_command` to another compatible tool",
    );
    if uses_command(&config.tools.copy_command, "ydotool")
        || uses_command(&config.tools.paste_command, "ydotool")
        || uses_command(&config.tools.type_command, "ydotool")
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

    command_check(
        &mut checks,
        "configured engine",
        &config.engine.argv,
        CheckLevel::Optional,
        &format!(
            "install the configured engine command or edit [engine] in {}",
            config_path.display()
        ),
    );

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
    println!(
        "- whisper model: {}",
        environment
            .whisper_model
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| config.whisper.model.clone())
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
        config
            .engine
            .argv
            .first()
            .cloned()
            .unwrap_or_else(|| "<unset>".into())
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
        whisper_model: find_whisper_model(),
        engine_choices: detect_engine_choices(),
    }
}

fn render_init_config(environment: &DetectedEnvironment) -> String {
    let whisper_model = environment
        .whisper_model
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "~/.local/share/whisper-models/ggml-medium.bin".into());
    let (copy_command, paste_command, type_command) = preferred_input_commands(environment);
    let engine_command = environment
        .engine_choices
        .first()
        .map(|choice| toml_array_owned(&choice.argv))
        .unwrap_or_else(|| toml_array(&["your-llm-cli", "--prompt", "{{prompt}}"]));
    let engine_comment = environment
        .engine_choices
        .first()
        .map(|choice| format!("# Auto-detected engine: {}\n", choice.name))
        .unwrap_or_else(|| {
            "# Configure this to whichever CLI you want to use for text transformation.\n".into()
        });

    format!(
        r#"[tools]
read_clipboard_command = ["wl-paste", "--no-newline"]
write_clipboard_command = ["wl-copy"]
copy_command = {copy_command}
paste_command = {paste_command}
type_command = {type_command}
clipboard_settle_ms = 120

[overlay]
enabled = true

[whisper]
model = "{whisper_model}"
argv = ["whisper-cli", "-m", "{{{{model}}}}", "-f", "{{{{audio_path}}}}", "-np", "-nt"]
capture_stdout = true

{engine_comment}[engine]
argv = {engine_command}
stdin = false
capture_stdout = true

[[prompts]]
name = "shorten"
template = """
Rewrite the following text so it is shorter but keeps the original meaning.
Return only the rewritten text.

{{{{text}}}}
"""
strip_markdown_fences = true
trim_whitespace = true

[[prompts]]
name = "fix-grammar"
template = """
Correct grammar, spelling, and punctuation in the following text.
Return only the corrected text.

{{{{text}}}}
"""
strip_markdown_fences = true
trim_whitespace = true
"#
    )
}

fn preferred_input_commands(environment: &DetectedEnvironment) -> (String, String, String) {
    if environment.wtype || !environment.ydotool {
        return (
            toml_array(&["wtype", "-M", "ctrl", "-k", "c", "-m", "ctrl"]),
            toml_array(&["wtype", "-M", "ctrl", "-k", "v", "-m", "ctrl"]),
            toml_array(&["wtype", "{{text}}"]),
        );
    }

    (
        toml_array(&["ydotool", "key", "29:1", "46:1", "46:0", "29:0"]),
        toml_array(&["ydotool", "key", "29:1", "47:1", "47:0", "29:0"]),
        toml_array(&["ydotool", "type", "--escape", "0", "{{text}}"]),
    )
}

fn detect_engine_choices() -> Vec<EngineChoice> {
    let mut choices = Vec::new();

    if command_exists("gemini") {
        choices.push(EngineChoice {
            name: "gemini",
            argv: vec!["gemini".into(), "-p".into(), "{{prompt}}".into()],
        });
    }

    if command_exists("codex") {
        choices.push(EngineChoice {
            name: "codex",
            argv: vec!["codex".into(), "exec".into(), "{{prompt}}".into()],
        });
    }

    if command_exists("claude") {
        choices.push(EngineChoice {
            name: "claude",
            argv: vec!["claude".into(), "-p".into(), "{{prompt}}".into()],
        });
    }

    if gh_copilot_exists() {
        choices.push(EngineChoice {
            name: "github-copilot",
            argv: vec![
                "gh".into(),
                "copilot".into(),
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

fn find_whisper_model() -> Option<PathBuf> {
    let directories = [
        "~/.local/share/whisper-models/",
        "~/.local/share/whisper.cpp/",
        "~/.local/share/whisper.cpp/models/",
        "~/.cache/whisper.cpp/",
        "~/.local/share/papagaia/whisper/",
    ];

    directories.iter().find_map(|directory| {
        let directory = PathBuf::from(expand_home(directory));
        find_first_whisper_model_in_dir(&directory)
    })
}

fn gh_copilot_exists() -> bool {
    command_exists("gh")
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

fn resolve_raw_prompt_text(args: RawPromptArgs) -> Result<String> {
    match (args.text, args.stdin) {
        (Some(text), false) => Ok(text),
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
    let mut stream = UnixStream::connect(&socket)
        .with_context(|| format!("failed to connect to daemon at {}", socket.display()))?;
    let request = serde_json::to_string(request)?;
    stream.write_all(request.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut response = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut response)?;
    let response: ClientResponse =
        serde_json::from_str(&response).context("failed to decode daemon response")?;
    Ok(response)
}

fn status_request() -> Result<ClientResponse> {
    let socket = socket_path()?;
    let mut stream = match UnixStream::connect(&socket) {
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

    let request = serde_json::to_string(&ClientRequest::Status)?;
    stream.write_all(request.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut response = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut response)?;
    let response: ClientResponse =
        serde_json::from_str(&response).context("failed to decode daemon response")?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::{DetectedEnvironment, EngineChoice, render_init_config};

    #[test]
    fn init_config_uses_detected_whisper_model() {
        let environment = DetectedEnvironment {
            wl_copy: true,
            wl_paste: true,
            wtype: true,
            ydotool: true,
            ydotoold: true,
            whisper_cli: true,
            whisper_model: Some("/tmp/model.bin".into()),
            engine_choices: vec![EngineChoice {
                name: "codex",
                argv: vec!["codex".into(), "exec".into(), "{{prompt}}".into()],
            }],
        };

        let config = render_init_config(&environment);
        assert!(config.contains("model = \"/tmp/model.bin\""));
        assert!(config.contains("argv = [\"codex\", \"exec\", \"{{prompt}}\"]"));
    }
}
