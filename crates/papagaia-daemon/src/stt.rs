//! Speech-to-text. The `cli` backend reloads the model per call; the `server`
//! backend POSTs the WAV to a warm `whisper-server` (model stays resident) and is
//! much faster. Any non-cancellation server failure falls back to `cli`.
//!
//! The HTTP client is hand-rolled over `TcpStream` to avoid a `reqwest`/`hyper`
//! dependency. Audio is streamed from disk, and socket I/O is asynchronous so
//! cancellation closes the connection without leaving a blocking request behind.

use std::{
    net::{SocketAddr, ToSocketAddrs},
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use papagaia_core::{WhisperBackend, WhisperConfig};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

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

/// Flatten a multi-segment transcript to a single line.
///
/// whisper-server writes one line per segment and wraps segments at 60 characters,
/// so a wrap can fall inside a word — joining those lines with a space is what
/// turns "enviar" into "env iar". Whisper's own spacing says where the words are:
/// a segment beginning a new word starts with a space, one continuing the previous
/// word does not. Join on that signal rather than on the newline.
fn normalize_transcript(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for segment in text.split('\n') {
        // Skip blank segments (the trailing newline leaves one) so they can't
        // strand a separator space with no word after it.
        if segment.trim().is_empty() {
            continue;
        }
        if !out.is_empty() && segment.starts_with(char::is_whitespace) {
            out.push(' ');
        }
        // Appending word by word also collapses whitespace runs inside a segment.
        for (index, word) in segment.split_whitespace().enumerate() {
            if index > 0 {
                out.push(' ');
            }
            out.push_str(word);
        }
    }
    out
}

/// Shortest prompt recital treated as an echo; a term or two said alone is real.
const MIN_ECHO_WORDS: usize = 3;

/// Whisper answers silence or noise by reciting its initial prompt. A transcript
/// that is nothing but a run of the prompt's words is that echo, not dictation.
pub fn echoes_prompt(transcript: &str, prompt: &str) -> bool {
    fn words(text: &str) -> Vec<String> {
        text.split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .collect()
    }
    let heard = words(transcript);
    heard.len() >= MIN_ECHO_WORDS && words(prompt).windows(heard.len()).any(|run| run == heard)
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
                &[
                    ("model", &whisper.model),
                    ("prompt", &whisper.initial_prompt()),
                ],
            );
            spawn_supervisor(argv);
        }
        Some(Arc::new(Self { endpoint }))
    }

    async fn transcribe(&self, audio_path: &Path, cancel: &CancelToken) -> Result<String> {
        if cancel.is_cancelled() {
            bail!("operation cancelled");
        }
        // Dropping the request closes the socket/file on cancellation. The
        // deadline covers the entire upload + response, including slow peers.
        tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("operation cancelled"),
            result = timeout(IO_TIMEOUT, post_inference(&self.endpoint, audio_path)) => {
                result.context("whisper-server request timed out")?
            }
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

/// Issue the multipart POST without loading or copying the WAV into memory.
async fn post_inference(endpoint: &Endpoint, audio_path: &Path) -> Result<String> {
    let mut stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(endpoint.addr))
        .await
        .context("connection to whisper-server timed out")?
        .context("failed to connect to whisper-server")?;
    let wav = tokio::fs::File::open(audio_path)
        .await
        .with_context(|| format!("failed to read {}", audio_path.display()))?;
    let wav_len = wav.metadata().await?.len();
    let boundary = "----papagaiaFormBoundary";
    let (prefix, suffix) = multipart_parts(boundary);
    let head = format!(
        "POST {INFERENCE_PATH} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Content-Type: multipart/form-data; boundary={boundary}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\r\n",
        host = endpoint.host_header,
        len = prefix.len() as u64 + wav_len + suffix.len() as u64,
    );

    stream.write_all(head.as_bytes()).await?;
    stream.write_all(prefix.as_bytes()).await?;
    // Larger disk reads amortize blocking-pool handoffs, while copy_buf writes
    // directly from this fixed buffer instead of allocating a second WAV copy.
    let mut wav = tokio::io::BufReader::with_capacity(64 * 1024, wav.take(wav_len));
    let copied = tokio::io::copy_buf(&mut wav, &mut stream).await?;
    if copied != wav_len {
        bail!("recording changed during upload");
    }
    stream.write_all(suffix.as_bytes()).await?;
    stream.flush().await?;

    // `Connection: close` → the server closes after the body, so read to EOF.
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .context("failed to read whisper-server response")?;

    parse_http_response(&response)
}

