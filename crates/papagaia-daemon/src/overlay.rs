use std::{
    io::Write,
    path::PathBuf,
    process::{ChildStdin, Command, Stdio},
    sync::{Arc, Mutex},
};

use anyhow::Result;
use papagaia_core::OverlayMessage;

#[derive(Clone)]
pub struct OverlayHandle {
    stdin: Arc<Mutex<Option<ChildStdin>>>,
}

impl OverlayHandle {
    pub fn spawn(enabled: bool) -> Result<Self> {
        if !enabled {
            return Ok(Self {
                stdin: Arc::new(Mutex::new(None)),
            });
        }

        let overlay_program = overlay_program();
        let child = Command::new(&overlay_program)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        let stdin = match child {
            Ok(mut child) => Arc::new(Mutex::new(child.stdin.take())),
            Err(error) => {
                eprintln!("papagaia-daemon: overlay disabled: {error}");
                Arc::new(Mutex::new(None))
            }
        };

        Ok(Self { stdin })
    }

    pub async fn send(&self, message: OverlayMessage) {
        let stdin = self.stdin.clone();
        let line = match serde_json::to_string(&message) {
            Ok(line) => format!("{line}\n"),
            Err(error) => {
                eprintln!("papagaia-daemon: failed to serialize overlay message: {error}");
                return;
            }
        };

        tokio::task::spawn_blocking(move || {
            let mut guard = stdin.lock().expect("overlay stdin lock poisoned");
            if let Some(stdin) = guard.as_mut()
                && (stdin.write_all(line.as_bytes()).is_err() || stdin.flush().is_err())
            {
                *guard = None;
            }
        })
        .await
        .ok();
    }
}

fn overlay_program() -> PathBuf {
    if let Ok(current_exe) = std::env::current_exe()
        && let Some(parent) = current_exe.parent()
    {
        let sibling = parent.join("papagaia-overlay");
        if sibling.exists() {
            return sibling;
        }
    }

    PathBuf::from("papagaia-overlay")
}
