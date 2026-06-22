use std::path::Path;

use std::future::Future;

use anyhow::{Context, Result, bail};
use papagaia_core::{EngineConfig, WhisperConfig};

use crate::{
    cancel::CancelToken,
    clipboard::{run_command, run_command_allow_exit, run_command_streaming},
};

/// Run the engine chain, returning the first engine's output that succeeds.
///
/// On failure the next engine is tried as a fallback. A cancellation is not a
/// fallback trigger — it aborts the chain immediately. If every engine fails,
/// the last error is returned.
pub async fn run_engine(
    engines: &[EngineConfig],
    prompt: &str,
    cancel: &CancelToken,
) -> Result<String> {
    let mut last_error = None;
    for (index, engine) in engines.iter().enumerate() {
        match run_single_engine(engine, prompt, cancel).await {
            Ok(text) => return Ok(text),
            Err(error) => {
                if cancel.is_cancelled() {
                    return Err(error);
                }
                if index + 1 < engines.len() {
                    eprintln!(
                        "papagaia: engine #{} failed, falling back to next: {error:#}",
                        index + 1
                    );
                }
                last_error = Some(error);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no engine configured")))
}

async fn run_single_engine(
    engine: &EngineConfig,
    prompt: &str,
    cancel: &CancelToken,
) -> Result<String> {
    if engine.argv.is_empty() {
        bail!("configured engine has no argv configured");
    }

    let argv = render_argv(&engine.argv, &[("prompt", prompt)]);
    let output = if engine.stdin {
        run_command(&argv, Some(prompt), cancel).await?
    } else {
        run_command(&argv, None, cancel).await?
    };

    if !engine.capture_stdout {
        return Ok(String::new());
    }

    let text =
        String::from_utf8(output.stdout).context("configured engine produced invalid UTF-8")?;
    Ok(clean_engine_output(&text))
}

pub async fn run_engine_streaming<F, Fut>(
    engine: &EngineConfig,
    prompt: &str,
    cancel: &CancelToken,
    on_stdout: F,
) -> Result<()>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    if engine.argv.is_empty() {
        bail!("configured engine has no argv configured");
    }

    // Streaming hands the output to the caller through `on_stdout` as it
    // arrives, so there's nothing useful to return here — the captured-stdout
    // copy the non-streaming path builds would just be discarded.
    let argv = render_argv(&engine.argv, &[("prompt", prompt)]);
    if engine.stdin {
        run_command_streaming(&argv, Some(prompt), cancel, on_stdout).await?;
    } else {
        run_command_streaming(&argv, None, cancel, on_stdout).await?;
    }

    Ok(())
}

/// Exit code whisper-cli returns when VAD detects no speech in the audio.
const WHISPER_EXIT_NO_SPEECH: i32 = 10;

pub async fn run_whisper(
    whisper: &WhisperConfig,
    audio_path: &Path,
    cancel: &CancelToken,
) -> Result<String> {
    let audio_path = audio_path
        .to_str()
        .context("audio path contains non-UTF-8 data")?;
    let argv = render_argv(
        &whisper.argv,
        &[("model", &whisper.model), ("audio_path", audio_path)],
    );
    let output = run_command_allow_exit(&argv, cancel, &[WHISPER_EXIT_NO_SPEECH]).await?;
    if !whisper.capture_stdout {
        return Ok(String::new());
    }

    let stdout = String::from_utf8(output.stdout).context("whisper output was not valid UTF-8")?;
    Ok(clean_whisper_output(&stdout))
}

fn render_argv(argv: &[String], vars: &[(&str, &str)]) -> Vec<String> {
    argv.iter()
        .map(|arg| {
            let mut rendered = arg.clone();
            for (name, value) in vars {
                rendered = rendered.replace(&format!("{{{{{name}}}}}"), value);
            }
            rendered
        })
        .collect()
}

fn clean_engine_output(output: &str) -> String {
    output.trim().to_string()
}

fn clean_whisper_output(output: &str) -> String {
    let cleaned_lines: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with('['))
        .collect();
    cleaned_lines.join(" ")
}

#[cfg(test)]
mod tests {
    use papagaia_core::EngineConfig;

    use crate::cancel::CancelToken;

    use super::{clean_whisper_output, render_argv, run_engine};

    fn engine(argv: &[&str]) -> EngineConfig {
        EngineConfig {
            argv: argv.iter().map(|arg| (*arg).to_string()).collect(),
            stdin: false,
            capture_stdout: true,
        }
    }

    #[test]
    fn renders_placeholders() {
        let argv = vec!["cmd".into(), "{{prompt}}".into()];
        assert_eq!(
            render_argv(&argv, &[("prompt", "hello")]),
            vec!["cmd".to_string(), "hello".to_string()]
        );
    }

    #[test]
    fn strips_whisper_log_lines() {
        let raw = "[00:00:00] loading\nhello\nworld\n";
        assert_eq!(clean_whisper_output(raw), "hello world");
    }

    #[tokio::test]
    async fn run_engine_falls_back_when_first_engine_fails() {
        // The first engine doesn't exist and fails to spawn; the second echoes.
        let engines = vec![
            engine(&["papagaia-nonexistent-engine-binary"]),
            engine(&["echo", "from fallback"]),
        ];

        let output = run_engine(&engines, "ignored", &CancelToken::new())
            .await
            .expect("the fallback engine should succeed");
        assert_eq!(output, "from fallback");
    }

    #[tokio::test]
    async fn run_engine_returns_last_error_when_all_fail() {
        let engines = vec![
            engine(&["papagaia-nonexistent-engine-a"]),
            engine(&["papagaia-nonexistent-engine-b"]),
        ];

        let error = run_engine(&engines, "ignored", &CancelToken::new())
            .await
            .expect_err("an all-failing chain should error");
        assert!(error.to_string().contains("papagaia-nonexistent-engine-b"));
    }
}
