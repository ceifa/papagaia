mod systemd;

use std::{
    fs,
    io::ErrorKind,
    io::{self, BufRead, BufReader, IsTerminal, Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::{ArgAction, Args, Parser, Subcommand};
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
    Dictate {
        #[command(subcommand)]
        command: DictateCommands,
    },
    Restart,
    ConfigPath,
}

#[derive(Debug, Subcommand)]
#[command(disable_help_subcommand = true)]
enum PromptCommands {
    List,
    Run { name: String },
    Raw(RawPromptArgs),
    Pick,
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
    #[arg(long)]
    stream_output: bool,
    #[arg(long, default_value_t = true, action = ArgAction::Set)]
    strip_markdown_fences: bool,
    #[arg(long = "trim-whitespace", default_value_t = true, action = ArgAction::Set)]
    trim_whitespace: bool,
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
    vad_model: Option<PathBuf>,
    engine_choices: Vec<EngineChoice>,
    niri: bool,
    hyprland: bool,
}

#[derive(Debug, Clone)]
struct EngineChoice {
    name: &'static str,
    argv: Vec<String>,
}

struct InitOptions {
    chosen_engine: Option<EngineChoice>,
    post_process: bool,
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
                print_response(send_request(&ClientRequest::Transform {
                    prompt: name,
                    selected_text: None,
                    preserve_selection: false,
                })?)
            }
            PromptCommands::Raw(args) => {
                let template = resolve_raw_prompt_text(&args)?;
                print_response(send_request(&ClientRequest::TransformRaw {
                    template,
                    selected_text: None,
                    preserve_selection: false,
                    strip_markdown_fences: args.strip_markdown_fences,
                    trim_whitespace: args.trim_whitespace,
                    stream_output: args.stream_output,
                })?)
            }
            PromptCommands::Pick => run_pick(),
        },
        Commands::Init { force, no_backup } => run_init(force, no_backup),
        Commands::Doctor => run_doctor(),
        Commands::Status => print_response(status_request()?),
        Commands::Dictate { command } => match command {
            DictateCommands::Start => print_response(send_request(&ClientRequest::DictateStart)?),
            DictateCommands::Stop => print_response(send_request(&ClientRequest::DictateStop)?),
            DictateCommands::Toggle => print_response(send_request(&ClientRequest::DictateToggle)?),
        },
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
        let summary = prompt_summary(&prompt.template);
        println!("  {:<width$}  {}", prompt.name, summary, width = name_width);
    }
    println!();
    println!("Run one with:   papagaia prompt run <name>");
    println!("Ad-hoc prompt:  papagaia prompt raw --text 'Rewrite this: {{{{text}}}}'");
    println!(
        "Streaming raw:  papagaia prompt raw --text 'Fix this: {{{{text}}}}' --stream-output --strip-markdown-fences false"
    );
    println!("Picker raw:     typing ad-hoc text in the picker streams by default");
    Ok(())
}

fn prompt_summary(template: &str) -> String {
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

fn run_pick() -> Result<()> {
    let config = Config::load()?;
    let entries: Vec<serde_json::Value> = config
        .prompts
        .iter()
        .map(|p| {
            serde_json::json!({
                "name": p.name,
                "summary": prompt_summary(&p.template),
            })
        })
        .collect();
    let entries_json = serde_json::to_string(&entries)?;

    let overlay = overlay_program();
    let mut child = std::process::Command::new(&overlay)
        .arg("--pick")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("failed to launch picker at {}", overlay.display()))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(entries_json.as_bytes())?;
    }

    let output = child.wait_with_output().context("picker process failed")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout = stdout.trim();

    if stdout.is_empty() {
        return Ok(());
    }

    // Give the compositor a moment to return focus to the previous window
    // before the daemon eventually pastes the transformed output.
    thread::sleep(Duration::from_millis(80));

    let result: serde_json::Value =
        serde_json::from_str(stdout).context("failed to parse picker result")?;

    match result.get("type").and_then(|t| t.as_str()) {
        Some("template") => {
            let name = result
                .get("name")
                .and_then(|n| n.as_str())
                .context("picker result missing 'name'")?
                .to_string();
            print_response(send_request(&ClientRequest::Transform {
                prompt: name,
                selected_text: None,
                preserve_selection: false,
            })?)
        }
        Some("raw") => {
            let template = result
                .get("template")
                .and_then(|t| t.as_str())
                .context("picker result missing 'template'")?
                .to_string();
            let strip_markdown_fences = result
                .get("strip-markdown-fences")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let trim_whitespace = result
                .get("trim-whitespace")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let stream_output = result
                .get("stream-output")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            print_response(send_request(&ClientRequest::TransformRaw {
                template,
                selected_text: None,
                preserve_selection: false,
                strip_markdown_fences,
                trim_whitespace,
                stream_output,
            })?)
        }
        _ => bail!("unexpected picker result: {stdout}"),
    }
}

