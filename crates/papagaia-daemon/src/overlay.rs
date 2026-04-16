use std::{
    io::Write,
    path::PathBuf,
    process::{ChildStdin, Command, Stdio},
};

use anyhow::Result;
use papagaia_core::OverlayMessage;
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct OverlayHandle {
    tx: mpsc::UnboundedSender<String>,
}

impl OverlayHandle {
    pub fn spawn(enabled: bool) -> Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::spawn(move || overlay_writer_thread(enabled, rx));
        Ok(Self { tx })
    }

    pub async fn send(&self, message: OverlayMessage) {
        let line = match serde_json::to_string(&message) {
            Ok(line) => format!("{line}\n"),
            Err(error) => {
                eprintln!("papagaia-daemon: failed to serialize overlay message: {error}");
                return;
            }
        };
        let _ = self.tx.send(line);
    }
}

fn overlay_writer_thread(enabled: bool, mut rx: mpsc::UnboundedReceiver<String>) {
    let mut stdin = if enabled { spawn_overlay() } else { None };

    while let Some(line) = rx.blocking_recv() {
        if let Some(writer) = stdin.as_mut() {
            if writer.write_all(line.as_bytes()).is_ok() && writer.flush().is_ok() {
                continue;
            }
        }
        if enabled {
            eprintln!("papagaia-daemon: overlay died, respawning");
            stdin = spawn_overlay();
            if let Some(writer) = stdin.as_mut() {
                if writer.write_all(line.as_bytes()).is_err() || writer.flush().is_err() {
                    eprintln!("papagaia-daemon: respawned overlay failed immediately");
                    stdin = None;
                }
            }
        }
    }
}

fn spawn_overlay() -> Option<ChildStdin> {
    let overlay_program = overlay_program();
    match Command::new(&overlay_program)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => child.stdin.take(),
        Err(error) => {
            eprintln!("papagaia-daemon: overlay disabled: {error}");
            None
        }
    }
}

fn overlay_program() -> PathBuf {
    papagaia_core::overlay_program()
}
