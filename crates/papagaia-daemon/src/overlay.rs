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
    enabled: bool,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
}

impl OverlayHandle {
    pub fn spawn(enabled: bool) -> Result<Self> {
        let stdin = if enabled {
            Arc::new(Mutex::new(spawn_overlay()))
        } else {
            Arc::new(Mutex::new(None))
        };

        Ok(Self { enabled, stdin })
    }

    pub async fn send(&self, message: OverlayMessage) {
        let enabled = self.enabled;
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
            if let Some(writer) = guard.as_mut() {
                if writer.write_all(line.as_bytes()).is_ok() && writer.flush().is_ok() {
                    return;
                }
            }
            // Overlay is dead or was never started — try to respawn.
            if enabled {
                eprintln!("papagaia-daemon: overlay died, respawning");
                *guard = spawn_overlay();
                if let Some(writer) = guard.as_mut() {
                    if writer.write_all(line.as_bytes()).is_err() || writer.flush().is_err() {
                        eprintln!("papagaia-daemon: respawned overlay failed immediately");
                        *guard = None;
                    }
                }
            }
        })
        .await
        .ok();
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
