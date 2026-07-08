//! Session-bus `org.freedesktop.ScreenSaver` inhibit service.
//!
//! GUI apps (browsers, media players, and Electron apps like VacuumTube) don't
//! take a logind idle inhibitor when they play video — Chromium instead asks the
//! *session-bus* `org.freedesktop.ScreenSaver` service to `Inhibit` display
//! sleep for the duration of playback. In a bare gamescope session nobody owns
//! that name, so the request fails silently and the screen blanks over the video.
//!
//! We own the name and implement the inhibit half of the freedesktop screensaver
//! interface. While any client holds an inhibitor the daemon stays awake — so a
//! video keeps the panel lit exactly while it's actually playing, with no
//! per-app wrapper. Complements the logind watcher in [`crate::inhibit`].
//!
//! Cookies handed out by `Inhibit` are released either by an explicit
//! `UnInhibit` or, if the client crashes without one, by watching
//! `NameOwnerChanged` and dropping every cookie owned by a bus name that
//! vanished — otherwise a crash would pin the screen on forever.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use zbus::interface;

/// The two object paths clients use for `org.freedesktop.ScreenSaver`. Different
/// toolkits pick different ones (Chromium/GTK use `/ScreenSaver`, KDE also
/// exposes the fully-qualified path), so we serve the same handler at both.
const PATHS: [&str; 2] = ["/org/freedesktop/ScreenSaver", "/ScreenSaver"];

struct Holder {
    app: String,
    reason: String,
    /// Unique bus name of the caller, so we can reap its cookies if it drops off
    /// the bus without calling `UnInhibit`.
    sender: Option<String>,
}

#[derive(Default)]
struct State {
    next_cookie: u32,
    holders: HashMap<u32, Holder>,
}

/// The D-Bus object; interior-mutable so the same state is shared with the
/// daemon's [`ScreenSaverWatch`] handle.
#[derive(Clone)]
struct ScreenSaver {
    state: Arc<Mutex<State>>,
}

#[interface(name = "org.freedesktop.ScreenSaver")]
impl ScreenSaver {
    /// Take an inhibitor; returns a cookie the caller passes back to `UnInhibit`.
    fn inhibit(
        &self,
        application_name: String,
        reason_for_inhibit: String,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
    ) -> u32 {
        let sender = hdr.sender().map(|s| s.to_string());
        let mut st = self.state.lock().unwrap();
        // Cookies must be non-zero and (practically) unique.
        st.next_cookie = st.next_cookie.wrapping_add(1);
        if st.next_cookie == 0 {
            st.next_cookie = 1;
        }
        let cookie = st.next_cookie;
        tracing::debug!(
            "screensaver Inhibit from {application_name:?}: {reason_for_inhibit:?} (cookie {cookie})"
        );
        st.holders.insert(
            cookie,
            Holder {
                app: application_name,
                reason: reason_for_inhibit,
                sender,
            },
        );
        cookie
    }

    /// Release a previously-taken inhibitor.
    fn un_inhibit(&self, cookie: u32) {
        let mut st = self.state.lock().unwrap();
        if st.holders.remove(&cookie).is_some() {
            tracing::debug!("screensaver UnInhibit cookie {cookie}");
        }
    }

    // --- Remaining spec methods, stubbed so well-behaved clients don't error. ---
    // We don't run a screensaver, so there's nothing active to report or toggle.

    fn get_active(&self) -> bool {
        false
    }

    fn set_active(&self, _activate: bool) -> bool {
        false
    }

    fn get_active_time(&self) -> u32 {
        0
    }

    fn get_session_idle_time(&self) -> u32 {
        0
    }

    fn lock(&self) {}

    fn simulate_user_activity(&self) {}
}

/// Handle the daemon keeps to query inhibitor state. Owns the D-Bus connection;
/// dropping it releases the bus name and tears the service down.
pub struct ScreenSaverWatch {
    state: Arc<Mutex<State>>,
    _conn: zbus::Connection,
}

impl ScreenSaverWatch {
    /// True while any client holds a screensaver inhibitor.
    pub fn inhibited(&self) -> bool {
        !self.state.lock().unwrap().holders.is_empty()
    }

    /// Human-readable descriptions of current holders, for debug logging.
    pub fn inhibitors(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .holders
            .values()
            .map(|h| format!("{}: {}", h.app, h.reason))
            .collect()
    }
}

/// Own `org.freedesktop.ScreenSaver` on the session bus and start serving it.
pub async fn spawn() -> Result<ScreenSaverWatch> {
    let state = Arc::new(Mutex::new(State::default()));
    let obj = ScreenSaver {
        state: state.clone(),
    };

    let conn = zbus::connection::Builder::session()
        .context("connecting to the session D-Bus")?
        .build()
        .await
        .context("building session D-Bus connection")?;

    // Serve the same object at both well-known paths before claiming the name,
    // so methods are ready the instant clients can see us.
    for path in PATHS {
        conn.object_server()
            .at(path, obj.clone())
            .await
            .with_context(|| format!("serving ScreenSaver object at {path}"))?;
    }

    conn.request_name("org.freedesktop.ScreenSaver")
        .await
        .context("requesting org.freedesktop.ScreenSaver (already owned?)")?;

    spawn_disconnect_reaper(&conn, state.clone()).await?;

    tracing::info!("serving org.freedesktop.ScreenSaver on the session bus");
    Ok(ScreenSaverWatch { state, _conn: conn })
}

/// Watch `NameOwnerChanged` and drop cookies owned by any bus name that vanishes,
/// so a client that crashes without `UnInhibit` doesn't pin the screen on.
async fn spawn_disconnect_reaper(conn: &zbus::Connection, state: Arc<Mutex<State>>) -> Result<()> {
    let dbus = zbus::fdo::DBusProxy::new(conn)
        .await
        .context("creating org.freedesktop.DBus proxy")?;
    let mut changes = dbus
        .receive_name_owner_changed()
        .await
        .context("subscribing to NameOwnerChanged")?;

    tokio::spawn(async move {
        while let Some(signal) = changes.next().await {
            let Ok(args) = signal.args() else { continue };
            // A vanished owner has an empty new_owner; only unique names (":1.x")
            // ever hold our cookies.
            if args.new_owner().is_some() {
                continue;
            }
            let name = args.name().to_string();
            let mut st = state.lock().unwrap();
            let before = st.holders.len();
            st.holders
                .retain(|_, h| h.sender.as_deref() != Some(name.as_str()));
            let dropped = before - st.holders.len();
            if dropped > 0 {
                tracing::debug!(
                    "screensaver: reaped {dropped} inhibitor(s) from gone client {name}"
                );
            }
        }
    });
    Ok(())
}
