use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// Runtime config: parsed from TOML, units converted to seconds.
#[derive(Debug, Clone)]
pub struct Config {
    pub allow: HashSet<String>,
    pub downscale: u32,
    pub blur: f64,
    pub tint: f64,
    pub debounce_s: f64,
    pub cache_max: usize,
    pub hide_settle_s: f64,
    pub revalidate_s: f64,
}

#[derive(Debug, Deserialize, Default)]
struct RawConfig {
    #[serde(default)]
    allowlist_app_ids: Vec<String>,
    #[serde(default = "d_downscale")]
    downscale: u32,
    #[serde(default = "d_radius")]
    blur: f64,
    #[serde(default)]
    tint: f64,
    #[serde(default = "d_debounce_ms")]
    debounce_ms: f64,
    #[serde(default = "d_cache_max")]
    snapshot_cache_max: usize,
    /// Settle wait after hiding before capturing, in ms.
    #[serde(default = "d_hide_settle_ms")]
    hide_settle_ms: f64,
    /// Re-check cadence with no events (drags), in ms. 0 = off.
    #[serde(default = "d_revalidate_ms")]
    revalidate_ms: f64,
}

fn d_downscale() -> u32 {
    8
}
fn d_radius() -> f64 {
    6.0
}
fn d_debounce_ms() -> f64 {
    150.0
}
fn d_cache_max() -> usize {
    16
}
fn d_hide_settle_ms() -> f64 {
    25.0
}
fn d_revalidate_ms() -> f64 {
    1000.0
}

/// Keep in sync with config.example.toml.
pub const DEFAULT_CONFIG: &str = r#"# Windows treated as transparent: the blurred snapshot is shown behind these app_ids.
allowlist_app_ids = [
  "com.mitchellh.ghostty",
  "Alacritty",
  "kitty",
  "foot",
]
# Blur strength. Higher = more blur.
blur = 2.0
# The screenshot is shrunk by this factor before blurring. Higher = faster and smoother.
downscale = 8
# Dark tint mixed into the blur, 0-100 (percent). 0 = no tint.
tint = 0.0
# Rapid sway events are coalesced for this long before refreshing (milliseconds).
debounce_ms = 150.0
# Wait after hiding before capturing, so the compositor presents the hidden frame (milliseconds).
hide_settle_ms = 25.0
# Re-check the tree on this cadence even with no events, e.g. drags (milliseconds; 0 = off).
revalidate_ms = 1000.0
# Snapshots kept per window.
snapshot_cache_max = 16
"#;

/// Write defaults to `path` if missing. Create-new: a lost race uses the winner's file.
pub fn ensure_default_config(path: &Path) -> Result<bool> {
    use std::io::ErrorKind;
    if path.is_file() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut f) => {
            use std::io::Write as _;
            f.write_all(DEFAULT_CONFIG.as_bytes())
                .with_context(|| format!("writing default config {}", path.display()))?;
            println!("[config] created default config at {}", path.display());
            Ok(true)
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e).with_context(|| format!("creating config {}", path.display())),
    }
}

impl Config {
    fn from_raw(r: RawConfig) -> Self {
        Self {
            allow: r.allowlist_app_ids.into_iter().collect(),
            downscale: r.downscale.max(1),
            blur: r.blur,
            tint: r.tint,
            debounce_s: r.debounce_ms / 1000.0,
            cache_max: r.snapshot_cache_max.max(1),
            hide_settle_s: r.hide_settle_ms.max(0.0) / 1000.0,
            // Clamped: bad values must not panic Duration::from_secs_f64.
            revalidate_s: r.revalidate_ms.clamp(0.0, 3_600_000.0) / 1000.0,
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        ensure_default_config(path)?;
        let data = fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let raw: RawConfig = toml::from_str(&data).context("parsing config.toml")?;
        Ok(Self::from_raw(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_parses() {
        let raw: RawConfig = toml::from_str(DEFAULT_CONFIG).expect("default config must parse");
        let cfg = Config::from_raw(raw);
        assert_eq!(cfg.allow.len(), 4);
        assert_eq!(cfg.cache_max, 16);
        assert_eq!(cfg.blur, 2.0);
    }
}
