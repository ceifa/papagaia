use anyhow::{Context, Result, bail};
use papagaia_core::ToolConfig;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    time::{Duration, sleep},
};

use crate::cancel::CancelToken;

pub async fn capture_selection(tools: &ToolConfig, cancel: &CancelToken) -> Result<String> {
    let before = read_clipboard_text(tools, cancel).await.ok();
    let probe = clipboard_probe_token();

    write_clipboard_text(
        tools,
        &probe,
        ClipboardWriteMode::SensitiveBestEffort,
        cancel,
    )
    .await?;
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
    // Restore the user's pre-probe clipboard. `paste_text` overwrites it again on
    // the happy path; if the op fails before that, this avoids stranding the
    // captured selection on the clipboard.
    restore_clipboard_text(tools, before.as_deref(), cancel).await;
    Ok(text)
}

pub async fn paste_text(tools: &ToolConfig, text: &str, cancel: &CancelToken) -> Result<()> {
    write_clipboard_text(tools, text, ClipboardWriteMode::Default, cancel).await?;
    sleep(Duration::from_millis(30)).await;
    run_command(&tools.paste_command, None, cancel).await?;
    Ok(())
}

pub async fn copy_text(tools: &ToolConfig, text: &str, cancel: &CancelToken) -> Result<()> {
    write_clipboard_text(tools, text, ClipboardWriteMode::Default, cancel).await
}

pub async fn run_command(
    argv: &[String],
    stdin_text: Option<&str>,
    cancel: &CancelToken,
) -> Result<std::process::Output> {
    run_command_checked(argv, stdin_text, cancel, &[]).await
}

/// Like [`run_command`] but tolerates specific non-zero exit codes, returning
/// the output as-is instead of bailing. Other non-zero codes still fail.
pub async fn run_command_allow_exit(
    argv: &[String],
    cancel: &CancelToken,
    allowed_codes: &[i32],
) -> Result<std::process::Output> {
    run_command_checked(argv, None, cancel, allowed_codes).await
}

/// Spawn `argv` (optionally feeding `stdin_text`), collect its output, and fail
/// unless the exit status is success or one of `allowed_codes`.
async fn run_command_checked(
    argv: &[String],
    stdin_text: Option<&str>,
    cancel: &CancelToken,
    allowed_codes: &[i32],
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

    let child = command
        .spawn()
        .with_context(|| format!("failed to spawn {}", argv.join(" ")))?;

    let output = run_child_collecting(child, stdin_text, cancel, argv).await?;

    let code = output.status.code().unwrap_or(-1);
    if !output.status.success() && !allowed_codes.contains(&code) {
        let stderr_text = String::from_utf8_lossy(&output.stderr);
        bail!("{}", command_failure(argv, stderr_text.trim()));
    }

    Ok(output)
}

async fn read_clipboard_text(tools: &ToolConfig, cancel: &CancelToken) -> Result<String> {
    let output = run_command(&tools.read_clipboard_command, None, cancel).await?;
    String::from_utf8(output.stdout).context("clipboard data was not valid UTF-8")
}

async fn restore_clipboard_text(tools: &ToolConfig, text: Option<&str>, cancel: &CancelToken) {
    if let Some(text) = text {
        let _ = write_clipboard_text(tools, text, ClipboardWriteMode::Default, cancel).await;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClipboardWriteMode {
    Default,
    SensitiveBestEffort,
}

async fn write_clipboard_text(
    tools: &ToolConfig,
    text: &str,
    mode: ClipboardWriteMode,
    cancel: &CancelToken,
) -> Result<()> {
    if matches!(mode, ClipboardWriteMode::SensitiveBestEffort)
        && let Some(argv) = with_wl_copy_sensitive_flag(&tools.write_clipboard_command)
        && run_command(&argv, Some(text), cancel).await.is_ok()
    {
        return Ok(());
    }

    run_command(&tools.write_clipboard_command, Some(text), cancel).await?;
    Ok(())
}

fn with_wl_copy_sensitive_flag(argv: &[String]) -> Option<Vec<String>> {
    let (program, rest) = argv.split_first()?;
    if program != "wl-copy" || rest.iter().any(|arg| arg == "--sensitive") {
        return None;
    }

    let mut sensitive = Vec::with_capacity(argv.len() + 1);
    sensitive.push(program.clone());
    sensitive.push("--sensitive".into());
    sensitive.extend(rest.iter().cloned());
    Some(sensitive)
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

/// Spawn `argv`, optionally feed `stdin_text`, and collect stdout/stderr while
/// waiting for exit.
///
/// Reads the pipes and writes borrowed stdin concurrently with the exit wait,
/// so a child that fills a pipe buffer cannot deadlock us — matters for long
/// whisper/engine output. Avoids `wait_with_output()` because `wl-copy` forks a
/// daemon that inherits our pipes and keeps them from reaching EOF; instead we
/// stop at child exit and give the buffer a short bounded drain.
async fn run_child_collecting(
    mut child: Child,
    stdin_text: Option<&str>,
    cancel: &CancelToken,
    argv: &[String],
) -> Result<std::process::Output> {
    let child_stdin = child.stdin.take();
    let write_stdin = async move {
        if let (Some(text), Some(mut stdin)) = (stdin_text, child_stdin) {
            let _ = stdin.write_all(text.as_bytes()).await;
        }
        // Close stdin at EOF, then disable this select branch. Keeping this
        // future local also drops the pipe immediately on cancellation/error.
        std::future::pending::<()>().await;
    };
    tokio::pin!(write_stdin);

    let mut child_stdout = child.stdout.take();
    let mut child_stderr = child.stderr.take();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    let status = loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let _ = child.start_kill();
                // Still reap the child so it doesn't become a zombie.
                let _ = child.wait().await;
                bail!("operation cancelled");
            }
            result = child.wait() => {
                break result.with_context(|| format!("failed to wait for {}", argv.join(" ")))?;
            }
            _ = &mut write_stdin => unreachable!(),
            result = read_pipe_into(&mut child_stdout, &mut stdout) => result?,
            result = read_pipe_into(&mut child_stderr, &mut stderr) => result?,
        }
    };

    // The child has exited; drain whatever is still buffered, bounded by a
    // short timeout (see the wl-copy note above).
    tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("operation cancelled"),
        _ = async {
            tokio::join!(
                append_drained(&mut child_stdout, &mut stdout),
                append_drained(&mut child_stderr, &mut stderr),
            );
        } => {}
    }

    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Read one chunk from an optional pipe directly into `out`, growing it in
