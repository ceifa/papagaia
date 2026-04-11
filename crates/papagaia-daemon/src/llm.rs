use std::path::Path;

use anyhow::{Context, Result, bail};
use papagaia_core::{EngineConfig, WhisperConfig};

use crate::clipboard::run_command;

pub async fn run_engine(engine: &EngineConfig, prompt: &str) -> Result<String> {
    if engine.argv.is_empty() {
        bail!("configured engine has no argv configured");
    }

    let argv = render_argv(&engine.argv, &[("prompt", prompt)]);
    let output = if engine.stdin {
        run_command(&argv, Some(prompt)).await?
    } else {
        run_command(&argv, None).await?
    };

    if !engine.capture_stdout {
        return Ok(String::new());
    }

    let text =
        String::from_utf8(output.stdout).context("configured engine produced invalid UTF-8")?;
    Ok(clean_engine_output(&text))
}

pub async fn run_whisper(whisper: &WhisperConfig, audio_path: &Path) -> Result<String> {
    let audio_path = audio_path
        .to_str()
        .context("audio path contains non-UTF-8 data")?;
    let argv = render_argv(
        &whisper.argv,
        &[("model", &whisper.model), ("audio_path", audio_path)],
    );
    let output = run_command(&argv, None).await?;
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
    use super::{clean_whisper_output, render_argv};

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
}
