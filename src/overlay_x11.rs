//! Alternative overlay backend: a fullscreen black **Xwayland** window flagged
//! with the `GAMESCOPE_EXTERNAL_OVERLAY` atom — the same mechanism mangoapp uses
//! to draw on top of games.
//!
//! The window is created lazily on the first dim/blank and mapped from then on;
//! it is never unmapped. gamescope only recomposites an idle scene in response
//! to a fresh frame on a *mapped* surface, so hiding is done by repainting the
//! window fully transparent (alpha 0) — unmapping instead would leave the last
//! black frame stuck on screen with nothing to trigger a recomposite. This
//! mirrors the layer-shell backend, which also hides by going transparent rather
//! than tearing anything down. It reconnects on X-server churn (e.g. a game
//! launch spinning up a new Xwayland), restoring the desired state.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ClipOrdering, ColormapAlloc, ConfigureWindowAux,
    ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Screen, VisualClass, Visualid,
    Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::overlay::OverlayControl;

enum Cmd {
    Show(f64),
    Hide,
    Quit,
}

/// Shared between the handle and the overlay thread. `alpha` is the desired
/// opacity (<= 0 means hidden) and is the source of truth across reconnects.
struct Shared {
    sender: Mutex<Option<Sender<Cmd>>>,
    alpha: Mutex<f64>,
    quit: AtomicBool,
}

pub struct X11OverlayHandle {
    shared: Arc<Shared>,
}

impl OverlayControl for X11OverlayHandle {
    fn show(&self, alpha: f64) {
        *self.shared.alpha.lock().unwrap() = alpha;
        if let Some(tx) = self.shared.sender.lock().unwrap().as_ref() {
            let _ = tx.send(Cmd::Show(alpha));
        }
    }
    fn hide(&self) {
        *self.shared.alpha.lock().unwrap() = 0.0;
        if let Some(tx) = self.shared.sender.lock().unwrap().as_ref() {
            let _ = tx.send(Cmd::Hide);
        }
    }
    fn quit(&self) {
        self.shared.quit.store(true, Ordering::SeqCst);
        if let Some(tx) = self.shared.sender.lock().unwrap().as_ref() {
            let _ = tx.send(Cmd::Quit);
        }
    }
}

/// Spawn the overlay thread. It (re)connects to Xwayland on its own, so this
/// never fails.
pub fn spawn() -> Result<X11OverlayHandle> {
    let shared = Arc::new(Shared {
        sender: Mutex::new(None),
        alpha: Mutex::new(0.0),
        quit: AtomicBool::new(false),
    });
    let thread_shared = shared.clone();
    thread::Builder::new()
        .name("gi-x11-overlay".into())
        .spawn(move || overlay_thread(thread_shared))
        .context("spawning x11 overlay thread")?;
    Ok(X11OverlayHandle { shared })
}

fn overlay_thread(shared: Arc<Shared>) {
    let mut backoff = Duration::from_millis(200);
    while !shared.quit.load(Ordering::SeqCst) {
        match x11rb::connect(None) {
            Ok((conn, screen_num)) => {
                let (tx, rx) = channel::<Cmd>();
                *shared.sender.lock().unwrap() = Some(tx);
                let result = run(&conn, screen_num, &rx, &shared);
                *shared.sender.lock().unwrap() = None;
                backoff = Duration::from_millis(200);
                if let Err(e) = result {
                    if !shared.quit.load(Ordering::SeqCst) {
                        tracing::warn!("external overlay connection lost ({e:#}); reconnecting");
                    }
                }
            }
            Err(e) => tracing::warn!("external overlay cannot reach Xwayland ({e:#}); retrying"),
        }
        if shared.quit.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(3));
    }
}

