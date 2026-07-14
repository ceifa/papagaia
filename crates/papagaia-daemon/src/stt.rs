//! Speech-to-text. The `cli` backend reloads the model per call; the `server`
//! backend POSTs the WAV to a warm `whisper-server` (model stays resident) and is
//! much faster. Any non-cancellation server failure falls back to `cli`.
//!
//! The HTTP client is hand-rolled over `TcpStream` to avoid a `reqwest`/`hyper`
//! dependency; the blocking I/O runs on `spawn_blocking` off the current-thread
//! runtime.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use papagaia_core::{WhisperBackend, WhisperConfig};

use crate::{cancel::CancelToken, llm};

/// Path of the whisper-server inference endpoint (its compiled-in default).
const INFERENCE_PATH: &str = "/inference";
/// Fail fast to the CLI fallback when the server isn't accepting connections
/// (e.g. still loading its model right after startup, or not running).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Generous ceiling for the transcription request itself.
const IO_TIMEOUT: Duration = Duration::from_secs(120);

/// Transcribe `audio_path`, preferring the warm server when available and falling
/// back to `whisper-cli` on any non-cancellation error.
pub async fn transcribe(
    whisper: &WhisperConfig,
    server: &Option<Arc<WhisperServer>>,
    audio_path: &Path,
    cancel: &CancelToken,
) -> Result<String> {
    let raw = transcribe_raw(whisper, server, audio_path, cancel).await?;
    // Flatten to one line: whisper emits a segment per pause, but a single
    // utterance shouldn't carry those as line breaks (only voice commands do).
    Ok(normalize_transcript(&raw))
}

async fn transcribe_raw(
    whisper: &WhisperConfig,
    server: &Option<Arc<WhisperServer>>,
    audio_path: &Path,
    cancel: &CancelToken,
) -> Result<String> {
    if let Some(server) = server {
        match server.transcribe(audio_path, cancel).await {
            Ok(text) => return Ok(text),
            Err(error) if cancel.is_cancelled() => return Err(error),
            Err(error) => {
                eprintln!(
                    "papagaia: whisper-server failed, falling back to whisper-cli: {error:#}"
                );
            }
        }
    }
    llm::run_whisper(whisper, audio_path, cancel).await
}

/// Collapse a transcript's inter-segment whitespace (including the newlines a
/// VAD-splitting server inserts) into single spaces.
fn normalize_transcript(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A handle to the warm whisper-server: the endpoint to POST to, plus (when
/// `manage_server` is set) a supervisor thread keeping the child alive.
pub struct WhisperServer {
    endpoint: Endpoint,
}

impl WhisperServer {
    /// Build the server handle for the configured backend. Returns `None` (so the
    /// caller uses the CLI path) when the backend is `cli` or the URL is invalid.
    pub fn launch(whisper: &WhisperConfig) -> Option<Arc<Self>> {
        if whisper.backend != WhisperBackend::Server {
            return None;
        }
        let endpoint = match Endpoint::parse(&whisper.server_url) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                eprintln!("papagaia: invalid whisper.server_url, using whisper-cli: {error:#}");
                return None;
            }
        };
        if whisper.manage_server {
            let argv = llm::render_argv(
                &whisper.server_argv,
                &[("model", &whisper.model), ("prompt", &whisper.prompt)],
            );
            spawn_supervisor(argv);
        }
        Some(Arc::new(Self { endpoint }))
    }

    async fn transcribe(&self, audio_path: &Path, cancel: &CancelToken) -> Result<String> {
        if cancel.is_cancelled() {
            bail!("operation cancelled");
        }
        let endpoint = self.endpoint.clone();
        let audio_path = audio_path.to_path_buf();

        // Run the synchronous POST (file read + socket I/O) on the blocking pool
        // so the current-thread runtime stays responsive.
        let join = tokio::task::spawn_blocking(move || {
            let wav = std::fs::read(&audio_path)
                .with_context(|| format!("failed to read {}", audio_path.display()))?;
            post_inference(&endpoint, &wav)
        });

        // spawn_blocking can't be aborted mid-syscall; on cancel, stop waiting and
        // let the detached task finish harmlessly (it's normally sub-second).
        tokio::select! {
            _ = cancel.cancelled() => bail!("operation cancelled"),
            result = join => result.context("transcription task failed")?,
        }
    }
}

/// Resolved `host:port` plus the `Host:` header value for the configured URL.
#[derive(Clone)]
struct Endpoint {
    addr: SocketAddr,
    host_header: String,
}

impl Endpoint {
    fn parse(url: &str) -> Result<Self> {
        let rest = url.strip_prefix("http://").context(
            "whisper.server_url must start with http:// (https is not supported for the local server)",
        )?;
        let authority = rest.split('/').next().unwrap_or(rest);
        let addr = authority
            .to_socket_addrs()
            .with_context(|| {
                format!("could not resolve whisper.server_url authority '{authority}'")
            })?
            .next()
            .with_context(|| format!("no address resolved for '{authority}'"))?;
        Ok(Self {
            addr,
            host_header: authority.to_string(),
        })
    }
}

