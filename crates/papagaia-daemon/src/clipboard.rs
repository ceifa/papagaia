use anyhow::{Context, Result, bail};
use papagaia_core::ToolConfig;
use tokio::{
    io::AsyncRead,
    process::{Child, Command},
    time::{Duration, sleep},
};

use crate::cancel::CancelToken;

pub async fn capture_selection(tools: &ToolConfig, cancel: &CancelToken) -> Result<String> {
    run_command(&tools.copy_command, None, cancel).await?;
    sleep(Duration::from_millis(tools.clipboard_settle_ms)).await;
    let output = run_command(&tools.read_clipboard_command, None, cancel).await?;
    let text = String::from_utf8(output.stdout).context("clipboard data was not valid UTF-8")?;
    if text.trim().is_empty() {
        bail!("clipboard copy produced empty text");
    }
    Ok(text)
}

pub async fn paste_text(tools: &ToolConfig, text: &str, cancel: &CancelToken) -> Result<()> {
    run_command(&tools.write_clipboard_command, Some(text), cancel).await?;
    sleep(Duration::from_millis(30)).await;
    run_command(&tools.paste_command, None, cancel).await?;
    Ok(())
}

pub async fn type_text(tools: &ToolConfig, text: &str, cancel: &CancelToken) -> Result<()> {
    let argv = render_text_command(&tools.type_command, text);
    run_command(&argv, None, cancel).await?;
    Ok(())
}

pub async fn run_command(
    argv: &[String],
    stdin_text: Option<&str>,
    cancel: &CancelToken,
) -> Result<std::process::Output> {
    if cancel.is_cancelled() {
        bail!("operation cancelled");
    }

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
    command.kill_on_drop(true);

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn {}", argv.join(" ")))?;

    if let Some(text) = stdin_text {
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(text.as_bytes()).await?;
        }
    }

    // Do not use wait_with_output() — commands like wl-copy fork a background
    // process that inherits our piped stdout/stderr. wait_with_output waits for
    // pipe EOF which never arrives while the fork lives, hanging the daemon.
    // Instead: wait for the child to exit, then drain whatever the pipes hold.
    let mut child_stdout = child.stdout.take();
    let mut child_stderr = child.stderr.take();

    let status = wait_or_cancel(&mut child, cancel, argv).await?;

    let stdout = drain_pipe(&mut child_stdout).await;
    let stderr = drain_pipe(&mut child_stderr).await;

    if !status.success() {
        let stderr_text = String::from_utf8_lossy(&stderr);
        bail!("{}", command_failure(argv, stderr_text.trim()));
    }

    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Wait for the child to exit, killing it if the cancel token fires.
///
/// We can't use `tokio::select! { _ = child.wait() => .., _ = cancel.cancelled() => child.start_kill() }`
/// directly: `child.wait()` holds a `&mut Child` borrow for the duration of
/// the future, which the borrow checker doesn't release inside the sibling
/// branch even though tokio drops the loser first. Polling `try_wait()` (a
/// synchronous probe) sidesteps the borrow entirely — each iteration touches
/// `child` only between await points.
async fn wait_or_cancel(
    child: &mut Child,
    cancel: &CancelToken,
    argv: &[String],
) -> Result<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) => {
                return Err(error).with_context(|| format!("failed to poll {}", argv.join(" ")));
            }
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                bail!("operation cancelled");
            }
            _ = sleep(Duration::from_millis(40)) => {}
        }
    }
}

/// Read whatever a pipe holds after the child has exited.
///
/// For well-behaved commands the pipe closes with the child and this returns
/// instantly. For commands that fork a background process (e.g. `wl-copy`)
/// the forked child keeps the pipe open — we give it a short grace window and
/// then return whatever was already buffered.
async fn drain_pipe<R: AsyncRead + Unpin>(pipe: &mut Option<R>) -> Vec<u8> {
    let Some(pipe) = pipe.as_mut() else {
        return Vec::new();
    };
    let mut buf = Vec::new();
    match tokio::time::timeout(
        Duration::from_millis(200),
        tokio::io::AsyncReadExt::read_to_end(pipe, &mut buf),
    )
    .await
    {
        Ok(Ok(_)) | Ok(Err(_)) => buf,
        Err(_) => buf, // timeout — return what we have
    }
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
