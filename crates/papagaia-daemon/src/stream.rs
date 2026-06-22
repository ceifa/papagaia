//! Incremental streaming of engine output into the focused window.
//!
//! LLM engines emit text progressively. To type it into the target app as it
//! arrives, [`stream_prompt_output`] drives the engine, normalizes each chunk
//! (stripping ANSI escapes, resolving cumulative/overlapping re-emissions,
//! trimming outer whitespace), and pastes the cleaned delta via the clipboard.

use std::sync::{Arc, Mutex as StdMutex};

use anyhow::Result;
use papagaia_core::{EngineConfig, ToolConfig};
use tokio::time::{Duration, sleep};

use crate::{cancel::CancelToken, clipboard, llm};

/// Streams the engine chain output to the target window. Returns the text
/// emitted so far together with the streaming result. The emitted text is
/// returned even on error so the caller can tell whether anything was already
/// pasted (a failed stream that emitted nothing can safely fall back to the raw
/// transcript).
///
/// Engines are tried in order. A failure only falls back to the next engine
/// while nothing has been pasted yet: once any text has landed in the target
/// window, a mid-stream failure is returned as-is so the output isn't typed
/// twice. Cancellation is never a fallback trigger.
pub async fn stream_prompt_output(
    tools: &ToolConfig,
    engines: &[EngineConfig],
    rendered_prompt: &str,
    cancel: &CancelToken,
) -> (String, Result<()>) {
    if engines.is_empty() {
        return (String::new(), Err(anyhow::anyhow!("no engine configured")));
    }

    for (index, engine) in engines.iter().enumerate() {
        let is_last = index + 1 == engines.len();
        let (emitted, result) = stream_single_engine(tools, engine, rendered_prompt, cancel).await;

        match &result {
            Ok(()) => return (emitted, result),
            Err(error) => {
                // Don't fall back if this is the final engine, if the user
                // cancelled, or if partial output already reached the window
                // (re-running would duplicate it).
                if is_last || cancel.is_cancelled() || !emitted.is_empty() {
                    return (emitted, result);
                }
                eprintln!(
                    "papagaia: engine #{} (streaming) failed, falling back to next: {error:#}",
                    index + 1
                );
            }
        }
    }

    unreachable!("the final engine always returns above");
}

async fn stream_single_engine(
    tools: &ToolConfig,
    engine: &EngineConfig,
    rendered_prompt: &str,
    cancel: &CancelToken,
) -> (String, Result<()>) {
    let state = Arc::new(StdMutex::new(StreamOutputState::new()));
    let tools_for_tail = tools.clone();
    let cancel_for_tail = cancel.clone();
    let callback_tools = tools.clone();
    let callback_cancel = cancel.clone();
    let callback_state = state.clone();

    let stream_result =
        llm::run_engine_streaming(engine, rendered_prompt, &cancel_for_tail, move |chunk| {
            let tools = callback_tools.clone();
            let cancel = callback_cancel.clone();
            let callback_state = callback_state.clone();
            async move {
                let flushed = {
                    let mut state = callback_state
                        .lock()
                        .expect("streaming output state lock poisoned");
                    state.push(&chunk)
                };
                if !flushed.is_empty() {
                    // Clipboard paste (not direct wtype) — wtype relies on
                    // virtual-keyboard keysyms which don't cover codepoints above
                    // the BMP, so emojis and other astral-plane chars get dropped
                    // or substituted. Clipboard paste round-trips raw UTF-8.
                    //
                    // The overlay intentionally stays visible ("Processing…")
                    // during streaming paste. Unmapping the layer surface between
                    // chunks disturbs the target window's keyboard focus on niri
                    // and swallows the Ctrl+V.
                    clipboard::paste_text(&tools, &flushed, &cancel).await?;
                    sleep(Duration::from_millis(28)).await;
                }
                Ok(())
            }
        })
        .await;

    let (tail, emitted) = {
        let mut state = state.lock().expect("streaming output state lock poisoned");
        let tail = state.finish();
        let emitted = state.emitted.clone();
        (tail, emitted)
    };

    let result = match stream_result {
        Ok(_) => {
            if tail.is_empty() {
                Ok(())
            } else {
                clipboard::paste_text(&tools_for_tail, &tail, &cancel_for_tail).await
            }
        }
        Err(error) => Err(error),
    };

    (emitted, result)
}

