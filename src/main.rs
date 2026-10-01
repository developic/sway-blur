//! sway-blur: snapshot blur overlay behind transparent windows.
//!
//! Pipeline: sway IPC watcher -> capture/blur worker -> gtk-layer-shell overlay.
//! Autostart via sway `exec_always` (NOT systemd) + single-instance flock guard.
//! Config: $HOME/.config/sway-blur/config.toml (auto-created with defaults).

mod capture;
mod config;
mod daemon;
mod ipc;
mod model;
mod overlay;
mod screencopy;
mod tree;
mod watcher;

use anyhow::{Context, Result};
use std::path::PathBuf;

fn get_config_file_path() -> Result<PathBuf> {
    match std::env::var_os("HOME") {
        Some(v) if !v.is_empty() => {
            Ok(PathBuf::from(v).join(".config/sway-blur/config.toml"))
        }
        _ => anyhow::bail!("HOME not set (expected $HOME/.config/sway-blur/config.toml)"),
    }
}

fn main() -> Result<()> {
    if std::env::args().skip(1).any(|a| a == "-v" || a == "--version") {
        println!("sway-blur {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let config_path = get_config_file_path()?;
    let config = config::Config::load(&config_path)
        .with_context(|| format!("loading {}", config_path.display()))?;

    // Single-instance guard (held for the process lifetime).
    let _lock = daemon::acquire_single_instance()?;

    gtk::init().context("gtk::init (no display?)")?;
    // The overlay lives on this (GTK) thread; the daemon worker only gets
    // the message handle. Spawn after init, before the main loop runs.
    let overlay = overlay::OverlayHandle::spawn();
    daemon::Daemon::new(config, overlay).run()
}
