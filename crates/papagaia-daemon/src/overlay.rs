use std::{process::Stdio, time::Duration};

use anyhow::Result;
use papagaia_core::{OverlayMessage, overlay_program};
use tokio::{
    io::AsyncWriteExt,
    process::{ChildStdin, Command},
    sync::watch,
    time::Instant,
};

/// The HUD displays current state: slow pipes must not queue old mic levels or
/// hold up dictation. A watch slot retains just the latest state.
#[derive(Clone, Default)]
pub struct OverlayHandle {
    tx: Option<watch::Sender<OverlayMessage>>,
}

impl OverlayHandle {
    pub fn spawn(enabled: bool) -> Result<Self> {
        if !enabled {
            return Ok(Self::default());
        }
        let (tx, rx) = watch::channel(OverlayMessage::Hidden);
        tokio::spawn(overlay_writer(rx));
        Ok(Self { tx: Some(tx) })
    }

    pub async fn send(&self, message: OverlayMessage) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(message);
        }
    }
}

async fn overlay_writer(mut rx: watch::Receiver<OverlayMessage>) {
    let mut stdin = spawn_overlay();
    let mut retry_at = Instant::now() + Duration::from_secs(2);
    let mut line = Vec::new();
    while rx.changed().await.is_ok() {
        let message = rx.borrow_and_update().clone();
        if stdin.is_none() {
            if Instant::now() < retry_at {
                continue;
            }
            stdin = spawn_overlay();
            retry_at = Instant::now() + Duration::from_secs(2);
        }
        let Some(writer) = stdin.as_mut() else {
            continue;
        };
        line.clear();
        if let Err(error) = serde_json::to_writer(&mut line, &message) {
            eprintln!("papagaia-daemon: failed to serialize overlay message: {error}");
            continue;
        }
        line.push(b'\n');
        if writer.write_all(&line).await.is_err() {
            eprintln!("papagaia-daemon: overlay died");
            stdin = None;
            // Recover immediately after a long-lived overlay exits, but cap
            // retries when startup itself fails (e.g. unavailable display).
            if Instant::now() >= retry_at {
                stdin = spawn_overlay();
                retry_at = Instant::now() + Duration::from_secs(2);
                if let Some(writer) = stdin.as_mut()
                    && writer.write_all(&line).await.is_err()
                {
                    stdin = None;
                }
            }
        }
    }
}

fn spawn_overlay() -> Option<ChildStdin> {
    let mut command = Command::new(overlay_program());
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    // Tie the overlay's lifetime to the daemon so it can't linger as an orphan.
    crate::proc::die_with_parent(command.as_std_mut());

    match command.spawn() {
        Ok(mut child) => {
            let stdin = child.stdin.take();
            // Reap through Tokio without dedicating an OS thread to the child.
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            stdin
        }
        Err(error) => {
            eprintln!("papagaia-daemon: overlay unavailable: {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stalled_writer_keeps_latest_state_without_blocking_producers() {
        let (tx, mut rx) = watch::channel(OverlayMessage::Hidden);
        let overlay = OverlayHandle { tx: Some(tx) };
        for _ in 0..10_000 {
            overlay
                .send(OverlayMessage::Recording {
                    level: 0.5,
                    target: None,
                })
                .await;
        }
        overlay.send(OverlayMessage::Hidden).await;
        rx.changed().await.unwrap();
        assert!(matches!(*rx.borrow_and_update(), OverlayMessage::Hidden));
        assert!(!rx.has_changed().unwrap());
        drop(overlay);
        assert!(rx.changed().await.is_err());
    }
}