struct StreamOutputState {
    saw_non_whitespace: bool,
    pending_whitespace: String,
    escape_state: EscapeState,
    observed_sanitized: String,
    pending_flush: String,
    emitted: String,
}

impl StreamOutputState {
    fn new() -> Self {
        Self {
            saw_non_whitespace: false,
            pending_whitespace: String::new(),
            escape_state: EscapeState::None,
            observed_sanitized: String::new(),
            pending_flush: String::new(),
            emitted: String::new(),
        }
    }

    fn push(&mut self, chunk: &str) -> String {
        let sanitized = self.sanitize_chunk(chunk);
        let raw_delta = compute_stream_delta(&self.observed_sanitized, &sanitized);
        if raw_delta.is_empty() {
            return String::new();
        }
        self.observed_sanitized.push_str(&raw_delta);

        let cleaned = self.trimmed_chunk(&raw_delta);
        if cleaned.is_empty() {
            return String::new();
        }

        self.emitted.push_str(&cleaned);
        self.pending_flush.push_str(&cleaned);
        if self.should_flush() {
            return std::mem::take(&mut self.pending_flush);
        }

        String::new()
    }

    fn finish(&mut self) -> String {
        self.pending_whitespace.clear();
        std::mem::take(&mut self.pending_flush)
    }

    fn trimmed_chunk(&mut self, chunk: &str) -> String {
        let mut out = String::new();

        for ch in chunk.chars() {
            if !self.saw_non_whitespace {
                if ch.is_whitespace() {
                    continue;
                }
                self.saw_non_whitespace = true;
            }

            if ch.is_whitespace() {
                self.pending_whitespace.push(ch);
            } else {
                if !self.pending_whitespace.is_empty() {
                    out.push_str(&self.pending_whitespace);
                    self.pending_whitespace.clear();
                }
                out.push(ch);
            }
        }

        out
    }

    fn sanitize_chunk(&mut self, chunk: &str) -> String {
        let mut out = String::new();

        for ch in chunk.chars() {
            match self.escape_state {
                EscapeState::None => {}
                EscapeState::Started => {
                    self.escape_state = if ch == '[' {
                        EscapeState::Csi
                    } else {
                        EscapeState::None
                    };
                    continue;
                }
                EscapeState::Csi => {
                    if ('@'..='~').contains(&ch) {
                        self.escape_state = EscapeState::None;
                    }
                    continue;
                }
            }

            match ch {
                '\u{1b}' => {
                    self.escape_state = EscapeState::Started;
                }
                '\r' => {}
                '\u{8}' | '\u{7f}' => {
                    out.pop();
                }
                _ if ch.is_control() && ch != '\n' && ch != '\t' => {}
                _ => out.push(ch),
            }
        }

        out
    }

    fn should_flush(&self) -> bool {
        self.pending_flush.contains('\n')
            || self.pending_flush.len() >= 64
            || (self.pending_flush.len() >= 24
                && self
                    .pending_flush
                    .chars()
                    .last()
                    .is_some_and(|ch| ch.is_whitespace() || ",.;:!?)]}".contains(ch)))
    }
}

#[derive(Clone, Copy)]
enum EscapeState {
    None,
    Started,
    Csi,
}