/// Keep `whisper-server` alive for the daemon's lifetime, respawning on crash but
/// giving up after repeated immediate failures (bad binary, port busy). The child
/// dies with the daemon via `die_with_parent`.
fn spawn_supervisor(argv: Vec<String>) {
    if argv.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let mut fast_failures = 0;
        loop {
            let started = Instant::now();
            match spawn_server_child(&argv) {
                Some(mut child) => {
                    let _ = child.wait();
                }
                None => break,
            }
            if started.elapsed() < Duration::from_secs(3) {
                fast_failures += 1;
                if fast_failures >= 5 {
                    eprintln!(
                        "papagaia: whisper-server keeps exiting immediately; giving up (using whisper-cli)"
                    );
                    break;
                }
            } else {
                fast_failures = 0;
            }
            std::thread::sleep(Duration::from_secs(2));
            // If we've been reparented to init the daemon is gone; stop respawning.
            if unsafe { libc::getppid() } == 1 {
                break;
            }
        }
    });
}

fn spawn_server_child(argv: &[String]) -> Option<std::process::Child> {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::proc::die_with_parent(&mut command);

    match command.spawn() {
        Ok(child) => Some(child),
        Err(error) => {
            eprintln!("papagaia: failed to spawn whisper-server: {error}");
            None
        }
    }
}

/// Issue the multipart POST and return the transcript text.
fn post_inference(endpoint: &Endpoint, wav: &[u8]) -> Result<String> {
    let boundary = "----papagaiaFormBoundary";
    let body = build_multipart_body(boundary, wav);
    let head = format!(
        "POST {INFERENCE_PATH} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Content-Type: multipart/form-data; boundary={boundary}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\r\n",
        host = endpoint.host_header,
        len = body.len(),
    );

    let mut stream = TcpStream::connect_timeout(&endpoint.addr, CONNECT_TIMEOUT)
        .context("failed to connect to whisper-server")?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.write_all(head.as_bytes())?;
    stream.write_all(&body)?;
    stream.flush()?;

    // `Connection: close` → the server closes after the body, so read to EOF.
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .context("failed to read whisper-server response")?;

    parse_http_response(&response)
}

/// Build a `multipart/form-data` body: the WAV under field `file`, plus
/// `response_format=json` so the reply is always `{"text": ...}`, and a fixed
/// temperature for deterministic output.
fn build_multipart_body(boundary: &str, wav: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(wav.len() + 512);
    body.extend_from_slice(
        format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\n\
             Content-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(wav);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"response_format\"\r\n\r\njson\r\n\
             --{boundary}\r\nContent-Disposition: form-data; name=\"temperature\"\r\n\r\n0.0\r\n\
             --{boundary}--\r\n"
        )
        .as_bytes(),
    );
    body
}

/// Split an HTTP/1.1 response, require a 2xx status, and extract `text` from the
/// JSON body. Any deviation is an error so the caller falls back to the CLI.
fn parse_http_response(raw: &[u8]) -> Result<String> {
    let split = find_subsequence(raw, b"\r\n\r\n")
        .context("malformed HTTP response (no header/body split)")?;
    let header = String::from_utf8_lossy(&raw[..split]);
    let body = &raw[split + 4..];

    let status_line = header.lines().next().unwrap_or_default();
    let ok = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .is_some_and(|code| (200..300).contains(&code));
    if !ok {
        bail!("whisper-server returned a non-2xx status: '{status_line}'");
    }

    let body_text = String::from_utf8_lossy(body);
    let parsed: serde_json::Value = serde_json::from_str(body_text.trim())
        .context("whisper-server returned a non-JSON body")?;
    let text = parsed
        .get("text")
        .and_then(|value| value.as_str())
        .context("whisper-server JSON had no 'text' field")?;
    Ok(text.trim().to_string())
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_parses_url_with_port() {
        let endpoint = Endpoint::parse("http://127.0.0.1:8080").expect("valid url");
        assert_eq!(endpoint.host_header, "127.0.0.1:8080");
        assert_eq!(endpoint.addr.port(), 8080);
    }

    #[test]
    fn endpoint_strips_trailing_path() {
        let endpoint = Endpoint::parse("http://127.0.0.1:9000/inference").expect("valid url");
        assert_eq!(endpoint.host_header, "127.0.0.1:9000");
    }

    #[test]
    fn endpoint_rejects_https() {
        assert!(Endpoint::parse("https://127.0.0.1:8080").is_err());
    }

    #[test]
    fn multipart_body_has_file_and_format_parts() {
        let body = build_multipart_body("BOUND", b"RIFFdata");
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"file\"; filename=\"audio.wav\""));
        assert!(text.contains("RIFFdata"));
        assert!(text.contains("name=\"response_format\""));
        assert!(text.contains("--BOUND--\r\n"));
    }

    #[test]
    fn normalize_transcript_collapses_vad_newlines() {
        assert_eq!(
            normalize_transcript("Cara, então, sobre\n Isso.\n Eu não gostei.\n"),
            "Cara, então, sobre Isso. Eu não gostei."
        );
        assert_eq!(normalize_transcript("  single   line  "), "single line");
    }

    #[test]
    fn parse_http_response_extracts_text() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"text\": \" hello world \"}";
        assert_eq!(parse_http_response(raw).unwrap(), "hello world");
    }

    #[test]
    fn parse_http_response_rejects_non_200() {
        let raw = b"HTTP/1.1 500 Internal Server Error\r\n\r\noops";
        assert!(parse_http_response(raw).is_err());
    }

    #[test]
    fn parse_http_response_rejects_non_json() {
        let raw = b"HTTP/1.1 200 OK\r\n\r\nnot json";
        assert!(parse_http_response(raw).is_err());
    }
}