/// place (no per-chunk allocation). Clears the pipe on EOF; a `None` pipe never
/// resolves, so its `select!` branch goes quiet once the pipe has closed.
async fn read_pipe_into<R: AsyncRead + Unpin>(
    pipe: &mut Option<R>,
    out: &mut Vec<u8>,
) -> Result<()> {
    match pipe.as_mut() {
        Some(reader) => {
            let read = reader
                .read_buf(out)
                .await
                .context("failed to read command output")?;
            if read == 0 {
                *pipe = None;
            }
            Ok(())
        }
        None => std::future::pending().await,
    }
}

/// Append whatever a pipe still holds after the child exited, time-bounded so a
/// lingering `wl-copy` fork (which inherits the pipe and never closes it) can't
/// hang the read forever.
async fn append_drained<R: AsyncRead + Unpin>(pipe: &mut Option<R>, out: &mut Vec<u8>) {
    if let Some(reader) = pipe.as_mut() {
        let _ = tokio::time::timeout(
            Duration::from_millis(100),
            tokio::io::AsyncReadExt::read_to_end(reader, out),
        )
        .await;
    }
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

    use super::{capture_selection, with_wl_copy_sensitive_flag};

    #[tokio::test]
    async fn collects_large_stdin_and_both_output_pipes_without_deadlock() {
        let input = "long input\n".repeat(100_000);
        let argv = vec![
            "sh".into(),
            "-c".into(),
            "cat; head -c 262144 /dev/zero >&2".into(),
        ];
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            super::run_command(&argv, Some(&input), &CancelToken::new()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(output.stdout, input.as_bytes());
        assert_eq!(output.stderr, vec![0; 262144]);
    }

    #[tokio::test]
    async fn inherited_pipes_do_not_wait_for_a_background_descendant() {
        let argv = vec![
            "sh".into(),
            "-c".into(),
            "sleep 2 & printf out; printf err >&2".into(),
        ];
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            super::run_command(&argv, None, &CancelToken::new()),
        )
        .await
        .expect("must not wait for the descendant's pipes")
        .unwrap();
        assert_eq!(output.stdout, b"out");
        assert_eq!(output.stderr, b"err");
    }

    #[tokio::test]
    async fn cancellation_reaps_child_even_with_blocked_stdin() {
        let child = tokio::process::Command::new("sleep")
            .arg("30")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let cancel = CancelToken::new();
        let input = "x".repeat(1024 * 1024);
        let argv = vec!["sleep".into(), "30".into()];
        let (result, ()) = tokio::join!(
            super::run_child_collecting(child, Some(&input), &cancel, &argv),
            async {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                cancel.cancel();
            },
        );
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[tokio::test]
    async fn short_lived_commands_are_reaped_by_their_tokio_child() {
        for _ in 0..64 {
            let output = super::run_command(&["true".into()], None, &CancelToken::new())
                .await
                .unwrap();
            assert!(output.status.success());
        }
    }

    #[tokio::test]
    #[ignore = "manual subprocess latency benchmark"]
    async fn benchmark_inherited_pipes() {
        let started = std::time::Instant::now();
        for _ in 0..20 {
            let argv = vec![
                "sh".into(),
                "-c".into(),
                "sleep 1 & printf out; printf err >&2".into(),
            ];
            let output = super::run_command(&argv, None, &CancelToken::new())
                .await
                .unwrap();
            assert_eq!(output.stdout, b"out");
            assert_eq!(output.stderr, b"err");
        }
        println!("20 inherited-pipe commands: {:?}", started.elapsed());
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

    #[test]
    fn adds_sensitive_flag_for_wl_copy_probe_writes() {
        let argv = vec!["wl-copy".into(), "--trim-newline".into()];
        assert_eq!(
            with_wl_copy_sensitive_flag(&argv),
            Some(vec![
                "wl-copy".to_string(),
                "--sensitive".to_string(),
                "--trim-newline".to_string(),
            ])
        );
    }

    #[test]
    fn leaves_non_wl_copy_commands_unchanged() {
        let argv = vec!["custom-copy".into(), "--flag".into()];
        assert_eq!(with_wl_copy_sensitive_flag(&argv), None);
    }

    #[test]
    fn does_not_duplicate_sensitive_flag() {
        let argv = vec!["wl-copy".into(), "--sensitive".into()];
        assert_eq!(with_wl_copy_sensitive_flag(&argv), None);
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
