use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};

pub const UNIT_NAME: &str = "papagaia-daemon.service";

pub fn unit_path() -> Result<PathBuf> {
    let config_dir = dirs::config_dir().context("XDG config directory is unavailable")?;
    Ok(config_dir.join("systemd").join("user").join(UNIT_NAME))
}

pub fn install() -> Result<PathBuf> {
    let daemon_binary = resolve_daemon_binary().context(
        "could not locate the `papagaia-daemon` binary. \
         Build the workspace (e.g. `cargo install --path crates/papagaia-daemon`) \
         or make sure the binary sits next to `papagaia` or is on PATH.",
    )?;

    let unit_path = unit_path()?;
    if let Some(parent) = unit_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let unit_body = render_unit(&daemon_binary);
    fs::write(&unit_path, unit_body)
        .with_context(|| format!("failed to write {}", unit_path.display()))?;

    run_systemctl(&["daemon-reload"])?;
    run_systemctl(&["enable", "--now", UNIT_NAME])?;

    Ok(unit_path)
}

pub fn restart() -> Result<()> {
    run_systemctl(&["restart", UNIT_NAME])
}

pub fn is_active() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", UNIT_NAME])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub fn is_enabled() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-enabled", "--quiet", UNIT_NAME])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn render_unit(daemon_binary: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=papagaia daemon\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={}\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        daemon_binary.display()
    )
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .context("failed to invoke `systemctl --user`. Is systemd available?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`systemctl --user {}` failed: {}",
            args.join(" "),
            stderr.trim()
        );
    }
    Ok(())
}

fn resolve_daemon_binary() -> Option<PathBuf> {
    if let Ok(current) = std::env::current_exe() {
        if let Some(dir) = current.parent() {
            let candidate = dir.join("papagaia-daemon");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }

    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join("papagaia-daemon"))
        .find(|path| path.exists())
}
