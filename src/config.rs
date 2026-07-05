//! Configuration: loaded from `~/.config/gamescope-idle/config.toml` if present,
//! otherwise sensible defaults. All fields are optional in the file.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

/// How to treat HDMI-CEC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CecMode {
    /// Use CEC standby/wake only if a `/dev/cec*` device is present. (default)
    #[default]
    Auto,
    /// Never touch CEC; overlay only.
    Off,
    /// Always attempt CEC (log an error if no device is found).
    On,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Seconds of no keyboard/controller input before the screen starts blanking.
    pub idle_timeout: u64,
    /// Seconds spent dimmed as a warning before going fully black.
    pub dim_warning: u64,
    /// Alpha (0.0–1.0) of the dim warning overlay. 1.0 = already black.
    pub dim_alpha: f64,
    /// CEC behaviour.
    pub cec: CecMode,
    /// CEC device path (used when a device is present / forced on).
    pub cec_device: String,
    /// Input device event-node basenames to ignore, e.g. `["event0", "event1"]`
    /// for the power button. Matched against the `/dev/input/eventN` basename.
    pub ignore_devices: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle_timeout: 300,
            dim_warning: 30,
            dim_alpha: 0.5,
            cec: CecMode::Auto,
            cec_device: "/dev/cec0".to_string(),
            ignore_devices: Vec::new(),
        }
    }
}

impl Config {
    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.idle_timeout)
    }

    pub fn dim_warning(&self) -> Duration {
        Duration::from_secs(self.dim_warning)
    }

    /// Default config path: `$XDG_CONFIG_HOME/gamescope-idle/config.toml`.
    pub fn default_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("io.github", "gehhilfe", "gamescope-idle")
            .map(|d| d.config_dir().join("config.toml"))
    }

    /// Load from `path` (or the default path when `None`). A missing file yields defaults.
    pub fn load(path: Option<PathBuf>) -> anyhow::Result<Self> {
        let path = match path.or_else(Self::default_path) {
            Some(p) => p,
            None => return Ok(Self::default()),
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let cfg: Config = toml::from_str(&text)?;
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(anyhow::anyhow!("reading {}: {e}", path.display())),
        }
    }
}
