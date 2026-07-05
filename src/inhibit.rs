//! logind (`org.freedesktop.login1`) integration.
//!
//! Two roles:
//!  * The daemon watches whether an **idle** inhibitor is held (`BlockInhibited`
//!    contains `idle`) and stays awake while one is.
//!  * The `inhibit` subcommand takes an `Inhibit("idle", …, "block")` lock and
//!    holds it for the lifetime of a child process — the branded, self-contained
//!    equivalent of `systemd-inhibit --what=idle`.

use std::ffi::OsString;

use anyhow::{Context, Result};
use zbus::zvariant::OwnedFd;

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Login1Manager {
    /// Colon-separated list of currently block-inhibited actions, e.g. `idle:sleep`.
    #[zbus(property)]
    fn block_inhibited(&self) -> zbus::Result<String>;

    /// Take an inhibitor lock; the returned fd holds it until closed.
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;
}

/// Handle used by the daemon to query inhibitor state.
pub struct InhibitWatch {
    proxy: Login1ManagerProxy<'static>,
}

impl InhibitWatch {
    pub async fn connect() -> Result<Self> {
        let conn = zbus::Connection::system()
            .await
            .context("connecting to the system D-Bus")?;
        let proxy = Login1ManagerProxy::new(&conn)
            .await
            .context("creating logind proxy")?;
        Ok(Self { proxy })
    }

    /// True if something currently holds an `idle` block-inhibitor.
    pub async fn idle_blocked(&self) -> bool {
        match self.proxy.block_inhibited().await {
            Ok(list) => list.split(':').any(|w| w == "idle"),
            Err(e) => {
                tracing::warn!("could not read BlockInhibited: {e}");
                false
            }
        }
    }
}

/// Implementation of the `inhibit` subcommand: hold an idle inhibitor while
/// running `program` with `args`, returning its exit code.
pub async fn run_inhibited(why: &str, program: OsString, args: Vec<OsString>) -> Result<i32> {
    let conn = zbus::Connection::system()
        .await
        .context("connecting to the system D-Bus")?;
    let proxy = Login1ManagerProxy::new(&conn).await?;

    // Keep the fd alive for the duration of the child; dropping it releases the lock.
    let _lock: OwnedFd = proxy
        .inhibit("idle", "gamescope-idle", why, "block")
        .await
        .context("taking logind idle inhibitor")?;

    let status = tokio::process::Command::new(&program)
        .args(&args)
        .status()
        .await
        .with_context(|| format!("running {}", program.to_string_lossy()))?;

    Ok(status.code().unwrap_or(1))
}
