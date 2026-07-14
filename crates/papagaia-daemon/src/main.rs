mod app;
mod cancel;
mod cleanup;
mod clipboard;
mod dictation;
mod keybinds;
mod llm;
mod overlay;
mod proc;
mod stt;

use std::{fs, io::ErrorKind, os::unix::fs::FileTypeExt, path::Path, sync::Arc};

use anyhow::{Context, Result, bail};
use app::App;
use papagaia_core::{ClientRequest, ClientResponse, Config, runtime_dir, socket_path};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    signal,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let config = Config::load()?;
    let runtime_dir = runtime_dir()?;
    fs::create_dir_all(&runtime_dir).with_context(|| {
        format!(
            "failed to create runtime directory {}",
            runtime_dir.display()
        )
    })?;

    let socket_path = socket_path()?;
    prepare_socket_path(&socket_path)?;

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind socket {}", socket_path.display()))?;
    if config.logging {
        eprintln!(
            "[papagaia] daemon started, listening on {}",
            socket_path.display()
        );
    }
    // Resolve the keybinds before `config` is moved into the App.
    let bindings = build_keybinds(&config.keybinds);

    let app = Arc::new(App::new(config).await?);

    // Global hotkeys: a background evdev watcher (see `keybinds`) feeds key edges
    // into the very same request handler the socket clients use, so there is a
    // single state machine and one set of races to reason about.
    if !bindings.is_empty() {
        let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel::<ClientRequest>();
        let app_for_requests = app.clone();
        tokio::spawn(async move {
            while let Some(request) = request_rx.recv().await {
                if let Err(error) = app_for_requests.handle(request).await {
                    eprintln!("papagaia-daemon: keybind request failed: {error:#}");
                }
            }
        });
        keybinds::spawn(bindings, request_tx, app.busy_flag());
    }

    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())?;

    loop {
        tokio::select! {
            biased;
            _ = signal::ctrl_c() => {
                eprintln!("[papagaia] received SIGINT, shutting down");
                break;
            }
            _ = sigterm.recv() => {
                eprintln!("[papagaia] received SIGTERM, shutting down");
                break;
            }
            result = listener.accept() => {
                let (stream, _) = result?;
                let app = app.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(app, stream).await {
                        eprintln!("papagaia-daemon: {error:#}");
                    }
                });
            }
        }
    }

    // Clean up the socket file on graceful shutdown.
    let _ = fs::remove_file(&socket_path);
    Ok(())
}

/// Resolve the `[keybinds]` config into `(keycode, action)` pairs, skipping empty
/// entries and warning on unrecognized key names.
fn build_keybinds(config: &papagaia_core::KeybindsConfig) -> Vec<(u16, keybinds::Action)> {
    let mut bindings = Vec::new();
    for (key, action) in [
        (&config.push_to_talk, keybinds::Action::PushToTalk),
        (&config.toggle, keybinds::Action::Toggle),
        (&config.pick, keybinds::Action::Pick),
    ] {
        if key.trim().is_empty() {
            continue;
        }
        match papagaia_core::parse_key_name(key) {
            Some(code) => bindings.push((code, action)),
            None => eprintln!("papagaia: unrecognized keybind '{key}'; ignoring"),
        }
    }
    bindings
}

fn prepare_socket_path(socket_path: &Path) -> Result<()> {
    if !socket_path.exists() {
        return Ok(());
    }

    let metadata = fs::symlink_metadata(socket_path)
        .with_context(|| format!("failed to inspect socket path {}", socket_path.display()))?;
    if !metadata.file_type().is_socket() {
        bail!(
            "refusing to remove non-socket path at {}",
            socket_path.display()
        );
    }

    match std::os::unix::net::UnixStream::connect(socket_path) {
        Ok(_) => bail!(
            "papagaia-daemon is already running at {}",
            socket_path.display()
        ),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::ConnectionRefused | ErrorKind::NotFound
            ) =>
        {
            fs::remove_file(socket_path).with_context(|| {
                format!("failed to remove stale socket {}", socket_path.display())
            })?;
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to probe existing socket at {}; another daemon may still be using it",
                    socket_path.display()
                )
            });
        }
    }

    Ok(())
}

async fn handle_connection(app: Arc<App>, stream: UnixStream) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    if line.trim().is_empty() {
        return Ok(());
    }

    let request: ClientRequest =
        serde_json::from_str(&line).context("failed to decode client request")?;
    let response = match app.handle(request).await {
        Ok(response) => response,
        Err(error) => ClientResponse::err(error.to_string()),
    };

    writer
        .write_all(format!("{}\n", serde_json::to_string(&response)?).as_bytes())
        .await?;
    Ok(())
}