fn compute_stream_delta(emitted: &str, chunk: &str) -> String {
    if chunk.is_empty() {
        return String::new();
    }
    if emitted.is_empty() {
        return chunk.to_string();
    }
    if emitted.starts_with(chunk) || emitted.ends_with(chunk) {
        return String::new();
    }
    if let Some(rest) = chunk.strip_prefix(emitted) {
        return rest.to_string();
    }

    // Find the longest suffix of `emitted` that is a prefix of `chunk`. Walk
    // lengths from longest to shortest so we can stop at the first match.
    // Reached only when the early-exit cases above don't fire — for typical
    // cumulative LLM output `strip_prefix` handles it without entering here.
    let emitted_bytes = emitted.as_bytes();
    let chunk_bytes = chunk.as_bytes();
    let max_overlap = emitted_bytes.len().min(chunk_bytes.len());

    let mut overlap = 0;
    for len in (1..=max_overlap).rev() {
        if emitted_bytes.ends_with(&chunk_bytes[..len]) && chunk.is_char_boundary(len) {
            overlap = len;
            break;
        }
    }

    chunk[overlap..].to_string()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use papagaia_core::{EngineConfig, ToolConfig};

    use crate::cancel::CancelToken;

    use super::{StreamOutputState, compute_stream_delta, stream_prompt_output};

    #[test]
    fn streaming_trim_drops_outer_whitespace() {
        let mut state = StreamOutputState::new();

        assert_eq!(state.push("  hello"), "");
        assert_eq!(state.push(" world  "), "");
        assert_eq!(state.finish(), "hello world");
        assert_eq!(state.emitted, "hello world");
    }

    #[test]
    fn compute_stream_delta_handles_cumulative_chunks() {
        assert_eq!(compute_stream_delta("h", "he"), "e");
        assert_eq!(compute_stream_delta("hello", "hello world"), " world");
    }

    #[test]
    fn compute_stream_delta_handles_overlapping_chunks() {
        assert_eq!(compute_stream_delta("hello", "lo world"), " world");
        assert_eq!(compute_stream_delta("hello world", "world"), "");
    }

    #[test]
    fn streaming_state_strips_ansi_sequences() {
        let mut state = StreamOutputState::new();
        assert_eq!(state.push("\u{1b}[2Khello"), "");
        assert_eq!(state.finish(), "hello");
    }

    #[tokio::test]
    async fn stream_prompt_output_types_exact_delta_once() {
        let dir = make_test_dir("stream-delta");
        let clipboard_script = dir.join("clipboard.sh");
        let engine_script = dir.join("engine.sh");
        let out_path = dir.join("typed.txt");
        fs::write(&out_path, "").expect("output file should be created");

        write_executable(
            &clipboard_script,
            r#"#!/usr/bin/env bash
set -euo pipefail
cat >> "$1"
"#,
        );
        write_executable(
            &engine_script,
            r#"#!/usr/bin/env bash
set -euo pipefail
printf '\033[2K'
printf 'The quick '
sleep 0.05
printf 'The quick brown '
sleep 0.05
printf 'own fox'
"#,
        );

        let tools = fake_tools(&clipboard_script, &out_path);
        let engine = EngineConfig {
            argv: vec![engine_script.display().to_string()],
            stdin: false,
            capture_stdout: true,
        };

        let (emitted, result) =
            stream_prompt_output(&tools, &[engine], "ignored", &CancelToken::new()).await;
        result.expect("streaming should succeed");
        let typed = fs::read_to_string(&out_path).expect("typed output should exist");

        assert_eq!(emitted, "The quick brown fox");
        assert_eq!(typed, "The quick brown fox");
    }

    #[tokio::test]
    async fn stream_prompt_output_handles_backspace_and_overlap() {
        let dir = make_test_dir("stream-backspace");
        let clipboard_script = dir.join("clipboard.sh");
        let engine_script = dir.join("engine.sh");
        let out_path = dir.join("typed.txt");
        fs::write(&out_path, "").expect("output file should be created");

        write_executable(
            &clipboard_script,
            r#"#!/usr/bin/env bash
set -euo pipefail
cat >> "$1"
"#,
        );
        write_executable(
            &engine_script,
            r#"#!/usr/bin/env bash
set -euo pipefail
printf 'hel'
sleep 0.05
printf 'hello '
sleep 0.05
printf 'o worl'
sleep 0.05
printf 'world!\b!'
"#,
        );

        let tools = fake_tools(&clipboard_script, &out_path);
        let engine = EngineConfig {
            argv: vec![engine_script.display().to_string()],
            stdin: false,
            capture_stdout: true,
        };

        let (emitted, result) =
            stream_prompt_output(&tools, &[engine], "ignored", &CancelToken::new()).await;
        result.expect("streaming should succeed");
        let typed = fs::read_to_string(&out_path).expect("typed output should exist");

        assert_eq!(emitted, "hello world!");
        assert_eq!(typed, "hello world!");
    }

    fn fake_tools(clipboard_script: &Path, out_path: &Path) -> ToolConfig {
        ToolConfig {
            read_clipboard_command: vec!["true".into()],
            write_clipboard_command: vec![
                clipboard_script.display().to_string(),
                out_path.display().to_string(),
            ],
            copy_command: vec!["true".into()],
            paste_command: vec!["true".into()],
            clipboard_settle_ms: 0,
        }
    }

    fn make_test_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be monotonic enough for tests")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("papagaia-{label}-{nonce}"));
        fs::create_dir_all(&dir).expect("test dir should be created");
        dir
    }

    fn write_executable(path: &Path, script: &str) {
        fs::write(path, script).expect("script should be written");
        let mut perms = fs::metadata(path)
            .expect("script metadata should exist")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).expect("script should be executable");
    }
}
