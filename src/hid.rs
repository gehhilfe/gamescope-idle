//! Controller activity via **hidraw**, for the Steam launcher case.
//!
//! In a game, Steam Input re-emits the controller as an evdev pad (handled by
//! [`crate::input`]). But in the Steam launcher / Big Picture UI, Steam consumes
//! the controller directly over hidraw and emits *no* evdev events — so evdev
//! can't tell whether you're navigating menus with the pad.
//!
//! The raw report is dominated by a packet counter and the IMU (gyro/accel), so
//! naive byte-diffing is hopeless. Instead we parse the actual report: the Steam
//! Controller "Triton" (28de:1304) layout is documented in SDL PR #15528 — report
//! id `0x42`, then a seq counter, then a u32 button bitmask, then analog axes and
//! IMU. We read the button field and wake on changes to the *digital* buttons,
//! masking out the capacitive touch/grip bits that flicker from merely holding
//! the pad. So a resting (or just-held) controller stays idle, and a real
//! button/D-pad/click press wakes the screen.

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
    /// Report id (first byte) of the input-state report we parse.
    report_id: u8,
    /// Minimum report length required to read the button field.
    min_len: usize,
    /// Byte offset of the little-endian u32 button bitmask in the report.
    buttons_offset: usize,
    /// Bits of the button field that count as real digital buttons. Capacitive
    /// touch/grip sensor bits (which flicker just from holding the pad) are
    /// masked out so only genuine presses register.
    button_mask: u32,
    /// Byte offsets of the signed-i16 analog stick axes (little-endian).
    stick_axes: &'static [usize],
    /// A stick counts as activity when any axis is deflected past this magnitude
    /// (center is 0). Filters out at-rest ADC jitter around center.
    stick_deadzone: i32,
}

// Capacitive touch/grip bits in the Triton button field (SDL PR #15528). These
// toggle from merely holding the controller, so they are excluded.
const TRITON_CAPACITIVE: u32 = 0x0010_0000   // right joystick touch
    | 0x0020_0000  // right touchpad touch
    | 0x0100_0000  // left joystick touch
    | 0x0200_0000  // left touchpad touch
    | 0x1000_0000  // right grip touch
    | 0x2000_0000; // left grip touch

/// Valve Steam Controller "Triton" (28de:1304, incl. the wireless "Puck"). Report
/// layout from SDL PR #15528: byte 0 = report id `0x42`, byte 1 = seq_num, bytes
/// 2..6 = u32 `buttons` (LE), then triggers/sticks/trackpads, then IMU at 30+. We
/// wake on changes to the digital buttons only (capacitive bits masked out).
const VALVE_STEAM: Profile = Profile {
    name: "Valve Steam Controller",
    vendor: 0x28DE,
    report_id: 0x42,
    min_len: 18, // through the stick axes (bytes 10..18)
    buttons_offset: 2,
    button_mask: !TRITON_CAPACITIVE,
    // Left X/Y, Right X/Y as i16 at bytes 10, 12, 14, 16.
    stick_axes: &[10, 12, 14, 16],
    stick_deadzone: 8000, // ~25% of i16 range
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

        // A digital button changed since the last report...
        let button_changed =
            !prev.is_empty() && masked_buttons(report, profile) != masked_buttons(&prev, profile);
        // ...or a stick is pushed past the deadzone.
        let stick_active = stick_deflected(report, profile);

        if button_changed || stick_active {
            let _ = tx.try_send(());
            if tracing::enabled!(tracing::Level::DEBUG) {
                let now = Instant::now();
                if last_log.is_none_or(|t| now.duration_since(t) >= LOG_THROTTLE) {
                    last_log = Some(now);
                    let what = if button_changed { "button" } else { "stick" };
                    tracing::debug!("{what} from {base} ({})", profile.name);
                }
            }
        }
        prev.clear();
        prev.extend_from_slice(report);
    }
}

/// Read the digital-button bits from a report (capacitive bits masked out).
fn masked_buttons(report: &[u8], profile: &Profile) -> Option<u32> {
    let o = profile.buttons_offset;
    let b = report.get(o..o + 4)?;
    let raw = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    Some(raw & profile.button_mask)
}

/// True if any analog stick axis is deflected past the deadzone.
fn stick_deflected(report: &[u8], profile: &Profile) -> bool {
    profile.stick_axes.iter().any(|&o| {
        report
            .get(o..o + 2)
            .map(|b| (i16::from_le_bytes([b[0], b[1]]) as i32).abs() > profile.stick_deadzone)
            .unwrap_or(false)
    })
}
