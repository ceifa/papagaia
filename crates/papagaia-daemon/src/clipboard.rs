use std::future::Future;

use anyhow::{Context, Result, bail};
use papagaia_core::ToolConfig;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    time::{Duration, sleep},
};

use crate::cancel::CancelToken;

pub async fn capture_selection(tools: &ToolConfig, cancel: &CancelToken) -> Result<String> {
    let before = read_clipboard_text(tools, cancel).await.ok();
    let probe = clipboard_probe_token();

    run_command(&tools.write_clipboard_command, Some(&probe), cancel).await?;
    run_command(&tools.copy_command, None, cancel).await?;
    sleep(Duration::from_millis(tools.clipboard_settle_ms)).await;
    let text = read_clipboard_text(tools, cancel).await?;
    if text.trim().is_empty() {
        restore_clipboard_text(tools, before.as_deref(), cancel).await;
        bail!("no text was selected");
    }
    if text == probe {
        restore_clipboard_text(tools, before.as_deref(), cancel).await;
        bail!("no text was selected");
    }
    Ok(text)
}

pub async fn paste_text(tools: &ToolConfig, text: &str, cancel: &CancelToken) -> Result<()> {
    run_command(&tools.write_clipboard_command, Some(text), cancel).await?;
    sleep(Duration::from_millis(30)).await;
    run_command(&tools.paste_command, None, cancel).await?;
    Ok(())
}

#[allow(dead_code)]
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

    if let Some(text) = stdin_text
        && let Some(mut stdin) = child.stdin.take()
    {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(text.as_bytes()).await?;
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

pub async fn run_command_streaming<F, Fut>(
    argv: &[String],
    stdin_text: Option<&str>,
    cancel: &CancelToken,
    mut on_stdout: F,
) -> Result<std::process::Output>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<()>>,
{
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

    if let Some(text) = stdin_text
        && let Some(mut stdin) = child.stdin.take()
    {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(text.as_bytes()).await?;
    }

    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut stdout_bytes = Vec::new();
    let mut pending_utf8 = Vec::new();
    let mut exit_status = None;

    while exit_status.is_none() {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("failed to poll {}", argv.join(" ")))?
        {
            exit_status = Some(status);
            break;
        }

        // Read raw bytes inside the select! so cancellation and the periodic
        // child.try_wait() poll stay responsive. The callback is deliberately
        // invoked OUTSIDE the select!: tokio::select! drops the losing branch,
        // and a slow callback (e.g. one that awaits wtype) would get cancelled
        // mid-flight whenever the 40ms timer branch won the race.
        let bytes = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                bail!("operation cancelled");
            }
            read = read_stdout_bytes(&mut stdout) => read?,
            _ = sleep(Duration::from_millis(40)) => continue,
        };

        if let Some(bytes) = bytes {
            stdout_bytes.extend_from_slice(&bytes);
            pending_utf8.extend_from_slice(&bytes);
            flush_valid_utf8(&mut pending_utf8, &mut on_stdout).await?;
        }
    }

    if exit_status.is_some() {
        drain_streaming_stdout(
            &mut stdout,
            &mut stdout_bytes,
            &mut pending_utf8,
            &mut on_stdout,
        )
        .await?;
    }

    if !pending_utf8.is_empty() {
        bail!("command produced invalid UTF-8 on stdout");
    }

    let status = exit_status.expect("exit status must be set before draining stdout");
    let stderr = drain_pipe(&mut stderr).await;

    if !status.success() {
        let stderr_text = String::from_utf8_lossy(&stderr);
        bail!("{}", command_failure(argv, stderr_text.trim()));
    }

    Ok(std::process::Output {
        status,
        stdout: stdout_bytes,
        stderr,
    })
}

async fn read_clipboard_text(tools: &ToolConfig, cancel: &CancelToken) -> Result<String> {
    let output = run_command(&tools.read_clipboard_command, None, cancel).await?;
    String::from_utf8(output.stdout).context("clipboard data was not valid UTF-8")
}

async fn restore_clipboard_text(tools: &ToolConfig, text: Option<&str>, cancel: &CancelToken) {
    if let Some(text) = text {
        let _ = run_command(&tools.write_clipboard_command, Some(text), cancel).await;
    }
}

fn clipboard_probe_token() -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "__papagaia_selection_probe_{}_{}__",
        std::process::id(),
        nonce
    )
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

/// Read the next chunk of stdout bytes. Returns `Ok(None)` when the pipe
/// closes. Never invokes the streaming callback — callers should run the
/// callback outside of any `select!`/`timeout` wrapper, otherwise a slow
/// callback gets dropped mid-flight when the enclosing future loses a race.
async fn read_stdout_bytes(
    child_stdout: &mut Option<tokio::process::ChildStdout>,
) -> Result<Option<Vec<u8>>> {
    let Some(stdout) = child_stdout.as_mut() else {
        sleep(Duration::from_millis(40)).await;
        return Ok(None);
    };

    let mut buf = [0_u8; 1024];
    match stdout.read(&mut buf).await {
        Ok(0) => {
            *child_stdout = None;
            Ok(None)
        }
        Ok(read) => Ok(Some(buf[..read].to_vec())),
        Err(error) => Err(error).context("failed to read command stdout"),
    }
}

