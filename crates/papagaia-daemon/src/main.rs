mod app;
mod cancel;
mod clipboard;
mod dictation;
mod llm;
mod overlay;

use std::{fs, sync::Arc};

use anyhow::{Context, Result};
use app::App;
use papagaia_core::{ClientRequest, ClientResponse, Config, runtime_dir, socket_path};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

#[tokio::main]
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
    if socket_path.exists() {
        fs::remove_file(&socket_path)
            .with_context(|| format!("failed to remove stale socket {}", socket_path.display()))?;
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind socket {}", socket_path.display()))?;
    if config.logging {
        eprintln!(
            "[papagaia] daemon started, listening on {}",
            socket_path.display()
        );
    }
    let app = Arc::new(App::new(config).await?);

    loop {
        let (stream, _) = listener.accept().await?;
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_connection(app, stream).await {
                eprintln!("papagaia-daemon: {error:#}");
            }
        });
    }
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