/// Framing for the WAV plus response format and decoding options. Keeping the
/// file separate bounds upload memory independently of recording duration.
fn multipart_parts(boundary: &str) -> (String, String) {
    let prefix = format!(
        "--{boundary}\r\n\
         Content-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\n\
         Content-Type: audio/wav\r\n\r\n"
    );
    let suffix = format!(
        "\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"response_format\"\r\n\r\njson\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"temperature\"\r\n\r\n0.0\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"split_on_word\"\r\n\r\ntrue\r\n\
         --{boundary}--\r\n"
    );
    (prefix, suffix)
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

    struct AudioFixture(std::path::PathBuf);

    impl AudioFixture {
        fn new(len: u64) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "papagaia-upload-test-{}-{id}.wav",
                std::process::id()
            ));
            std::fs::File::create(&path).unwrap().set_len(len).unwrap();
            Self(path)
        }
    }

    impl Drop for AudioFixture {
        fn drop(&mut self) {
            std::fs::remove_file(&self.0).unwrap();
        }
    }

    async fn request_body(
        socket: tokio::net::TcpStream,
    ) -> (tokio::io::BufReader<tokio::net::TcpStream>, u64) {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(socket);
        let mut length = None;
        loop {
            let mut line = String::new();
            assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length: ") {
                length = Some(value.trim().parse().unwrap());
            }
        }
        (reader, length.expect("request must include content length"))
    }

    async fn local_server() -> (WhisperServer, tokio::net::TcpListener) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (
            WhisperServer {
                endpoint: Endpoint {
                    addr,
                    host_header: addr.to_string(),
                },
            },
            listener,
        )
    }

    #[tokio::test]
    async fn upload_preserves_binary_audio_and_multipart_length() {
        let audio = AudioFixture::new(0);
        let bytes: Vec<u8> = (0..200_000).map(|i| (i % 256) as u8).collect();
        std::fs::write(&audio.0, &bytes).unwrap();
        let (server, listener) = local_server().await;
        let peer = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (mut reader, length) = request_body(socket).await;
            let mut body = Vec::new();
            assert_eq!(
                (&mut reader)
                    .take(length)
                    .read_to_end(&mut body)
                    .await
                    .unwrap() as u64,
                length
            );
            let start = find_subsequence(&body, b"\r\n\r\n").unwrap() + 4;
            assert_eq!(&body[start..start + bytes.len()], bytes);
            assert!(body.ends_with(b"--papagaiaFormBoundary--\r\n"));
            reader.get_mut().write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"text\":\" streamed transcript \"}").await.unwrap();
        });
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            server.transcribe(&audio.0, &CancelToken::new()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result, "streamed transcript");
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_closes_the_connection_while_waiting_for_response() {
        let audio = AudioFixture::new(1024);
        let (server, listener) = local_server().await;
        let cancel = CancelToken::new();
        let peer_cancel = cancel.clone();
        let peer = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (mut reader, length) = request_body(socket).await;
            tokio::io::copy(&mut (&mut reader).take(length), &mut tokio::io::sink())
                .await
                .unwrap();
            peer_cancel.cancel();
            let mut byte = [0];
            let read = tokio::time::timeout(Duration::from_secs(1), reader.read(&mut byte))
                .await
                .expect("cancelled request must close the socket")
                .unwrap();
            assert_eq!(read, 0);
        });
        let error = server.transcribe(&audio.0, &cancel).await.unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        peer.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "manual upload memory/throughput benchmark; run alone in release mode"]
    async fn benchmark_upload() {
        // One hour of 16 kHz mono S16 PCM plus the WAV header. A sparse file
        // avoids counting fixture construction against the client's peak RSS.
        let audio = AudioFixture::new(16_000 * 2 * 3600 + 44);
        let (server, listener) = local_server().await;
        let peer = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (mut reader, length) = request_body(socket).await;
            assert_eq!(
                tokio::io::copy(&mut (&mut reader).take(length), &mut tokio::io::sink())
                    .await
                    .unwrap(),
                length
            );
            reader
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"text\":\"ok\"}")
                .await
                .unwrap();
        });
        let started = Instant::now();
        assert_eq!(
            server
                .transcribe(&audio.0, &CancelToken::new())
                .await
                .unwrap(),
            "ok"
        );
        peer.await.unwrap();
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        println!(
            "upload: {:?}; {}",
            started.elapsed(),
            status
                .lines()
                .find(|line| line.starts_with("VmHWM:"))
                .unwrap()
        );
    }

    #[test]
    fn a_recited_prompt_is_an_echo() {
        let prompt = "Natural dictation, correct punctuation. Ask Wai, Waizer, Blip, Samara.";
        assert!(echoes_prompt(
            "Natural dictation, correct punctuation.",
            prompt
        ));
        assert!(echoes_prompt("ask wai waizer blip", prompt));
        assert!(echoes_prompt("Waizer, Blip, Samara.", prompt));
    }

    #[test]
    fn real_dictation_is_not_an_echo() {
        let prompt = "Natural dictation, correct punctuation. Ask Wai, Waizer, Blip, Samara.";
        // Terms said alone or out of order are dictation.
        assert!(!echoes_prompt("Ask Wai", prompt));
        assert!(!echoes_prompt("Blip Waizer Samara", prompt));
        assert!(!echoes_prompt(
            "Ask Wai, Waizer, Blip, Samara and more.",
            prompt
        ));
        assert!(!echoes_prompt("", prompt));
    }

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
        let (prefix, suffix) = multipart_parts("BOUND");
        let text = format!("{prefix}RIFFdata{suffix}");
        assert!(text.contains("name=\"file\"; filename=\"audio.wav\""));
        assert!(text.contains("RIFFdata"));
        assert!(text.contains("name=\"response_format\""));
        assert!(text.contains("name=\"split_on_word\"\r\n\r\ntrue"));
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
    fn normalize_transcript_rejoins_word_split_across_segments() {
        // Real whisper-server output: its 60-character segment wrap landed inside
        // "enviar", so the next segment carries no leading space. Joining on the
        // newline would type "env iar".
        assert_eq!(
            normalize_transcript(" o usuário vai env\niar um arquivo\n"),
            "o usuário vai enviar um arquivo"
        );
        // Same, with the continuation carrying the sentence's final punctuation.
        assert_eq!(
            normalize_transcript(" ele vai acordando e continu\nando.\n"),
            "ele vai acordando e continuando."
        );
    }

    #[test]
    fn normalize_transcript_keeps_space_when_segment_starts_a_word() {
        // A leading space on the next segment marks a real word boundary — those
        // words must not be glued together.
        assert_eq!(
            normalize_transcript(" para ser melhorado\n e pensa em novas\n"),
            "para ser melhorado e pensa em novas"
        );
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
