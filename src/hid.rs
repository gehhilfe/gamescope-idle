//! Controller activity via **hidraw**, for the Steam launcher case.
//!
//! In a game, Steam Input re-emits the controller as an evdev pad (handled by
//! [`crate::input`]). But in the Steam launcher / Big Picture UI, Steam consumes
//! the controller directly over hidraw and emits *no* evdev events — so evdev
//! can't tell whether you're navigating menus with the pad.
//!
//! The raw report is dominated by a packet counter and the IMU (gyro/accel),
//! which churn constantly even when the controller is perfectly still. Worse,
//! empirically the button/stick/trackpad state is *not* cleanly diff-able from
//! this report — it's buried at a change rate indistinguishable from the counter
//! noise. What *is* reliable is **motion**: a handful of orientation/gyro bytes
//! read exactly zero while the controller sits still and light up the instant it
//! is handled. That's the signal we use — and it's the right proxy for the
//! launcher: while you navigate you're holding the pad (hand tremor/tilt →
//! motion → awake); set it down and it goes still → the screen blanks. Those
//! bytes are silent at rest, so a resting controller produces zero false activity.
//!
//! The byte offsets are per-controller-model; the profile below was measured
//! empirically (30s at rest, still vs. handled).

use std::collections::HashSet;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;

use crate::config::Config;

const RESCAN_INTERVAL: Duration = Duration::from_secs(3);
const LOG_THROTTLE: Duration = Duration::from_secs(1);

/// A known controller's report layout.
struct Profile {
    name: &'static str,
    vendor: u16,
    /// First byte of the input/status report we key on.
    report_id: u8,
    /// Minimum report length for the motion-byte offsets to be valid.
    min_len: usize,
    /// Report byte offsets that read zero while the controller is still and
    /// change when it is moved/handled (orientation + gyro). Everything else is
    /// the packet counter or high-rate IMU noise that never goes quiet.
    motion_bytes: &'static [usize],
}

/// Valve Steam Controller, including the wireless "Puck" (up to 4 pads). Offsets
/// measured on `Valve Software Steam Controller Puck` (28de:1304): bytes 8, 18–29
/// and 35/37/39 had 0 changes over 30s while still and lit up when the pad was
/// handled, while the counter (byte 1) and IMU (10–16, 30–45) churn continuously.
const VALVE_STEAM: Profile = Profile {
    name: "Valve Steam Controller",
    vendor: 0x28DE,
    report_id: 0x42,
    min_len: 40,
    motion_bytes: &[
        8, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 35, 37, 39,
    ],
};

const PROFILES: &[&Profile] = &[&VALVE_STEAM];

pub fn spawn(cfg: Arc<Config>, tx: mpsc::Sender<()>) {
    if !cfg.watch_hidraw {
        return;
    }
    let watched: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
    tokio::spawn(async move {
        loop {
            for path in hidraw_nodes() {
                if watched.lock().unwrap().contains(&path) {
                    continue;
                }
                // Non-matching nodes are simply re-checked each rescan (cheap
                // sysfs read); only matched ones join the watched set and are
                // self-removed by their task on death.
                if let Some(profile) = profile_for(&path) {
                    let base = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default()
                        .to_string();
                    tracing::info!("watching {base} ({} hidraw)", profile.name);
                    watched.lock().unwrap().insert(path.clone());
                    tokio::spawn(watch_hidraw(path, profile, tx.clone(), watched.clone()));
                }
            }
            tokio::time::sleep(RESCAN_INTERVAL).await;
        }
    });
}

/// List `/dev/hidraw*` device paths.
fn hidraw_nodes() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/dev") {
        for e in entries.flatten() {
            let p = e.path();
            if p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("hidraw"))
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Match a hidraw node to a known profile via its sysfs `HID_ID` vendor.
fn profile_for(path: &std::path::Path) -> Option<&'static Profile> {
    let base = path.file_name()?.to_str()?;
    let uevent = std::fs::read_to_string(format!("/sys/class/hidraw/{base}/device/uevent")).ok()?;
    // HID_ID=<bus>:<vendor>:<product>, all hex, e.g. 0003:000028DE:00001304
    let hid_id = uevent.lines().find_map(|l| l.strip_prefix("HID_ID="))?;
    let vendor_hex = hid_id.split(':').nth(1)?;
    let vendor = u32::from_str_radix(vendor_hex.trim(), 16).ok()? as u16;
    PROFILES.iter().copied().find(|p| p.vendor == vendor)
}

async fn watch_hidraw(
    path: PathBuf,
    profile: &'static Profile,
    tx: mpsc::Sender<()>,
    watched: Arc<Mutex<HashSet<PathBuf>>>,
) {
    let base = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();

    let result = read_loop(&path, &base, profile, &tx).await;
    if let Err(e) = result {
        tracing::debug!("{base} ended: {e}");
    }
    // Allow the rescan to re-attach if the node is reused (Steam re-creates
    // controller interfaces as pads connect/disconnect).
    watched.lock().unwrap().remove(&path);
}

async fn read_loop(
    path: &std::path::Path,
    base: &str,
    profile: &Profile,
    tx: &mpsc::Sender<()>,
) -> std::io::Result<()> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    let afd = AsyncFd::new(file)?;

    let mut prev: Vec<u8> = Vec::new();
    let mut buf = [0u8; 128];
    let mut last_log: Option<Instant> = None;

    loop {
        let mut guard = afd.readable().await?;
        let read = guard.try_io(|inner| inner.get_ref().read(&mut buf));
        let n = match read {
            Ok(Ok(0)) => return Ok(()), // EOF
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e),
            Err(_would_block) => continue, // readiness was spurious
        };

        let report = &buf[..n];
        if report.len() < profile.min_len || report[0] != profile.report_id {
            continue;
        }

        if !prev.is_empty() {
            let moved = profile
                .motion_bytes
                .iter()
                .any(|&i| i < report.len() && i < prev.len() && report[i] != prev[i]);
            if moved {
                let _ = tx.try_send(());
                if tracing::enabled!(tracing::Level::DEBUG) {
                    let now = Instant::now();
                    if last_log.is_none_or(|t| now.duration_since(t) >= LOG_THROTTLE) {
                        last_log = Some(now);
                        tracing::debug!("motion from {base} ({})", profile.name);
                    }
                }
            }
        }
        prev.clear();
        prev.extend_from_slice(report);
    }
}
