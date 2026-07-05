//! Input activity detection by reading `/dev/input/event*` directly with evdev.
//!
//! This is deliberately *not* compositor-based: gamescope exposes no idle
//! protocol, and — crucially — game controllers are consumed by Steam Input and
//! re-emitted as a virtual pad, so they never reach the compositor as pointer or
//! keyboard events. Reading evdev is the only way to count both keyboard and
//! controller activity.
//!
//! Absolute axes (sticks, triggers, gyro) get a per-axis deadzone derived from
//! the kernel `absinfo`, so drift and idle jitter don't keep the screen awake,
//! while D-pad (hat) presses and real stick pushes do.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use evdev::{Device, EventType};
use tokio::sync::mpsc;

use crate::config::Config;

const RESCAN_INTERVAL: Duration = Duration::from_secs(3);

/// Minimum gap between per-device activity debug lines (avoids flooding).
const LOG_THROTTLE: Duration = Duration::from_secs(1);

/// Device *names* never treated as user input. "Video Bus" emits key events on
/// display/DPMS changes — including ones our own blanking can cause — which
/// would create a wake/blank feedback loop.
const NAME_BLOCKLIST: &[&str] = &["Video Bus"];

/// Spawn the input-watching machinery. Every burst of real activity sends `()`
/// on `tx` (best-effort; the receiver only needs to know "something happened").
pub fn spawn(cfg: Arc<Config>, tx: mpsc::Sender<()>) {
    tokio::spawn(async move {
        let mut watched: HashSet<PathBuf> = HashSet::new();
        loop {
            for (path, dev) in evdev::enumerate() {
                if watched.contains(&path) {
                    continue;
                }
                let base = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string();
                if cfg.ignore_devices.iter().any(|ig| ig == &base) {
                    continue;
                }
                if !is_input_source(&dev) {
                    continue;
                }
                let dev_name = dev.name().unwrap_or_default().trim().to_string();
                if NAME_BLOCKLIST.iter().any(|b| dev_name.contains(b)) {
                    watched.insert(path.clone()); // remember so we don't re-log/re-check it
                    continue;
                }
                tracing::info!(
                    "watching {} ({})",
                    base,
                    dev.name().unwrap_or("unnamed").trim()
                );
                watched.insert(path.clone());
                let tx = tx.clone();
                tokio::spawn(watch_device(dev, path, tx));
            }
            // Drop unplugged devices so a re-plug (same node) is picked up again.
            watched.retain(|p| p.exists());
            tokio::time::sleep(RESCAN_INTERVAL).await;
        }
    });
}

/// A device counts if it can produce keyboard/button, pointer, or axis events.
fn is_input_source(dev: &Device) -> bool {
    let ev = dev.supported_events();
    ev.contains(EventType::KEY)
        || ev.contains(EventType::RELATIVE)
        || ev.contains(EventType::ABSOLUTE)
}

/// Per-axis deadzone thresholds keyed by ABS code.
fn abs_thresholds(dev: &Device) -> HashMap<u16, i32> {
    let mut map = HashMap::new();
    if let Ok(states) = dev.get_abs_state() {
        // `get_abs_state` returns a fixed-size array indexed by axis code.
        for (code, info) in states.iter().enumerate() {
            let range = (info.maximum - info.minimum).max(0);
            // 5% of range, but at least the kernel deadzone (`flat`), at least 1.
            let threshold = (range / 20).max(info.flat.abs()).max(1);
            map.insert(code as u16, threshold);
        }
    }
    map
}

async fn watch_device(dev: Device, path: PathBuf, tx: mpsc::Sender<()>) {
    let thresholds = abs_thresholds(&dev);
    let mut last_abs: HashMap<u16, i32> = HashMap::new();

    let base = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let name = dev.name().unwrap_or("unnamed").trim().to_string();

    let mut stream = match dev.into_event_stream() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("cannot stream {}: {e}", path.display());
            return;
        }
    };

    // Per-device throttle so RUST_LOG=debug doesn't flood the journal during play.
    let mut last_log: Option<Instant> = None;

    loop {
        match stream.next_event().await {
            Ok(ev) => {
                if is_activity(&ev, &thresholds, &mut last_abs) {
                    // Best-effort: if the channel is full, activity is already signalled.
                    let _ = tx.try_send(());

                    // Debug: name the device/event keeping the screen awake.
                    if tracing::enabled!(tracing::Level::DEBUG) {
                        let now = Instant::now();
                        if last_log.is_none_or(|t| now.duration_since(t) >= LOG_THROTTLE) {
                            last_log = Some(now);
                            tracing::debug!(
                                "activity from {base} ({name}): type={:?} code={} value={}",
                                ev.event_type(),
                                ev.code(),
                                ev.value()
                            );
                        }
                    }
                }
            }
            Err(e) => {
                tracing::debug!("{base} ended: {e}");
                break;
            }
        }
    }
}

/// Decide whether an event represents genuine user activity.
fn is_activity(
    ev: &evdev::InputEvent,
    thresholds: &HashMap<u16, i32>,
    last_abs: &mut HashMap<u16, i32>,
) -> bool {
    match ev.event_type() {
        EventType::KEY | EventType::RELATIVE => true,
        EventType::ABSOLUTE => {
            let code = ev.code();
            let value = ev.value();
            let threshold = thresholds.get(&code).copied().unwrap_or(1);
            let moved = match last_abs.get(&code) {
                Some(prev) => (value - prev).abs() >= threshold,
                None => false, // first sample establishes a baseline, not activity
            };
            last_abs.insert(code, value);
            moved
        }
        _ => false,
    }
}