fn overlay_program() -> PathBuf {
    papagaia_core::overlay_program()
}

fn run_restart() -> Result<()> {
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
    let interactive = io::stdin().is_terminal();

    print_detection_summary(&environment);

    let chosen_engine = if interactive {
        choose_engine_interactive(&environment.engine_choices)?
    } else {
        environment.engine_choices.first().cloned()
    };

    let post_process = if chosen_engine.is_some() {
        if interactive {
            ask_yes_no(
                "Enable post-processing of dictation through the LLM engine?",
                true,
            )?
        } else {
            true
        }
    } else {
        false
    };

    let options = InitOptions {
        chosen_engine,
        post_process,
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
        "  whisper:       {}",
        if env.whisper_cli { "yes" } else { "no" }
    );
    if let Some(model) = &env.whisper_model {
        println!("  whisper model: {}", model.display());
    }
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

fn choose_engine_interactive(choices: &[EngineChoice]) -> Result<Option<EngineChoice>> {
    if choices.is_empty() {
        println!("No LLM engines detected on PATH.");
        println!("You will need to configure the [engine] section manually after init.\n");
        return Ok(None);
    }

    if choices.len() == 1 {
        if ask_yes_no(&format!("Use {} as the engine?", choices[0].name), true)? {
            return Ok(Some(choices[0].clone()));
        }
        return Ok(None);
    }

    println!("Select an LLM engine:");
    for (i, choice) in choices.iter().enumerate() {
        println!("  [{}] {}", i + 1, choice.name);
    }
    println!("  [0] none (configure manually)");

    loop {
        print!("Choice [1-{}]: ", choices.len());
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim();

        if input.is_empty() {
            println!("Using {}.\n", choices[0].name);
            return Ok(Some(choices[0].clone()));
        }

        if let Ok(n) = input.parse::<usize>() {
            if n == 0 {
                return Ok(None);
            }
            if n >= 1 && n <= choices.len() {
                println!("Using {}.\n", choices[n - 1].name);
                return Ok(Some(choices[n - 1].clone()));
            }
        }

        println!("Please enter a number between 0 and {}.", choices.len());
    }
}

fn ask_yes_no(question: &str, default: bool) -> Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    print!("{question} {hint} ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim().to_lowercase();

    if input.is_empty() {
        return Ok(default);
    }

    Ok(input.starts_with('y'))
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

    checks.push(DoctorCheck {
        level: CheckLevel::Optional,
        ok: environment.vad_model.is_some(),
        label: "VAD model (silero-vad.onnx)".into(),
        suggestion: Some(
            "download silero-vad.onnx to ~/.local/share/whisper-models/ for voice activity detection (reduces hallucinations on silent audio)".into(),
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
        vad_model: find_vad_model(),
        engine_choices: detect_engine_choices(),
        niri: command_exists("niri"),
        hyprland: command_exists("hyprctl"),
    }
}

fn render_init_config(environment: &DetectedEnvironment, options: &InitOptions) -> String {
    let whisper_model = environment
        .whisper_model
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "~/.local/share/whisper-models/ggml-base.bin".into());
    let (copy_command, paste_command, type_command) = preferred_input_commands(environment);
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
    let window_title_command = if environment.niri {
        toml_array(&["niri", "msg", "-j", "focused-window"])
    } else if environment.hyprland {
        toml_array(&["hyprctl", "activewindow", "-j"])
    } else {
        "[]".into()
    };
    let post_process = if options.post_process { "true" } else { "false" };
    let vad_args = environment
        .vad_model
        .as_ref()
        .map(|path| format!(r#", "--vad", "-vm", "{}""#, path.display()))
        .unwrap_or_default();

    format!(
        r#"logging = false

[tools]
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
argv = ["whisper-cli", "-m", "{{{{model}}}}", "-f", "{{{{audio_path}}}}", "-np", "-nt", "-l", "auto"{vad_args}, "--prompt", "Natural spoken dictation with correct punctuation, natural sentences, and no filler words."]
capture_stdout = true

[dictation]
# Post-process dictation through the LLM engine to clean up transcription.
# When enabled, the whisper transcript is refined by the configured [engine]
# before being typed into the focused window.
post_process = {post_process}
stream_post_process = true
post_process_template = """
You are a voice-to-text post-processor. Your job is to turn raw speech transcription into clean, ready-to-use text.

Rules:
- Fix punctuation, capitalization, and grammar
- Remove filler words and speech artifacts (um, uh, like, you know, so, basically, I mean, right, well, tipo, né, então)
- Remove false starts and repeated words ("I want to I want to go" → "I want to go")
- Interpret voice commands literally: "new line" or "nova linha" → insert a line break, "new paragraph" or "novo parágrafo" → insert two line breaks, "period" or "ponto final" → ".", "comma" or "vírgula" → ","
- Preserve the original language — do not translate
- Preserve the speaker's meaning, intent, and tone
- Do not add, invent, or editorialize any content
- Output only the cleaned text, nothing else — no preamble, no quotes, no explanation
{{{{context}}}}
Transcription:
{{{{text}}}}
"""
# Capture the focused window title before recording starts.
# This context is injected into the post-processing prompt via {{{{context}}}}.
context_awareness = true
window_title_command = {window_title_command}

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
# Optional: set stream_output = true to type text as the engine prints it.
# Streaming prompts must keep strip_markdown_fences = false.

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
                "You transform text. Output only the transformed text, no preamble or explanation.".into(),
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
    let names = ["silero-vad.onnx", "silero_vad.onnx"];
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
            whisper_model: Some("/tmp/model.bin".into()),
            vad_model: None,
            engine_choices: vec![test_engine()],
            niri: true,
            hyprland: false,
        }
    }

    #[test]
    fn init_config_uses_detected_whisper_model() {
        let environment = test_environment();
        let options = InitOptions {
            chosen_engine: Some(test_engine()),
            post_process: true,
        };

        let config = render_init_config(&environment, &options);
        assert!(config.contains("model = \"/tmp/model.bin\""));
        assert!(config.contains(
            "argv = [\"codex\", \"exec\", \"-m\", \"gpt-5.4-mini\", \"--ephemeral\", \"--skip-git-repo-check\", \"-c\", \"model_reasoning_effort=none\", \"-c\", \"model_verbosity=low\", \"-c\", \"model_reasoning_summary=none\", \"-c\", \"hide_agent_reasoning=true\", \"-c\", \"model_instructions_file=\\\"~/.config/papagaia/codex_instructions.md\\\"\", \"-c\", \"sandbox_mode=read-only\", \"-c\", \"approval_policy=never\", \"-c\", \"include_environment_context=false\", \"-c\", \"skills.bundled.enabled=false\", \"--disable\", \"shell_tool\", \"--disable\", \"plugins\", \"--disable\", \"multi_agent\", \"--disable\", \"tool_suggest\", \"--disable\", \"fast_mode\", \"--disable\", \"undo\", \"{{prompt}}\"]"
        ));
        assert!(config.contains("[dictation]"));
        assert!(
            config
                .contains("window_title_command = [\"niri\", \"msg\", \"-j\", \"focused-window\"]")
        );
    }

    #[test]
    fn init_config_respects_post_process_option() {
        let environment = test_environment();

        let enabled = InitOptions {
            chosen_engine: Some(test_engine()),
            post_process: true,
        };
        let config = render_init_config(&environment, &enabled);
        assert!(config.contains("post_process = true"));

        let disabled = InitOptions {
            chosen_engine: Some(test_engine()),
            post_process: false,
        };
        let config = render_init_config(&environment, &disabled);
        assert!(config.contains("post_process = false"));
    }

    #[test]
    fn init_config_context_awareness_enabled_by_default() {
        let environment = test_environment();
        let options = InitOptions {
            chosen_engine: Some(test_engine()),
            post_process: false,
        };
        let config = render_init_config(&environment, &options);
        assert!(config.contains("context_awareness = true"));
    }

    #[test]
    fn init_config_no_engine_uses_placeholder() {
        let environment = test_environment();
        let options = InitOptions {
            chosen_engine: None,
            post_process: false,
        };
        let config = render_init_config(&environment, &options);
        assert!(config.contains("\"your-llm-cli\""));
    }
}
