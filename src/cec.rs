//! HDMI-CEC control via `cec-ctl` (from v4l-utils).
//!
//! When a `/dev/cec*` adapter is present (e.g. the UGREEN DisplayPort adapter
//! with CEC on the target OLED-TV setup) we put the TV into real standby on
//! blank and wake it on activity. With no adapter this is a silent no-op and the
//! black overlay alone protects the panel — which is the case on the interim
//! DELL monitor and any GPU without CEC.
//!
//! CEC behaviour cannot be validated until the adapter + TV are connected, so
//! failures here are logged and never fatal.

use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::config::{CecMode, Config};

pub struct Cec {
    device: Option<PathBuf>,
}

impl Cec {
    /// Decide, from config + what is present on disk, whether CEC is in play.
    pub fn new(cfg: &Config) -> Self {
        let device = match cfg.cec {
            CecMode::Off => None,
            CecMode::On => Some(PathBuf::from(&cfg.cec_device)),
            CecMode::Auto => {
                let configured = PathBuf::from(&cfg.cec_device);
                if configured.exists() {
                    Some(configured)
                } else {
                    first_cec_device()
                }
            }
        };

        match &device {
            Some(d) if d.exists() => tracing::info!("CEC enabled on {}", d.display()),
            Some(d) => tracing::warn!(
                "CEC device {} not present; TV standby disabled (overlay only)",
                d.display()
            ),
            None => tracing::info!("no CEC adapter; overlay-only blanking"),
        }

        Self { device }
    }

    fn active_device(&self) -> Option<&Path> {
        match &self.device {
            Some(d) if d.exists() => Some(d.as_path()),
            _ => None,
        }
    }

    /// Put the TV into standby.
    pub async fn standby(&self) {
        if let Some(dev) = self.active_device() {
            self.run(dev, &["--to", "0", "--standby"]).await;
        }
    }

    /// Wake the TV (Image View On + become the active source).
    pub async fn wake(&self) {
        if let Some(dev) = self.active_device() {
            self.run(dev, &["--to", "0", "--image-view-on"]).await;
            self.run(dev, &["--active-source", "phys-addr=0.0.0.0"])
                .await;
        }
    }

    async fn run(&self, dev: &Path, args: &[&str]) {
        let result = Command::new("cec-ctl")
            .arg("-d")
            .arg(dev)
            .args(args)
            .output()
            .await;
        match result {
            Ok(out) if out.status.success() => {}
            Ok(out) => tracing::warn!(
                "cec-ctl {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(e) => tracing::warn!("could not run cec-ctl (is v4l-utils installed?): {e}"),
        }
    }
}

/// Return the first `/dev/cec*` device found, if any.
fn first_cec_device() -> Option<PathBuf> {
    let entries = std::fs::read_dir("/dev").ok()?;
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("cec"))
        })
        .collect();
    found.sort();
    found.into_iter().next()
}