fn run(
    conn: &RustConnection,
    screen_num: usize,
    rx: &Receiver<Cmd>,
    shared: &Shared,
) -> Result<()> {
    let screen = conn.setup().roots[screen_num].clone();
    // The Xwayland root size changes with the game/output resolution, so query
    // it live rather than trusting the size from connection setup.
    let geo = conn.get_geometry(screen.root)?.reply()?;
    let (w, h) = (geo.width, geo.height);

    let visual = find_argb_visual(&screen).context("no 32-bit ARGB visual on this X screen")?;
    let colormap = conn.generate_id()?;
    conn.create_colormap(ColormapAlloc::NONE, colormap, screen.root, visual)?;

    let win = conn.generate_id()?;
    conn.create_window(
        32,
        win,
        screen.root,
        0,
        0,
        w,
        h,
        0,
        WindowClass::INPUT_OUTPUT,
        visual,
        &CreateWindowAux::new()
            .background_pixel(0x0000_0000)
            .border_pixel(0)
            .colormap(colormap)
            .override_redirect(1)
            .event_mask(EventMask::NO_EVENT),
    )?;

    let atom = conn
        .intern_atom(false, b"GAMESCOPE_EXTERNAL_OVERLAY")?
        .reply()?
        .atom;
    conn.change_property32(PropMode::REPLACE, win, atom, AtomEnum::CARDINAL, &[1])?;

    // Empty input region → clicks/touches pass through to the game/Steam.
    conn.shape_rectangles(
        shape::SO::SET,
        shape::SK::INPUT,
        ClipOrdering::UNSORTED,
        win,
        0,
        0,
        &[],
    )?;
    conn.flush()?;
    tracing::info!("external overlay ready ({w}x{h})");

    // Restore the desired state after a (re)connect.
    let mut mapped = false;
    let desired = *shared.alpha.lock().unwrap();
    if desired > 0.0 {
        show(conn, win, screen.root, desired, &mut mapped)?;
    }

    loop {
        // Drain X events so the connection buffer stays healthy (and surface errors).
        while conn.poll_for_event()?.is_some() {}

        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Cmd::Show(alpha)) => show(conn, win, screen.root, alpha, &mut mapped)?,
            Ok(Cmd::Hide) => {
                if mapped {
                    // Hide by repainting fully transparent — do NOT unmap. When
                    // the scene is idle (exactly when the overlay is up),
                    // gamescope only recomposites in response to a fresh frame on
                    // a *mapped* surface. A bare unmap produces no such frame
                    // (Xwayland tears down the wl_surface), so the last black
                    // frame stays stuck on screen. Committing a transparent frame
                    // while mapped forces the recomposite and the black clears.
                    paint(conn, win, 0.0)?;
                    tracing::debug!("external overlay hidden (transparent)");
                }
            }
            Ok(Cmd::Quit) => {
                let _ = conn.destroy_window(win);
                let _ = conn.flush();
                return Ok(());
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

fn show(
    conn: &RustConnection,
    win: Window,
    root: Window,
    alpha: f64,
    mapped: &mut bool,
) -> Result<()> {
    // Size to the real output. gamescope draws external overlays at output-native
    // pixels, but the Xwayland root reflects the *game's* render resolution (e.g.
    // 1080p) which can be smaller than the physical output (e.g. 1440p). X can't
    // see the true output size, so read it from DRM and take the larger of the
    // two so we always cover the whole screen.
    let geo = conn.get_geometry(root)?.reply()?;
    let (ow, oh) = output_size().unwrap_or((0, 0));
    let w = ow.max(geo.width);
    let h = oh.max(geo.height);
    conn.configure_window(
        win,
        &ConfigureWindowAux::new()
            .x(0)
            .y(0)
            .width(w as u32)
            .height(h as u32),
    )?;

    if !*mapped {
        conn.map_window(win)?;
        *mapped = true;
    }
    paint(conn, win, alpha)?;
    tracing::debug!("external overlay shown at alpha={alpha} ({w}x{h})");
    Ok(())
}

/// Repaint the window's whole area at the given opacity (premultiplied black, so
/// the pixel is just the alpha byte). The resulting frame is what makes gamescope
/// recomposite — used both to show (alpha > 0) and to hide (alpha 0).
fn paint(conn: &RustConnection, win: Window, alpha: f64) -> Result<()> {
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
    let pixel = a << 24;
    conn.change_window_attributes(
        win,
        &ChangeWindowAttributesAux::new().background_pixel(pixel),
    )?;
    conn.clear_area(false, win, 0, 0, 0, 0)?; // width/height 0 → whole window
    conn.flush()?;
    Ok(())
}

/// The physical output resolution, read from DRM (the largest connected
/// connector's preferred mode). Returns `None` if it can't be determined.
fn output_size() -> Option<(u16, u16)> {
    let mut best: Option<(u16, u16)> = None;
    for entry in std::fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let p = entry.path();
        let connected = std::fs::read_to_string(p.join("status"))
            .map(|s| s.trim() == "connected")
            .unwrap_or(false);
        if !connected {
            continue;
        }
        let Ok(modes) = std::fs::read_to_string(p.join("modes")) else {
            continue;
        };
        let Some(first) = modes.lines().next() else {
            continue;
        };
        let Some((w, h)) = first.trim().split_once('x') else {
            continue;
        };
        let (Ok(w), Ok(h)) = (w.parse::<u16>(), h.parse::<u16>()) else {
            continue;
        };
        if best.is_none_or(|(bw, bh)| (bw as u32 * bh as u32) < (w as u32 * h as u32)) {
            best = Some((w, h));
        }
    }
    best
}

fn find_argb_visual(screen: &Screen) -> Option<Visualid> {
    screen
        .allowed_depths
        .iter()
        .find(|d| d.depth == 32)?
        .visuals
        .iter()
        .find(|v| v.class == VisualClass::TRUE_COLOR)
        .map(|v| v.visual_id)
}
