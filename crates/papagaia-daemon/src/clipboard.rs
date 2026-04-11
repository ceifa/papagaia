use anyhow::{Context, Result, bail};
use papagaia_core::ToolConfig;
use tokio::{
    process::Command,
    time::{Duration, sleep},
};

pub async fn capture_selection(tools: &ToolConfig) -> Result<String> {
    run_command(&tools.copy_command, None).await?;
    sleep(Duration::from_millis(tools.clipboard_settle_ms)).await;
    let output = run_command(&tools.read_clipboard_command, None).await?;
    let text = String::from_utf8(output.stdout).context("clipboard data was not valid UTF-8")?;
    if text.trim().is_empty() {
        bail!("clipboard copy produced empty text");
    }
    Ok(text)
}

pub async fn replace_selection(tools: &ToolConfig, text: &str) -> Result<()> {
    run_command(&tools.write_clipboard_command, Some(text)).await?;
    sleep(Duration::from_millis(30)).await;
    run_command(&tools.paste_command, None).await?;
    Ok(())
}

pub async fn type_text(tools: &ToolConfig, text: &str) -> Result<()> {
    let argv = render_text_command(&tools.type_command, text);
    run_command(&argv, None).await?;
    Ok(())
}

pub async fn run_command(
    argv: &[String],
    stdin_text: Option<&str>,
) -> Result<std::process::Output> {
    let Some(program) = argv.first() else {
        bail!("cannot run an empty command");
    };

    let mut command = Command::new(program);
    command.args(&argv[1..]);

    if stdin_text.is_some() {
        command.stdin(std::process::Stdio::piped());
    }
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn {}", argv.join(" ")))?;

    if let Some(text) = stdin_text {
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(text.as_bytes()).await?;
        }
    }

    let output = child
        .wait_with_output()
        .await
        .with_context(|| format!("failed to wait for {}", argv.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{}", command_failure(argv, stderr.trim()));
    }

    Ok(output)
}

fn render_text_command(argv: &[String], text: &str) -> Vec<String> {
    let mut rendered = Vec::with_capacity(argv.len() + 1);
    let mut used_placeholder = false;

    for arg in argv {
        if arg.contains("{{text}}") {
            rendered.push(arg.replace("{{text}}", text));
            used_placeholder = true;
        } else {
            rendered.push(arg.clone());
        }
    }

    if !used_placeholder {
        rendered.push(text.to_string());
    }

    rendered
}

fn command_failure(argv: &[String], stderr: &str) -> String {
    let command = argv.join(" ");
    let details = if stderr.is_empty() {
        "command returned a non-zero exit status"
    } else {
        stderr
    };

    if matches!(argv.first().map(String::as_str), Some("ydotool")) {
        return format!(
            "command failed: {command}: {details}. If ydotool is installed but not working, make sure `ydotoold` is running."
        );
    }

    if matches!(argv.first().map(String::as_str), Some("wtype")) {
        return format!(
            "command failed: {command}: {details}. This usually means `wtype` is missing or virtual keyboard input is not available in the current Wayland session."
        );
    }

    format!("command failed: {command}: {details}")
}

#[cfg(test)]
mod tests {
    use super::render_text_command;

    #[test]
    fn renders_text_placeholder() {
        let argv = vec!["wtype".into(), "{{text}}".into()];
        assert_eq!(
            render_text_command(&argv, "hello"),
            vec!["wtype".to_string(), "hello".to_string()]
        );
    }

    #[test]
    fn appends_text_when_placeholder_is_missing() {
        let argv = vec!["some-tool".into()];
        assert_eq!(
            render_text_command(&argv, "hello"),
            vec!["some-tool".to_string(), "hello".to_string()]
        );
    }
}
