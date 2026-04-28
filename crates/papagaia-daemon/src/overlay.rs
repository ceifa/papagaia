use std::{
    io::Write,
    os::unix::process::CommandExt,
    process::{ChildStdin, Command, Stdio},
};

use anyhow::Result;
use papagaia_core::{OverlayMessage, overlay_program};
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
        if let Some(writer) = stdin.as_mut()
            && writer.write_all(line.as_bytes()).is_ok()
            && writer.flush().is_ok()
        {
            continue;
        }
        if enabled {
            eprintln!("papagaia-daemon: overlay died, respawning");
            stdin = spawn_overlay();
            if let Some(writer) = stdin.as_mut()
                && (writer.write_all(line.as_bytes()).is_err() || writer.flush().is_err())
            {
                eprintln!("papagaia-daemon: respawned overlay failed immediately");
                stdin = None;
            }
        }
    }
}

fn spawn_overlay() -> Option<ChildStdin> {
    let mut command = Command::new(overlay_program());
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // Tie the overlay's lifetime to the daemon: if the daemon exits (crash,
    // SIGKILL, systemd restart) the kernel delivers SIGKILL to the overlay so
    // it can't linger as an orphan. Orphans matter because they keep the
    // layer-shell window mapped and would previously collide with fresh
    // overlays spawned by the next daemon.
    //
    // SAFETY: pre_exec runs in the forked child between fork and execve. We
    // only call async-signal-safe syscalls (prctl), so this is safe.
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // Race: if the daemon died between fork and prctl, the pdeathsig
            // never fires. Check explicitly and exit if we're already an
            // orphan (reparented to init, ppid == 1).
            if libc::getppid() == 1 {
                libc::_exit(0);
            }
            Ok(())
        });
    }

    match command.spawn() {
        Ok(mut child) => {
            let stdin = child.stdin.take();
            // Reap the child in a background thread so exited overlays don't
            // accumulate as zombies across respawns.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            stdin
        }
        Err(error) => {
            eprintln!("papagaia-daemon: overlay disabled: {error}");
            None
        }
    }
}
