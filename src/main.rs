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

fn parse_seconds(argv: &[String]) -> f64 {
    let mut seconds = 0.0;
    for a in &argv[1..] {
        if let Ok(v) = a.parse::<f64>() {
            seconds = v;
        }
    }
    seconds
}

fn get_config_file_path() -> Result<PathBuf> {
    match std::env::var_os("HOME") {
        Some(v) if !v.is_empty() => {
            Ok(PathBuf::from(v).join(".config/sway-blur/config.toml"))
        }
        _ => anyhow::bail!("HOME not set (expected $HOME/.config/sway-blur/config.toml)"),
    }
}

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    let config_path = get_config_file_path()?;
    let config = config::Config::load(&config_path)
        .with_context(|| format!("loading {}", config_path.display()))?;

    if argv.iter().any(|a| a == "--watch-only") {
        let seconds = parse_seconds(&argv);
        let cfg = config.clone();
        let watcher = watcher::Watcher::new(&config, move || daemon::preview_refresh(&cfg));
        if seconds > 0.0 {
            let w2 = watcher.clone();
            std::thread::spawn(move || {
                if let Err(e) = w2.run() {
                    eprintln!("[watcher] exited: {e:#}");
                }
            });
            std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
            watcher.stop();
        } else if let Err(e) = watcher.run() {
            eprintln!("[watcher] exited: {e:#}");
        }
        return Ok(());
    }

    // Single-instance guard (held for the process lifetime).
    let _lock = daemon::acquire_single_instance()?;

    gtk::init().context("gtk::init (no display?)")?;
    // The overlay lives on this (GTK) thread; the daemon worker only gets
    // the message handle. Spawn after init, before the main loop runs.
    let overlay = overlay::OverlayHandle::spawn();
    daemon::Daemon::new(config, overlay).run(parse_seconds(&argv))
}