async fn drain_streaming_stdout<F, Fut>(
    stdout: &mut Option<tokio::process::ChildStdout>,
    stdout_bytes: &mut Vec<u8>,
    pending_utf8: &mut Vec<u8>,
    on_stdout: &mut F,
) -> Result<()>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    while stdout.is_some() {
        match tokio::time::timeout(Duration::from_millis(200), read_stdout_bytes(stdout)).await {
            Ok(Ok(Some(bytes))) => {
                stdout_bytes.extend_from_slice(&bytes);
                pending_utf8.extend_from_slice(&bytes);
                flush_valid_utf8(pending_utf8, on_stdout).await?;
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => break,
        }
    }
    Ok(())
}

async fn flush_valid_utf8<F, Fut>(pending_utf8: &mut Vec<u8>, on_stdout: &mut F) -> Result<()>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let Some(text) = take_valid_utf8_prefix(pending_utf8)? else {
        return Ok(());
    };
    if text.is_empty() {
        return Ok(());
    }
    on_stdout(text).await
}

fn take_valid_utf8_prefix(buffer: &mut Vec<u8>) -> Result<Option<String>> {
    match std::str::from_utf8(buffer) {
        Ok(text) => {
            let text = text.to_string();
            buffer.clear();
            Ok(Some(text))
        }
        Err(error) => {
            let valid_up_to = error.valid_up_to();
            if valid_up_to == 0 {
                if error.error_len().is_none() {
                    return Ok(None);
                }
                bail!("command produced invalid UTF-8 on stdout");
            }

            let text = std::str::from_utf8(&buffer[..valid_up_to])
                .expect("valid UTF-8 prefix")
                .to_string();
            let rest = buffer[valid_up_to..].to_vec();
            *buffer = rest;
            Ok(Some(text))
        }
    }
}

#[allow(dead_code)]
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
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use papagaia_core::ToolConfig;

    use crate::cancel::CancelToken;

    use super::{capture_selection, render_text_command};

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

    #[tokio::test]
    async fn capture_selection_accepts_same_text_as_existing_clipboard() {
        let dir = make_test_dir("capture-selection-same-text");
        let clipboard_script = dir.join("clipboard.sh");
        let clipboard_path = dir.join("clipboard.txt");
        let selection_path = dir.join("selection.txt");

        write_executable(
            &clipboard_script,
            r#"#!/usr/bin/env bash
set -euo pipefail
mode="$1"
clipboard="$2"
selection="$3"
case "$mode" in
  read)
    [[ -f "$clipboard" ]] && cat "$clipboard"
    ;;
  write)
    cat > "$clipboard"
    ;;
  copy)
    if [[ -s "$selection" ]]; then
      cat "$selection" > "$clipboard"
    fi
    ;;
  *)
    exit 1
    ;;
esac
"#,
        );

        fs::write(&clipboard_path, "same text").expect("clipboard should be written");
        fs::write(&selection_path, "same text").expect("selection should be written");

        let selected = capture_selection(
            &fake_tools(&clipboard_script, &clipboard_path, &selection_path),
            &CancelToken::new(),
        )
        .await
        .expect("selection should be captured");

        assert_eq!(selected, "same text");
        assert_eq!(
            fs::read_to_string(&clipboard_path).expect("clipboard should be readable"),
            "same text"
        );
    }

    #[tokio::test]
    async fn capture_selection_rejects_missing_selection_and_restores_clipboard() {
        let dir = make_test_dir("capture-selection-missing");
        let clipboard_script = dir.join("clipboard.sh");
        let clipboard_path = dir.join("clipboard.txt");
        let selection_path = dir.join("selection.txt");

        write_executable(
            &clipboard_script,
            r#"#!/usr/bin/env bash
set -euo pipefail
mode="$1"
clipboard="$2"
selection="$3"
case "$mode" in
  read)
    [[ -f "$clipboard" ]] && cat "$clipboard"
    ;;
  write)
    cat > "$clipboard"
    ;;
  copy)
    if [[ -s "$selection" ]]; then
      cat "$selection" > "$clipboard"
    fi
    ;;
  *)
    exit 1
    ;;
esac
"#,
        );

        fs::write(&clipboard_path, "original clipboard").expect("clipboard should be written");
        fs::write(&selection_path, "").expect("selection should be empty");

        let error = capture_selection(
            &fake_tools(&clipboard_script, &clipboard_path, &selection_path),
            &CancelToken::new(),
        )
        .await
        .expect_err("missing selection should fail");

        assert!(error.to_string().contains("no text was selected"));
        assert_eq!(
            fs::read_to_string(&clipboard_path).expect("clipboard should be restored"),
            "original clipboard"
        );
    }

    fn fake_tools(
        clipboard_script: &Path,
        clipboard_path: &Path,
        selection_path: &Path,
    ) -> ToolConfig {
        ToolConfig {
            read_clipboard_command: vec![
                clipboard_script.display().to_string(),
                "read".into(),
                clipboard_path.display().to_string(),
                selection_path.display().to_string(),
            ],
            write_clipboard_command: vec![
                clipboard_script.display().to_string(),
                "write".into(),
                clipboard_path.display().to_string(),
                selection_path.display().to_string(),
            ],
            copy_command: vec![
                clipboard_script.display().to_string(),
                "copy".into(),
                clipboard_path.display().to_string(),
                selection_path.display().to_string(),
            ],
            paste_command: vec!["true".into()],
            type_command: vec!["true".into()],
            clipboard_settle_ms: 0,
        }
    }

    fn make_test_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be monotonic enough for tests")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("papagaia-clipboard-{label}-{nonce}"));
        fs::create_dir_all(&dir).expect("test dir should be created");
        dir
    }

    fn write_executable(path: &Path, script: &str) {
        fs::write(path, script).expect("script should be written");
        let mut perms = fs::metadata(path)
            .expect("script metadata should exist")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).expect("script permissions should be updated");
    }
}
