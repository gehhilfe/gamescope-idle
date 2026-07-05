//! The long-running daemon: ties input activity, the logind inhibitor, the
//! overlay, and CEC together into a small state machine.
//!
//! `ACTIVE ──idle_timeout & !inhibited──▶ DIM ──dim_warning──▶ BLACK`
//! Any input, a `wake` command, or `SIGUSR2` returns to ACTIVE.
//! A `blank` command or `SIGUSR1` jumps straight to BLACK.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::cec::Cec;
use crate::config::Config;
use crate::control::{socket_path, Command, State};
use crate::inhibit::InhibitWatch;
use crate::input;
use crate::overlay::{self, OverlayHandle};

/// Far-future sleep used to mean "no timer in this state".
const IDLE_FOREVER: Duration = Duration::from_secs(3600);

pub async fn run(cfg: Config) -> Result<()> {
    let cfg = Arc::new(cfg);
    tracing::info!(
        "starting: idle_timeout={}s dim_warning={}s dim_alpha={}",
        cfg.idle_timeout,
        cfg.dim_warning,
        cfg.dim_alpha
    );

    // Overlay is the primary blanking mechanism; if it can't reach the compositor
    // we still run (CEC-only) rather than dying.
    let overlay: Option<OverlayHandle> = match overlay::spawn() {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::error!("overlay unavailable, running without it: {e:#}");
            None
        }
    };

    let inhibit = match InhibitWatch::connect().await {
        Ok(w) => Some(w),
        Err(e) => {
            tracing::warn!("logind unavailable, inhibitors ignored: {e:#}");
            None
        }
    };

    let cec = Cec::new(&cfg);

    // Input activity.
    let (act_tx, mut act_rx) = mpsc::channel::<()>(1);
    input::spawn(cfg.clone(), act_tx);

    // Control socket.
    let sock = socket_path();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)
        .with_context(|| format!("binding control socket {}", sock.display()))?;
    tracing::info!("control socket at {}", sock.display());

    let mut sigusr1 = signal(SignalKind::user_defined1())?;
    let mut sigusr2 = signal(SignalKind::user_defined2())?;
    let mut sigterm = signal(SignalKind::terminate())?;

    let mut m = Machine {
        cfg: cfg.clone(),
        overlay,
        cec,
        state: State::Active,
        last_activity: Instant::now(),
        dim_deadline: Instant::now(),
    };

    loop {
        let deadline = match m.state {
            State::Active => m.last_activity + cfg.idle_timeout(),
            State::Dim => m.dim_deadline,
            State::Black => Instant::now() + IDLE_FOREVER,
        };
        let timer = tokio::time::sleep_until(deadline);

        tokio::select! {
            _ = act_rx.recv() => m.on_activity().await,
            _ = timer => m.on_timeout(inhibit.as_ref()).await,
            _ = sigusr1.recv() => m.force_blank().await,
            _ = sigusr2.recv() => m.on_activity().await,
            _ = sigterm.recv() => { tracing::info!("SIGTERM"); break; }
            _ = tokio::signal::ctrl_c() => { tracing::info!("SIGINT"); break; }
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    m.handle_control(stream).await;
                }
            }
        }
    }

    tracing::info!("shutting down");
    m.wake().await;
    if let Some(o) = &m.overlay {
        o.quit();
    }
    let _ = std::fs::remove_file(&sock);
    Ok(())
}

struct Machine {
    cfg: Arc<Config>,
    overlay: Option<OverlayHandle>,
    cec: Cec,
    state: State,
    last_activity: Instant,
    dim_deadline: Instant,
}

impl Machine {
    /// Any real input.
    async fn on_activity(&mut self) {
        self.last_activity = Instant::now();
        if self.state != State::Active {
            self.wake().await;
        }
    }

    /// Return to ACTIVE: remove overlay and wake the TV if it was in standby.
    async fn wake(&mut self) {
        let was_black = self.state == State::Black;
        if let Some(o) = &self.overlay {
            o.hide();
        }
        if was_black {
            self.cec.wake().await;
        }
        if self.state != State::Active {
            tracing::info!("awake");
        }
        self.state = State::Active;
    }

    async fn on_timeout(&mut self, inhibit: Option<&InhibitWatch>) {
        match self.state {
            State::Active => {
                let blocked = match inhibit {
                    Some(w) => w.idle_blocked().await,
                    None => false,
                };
                if blocked {
                    // Something holds an idle inhibitor; defer a full cycle.
                    tracing::debug!("idle inhibited; staying awake");
                    self.last_activity = Instant::now();
                    return;
                }
                if self.cfg.dim_warning == 0 {
                    self.enter_black().await;
                } else {
                    self.enter_dim();
                }
            }
            State::Dim => self.enter_black().await,
            State::Black => {}
        }
    }

    fn enter_dim(&mut self) {
        tracing::info!("dimming");
        if let Some(o) = &self.overlay {
            o.show(self.cfg.dim_alpha);
        }
        self.dim_deadline = Instant::now() + self.cfg.dim_warning();
        self.state = State::Dim;
    }

    async fn enter_black(&mut self) {
        tracing::info!("blanking");
        if let Some(o) = &self.overlay {
            o.show(1.0);
        }
        self.cec.standby().await;
        self.state = State::Black;
    }

    /// Explicit blank request: straight to black, ignoring inhibitors.
    async fn force_blank(&mut self) {
        if self.state != State::Black {
            self.enter_black().await;
        }
    }

    async fn handle_control(&mut self, stream: UnixStream) {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        if reader.read_line(&mut line).await.is_err() {
            return;
        }
        let reply = match Command::parse(&line) {
            Some(Command::Blank) => {
                self.force_blank().await;
                "ok".to_string()
            }
            Some(Command::Wake) => {
                self.last_activity = Instant::now();
                self.wake().await;
                "ok".to_string()
            }
            Some(Command::Status) => self.state.to_string(),
            None => "error: unknown command".to_string(),
        };
        let mut stream = reader.into_inner();
        let _ = stream.write_all(reply.as_bytes()).await;
        let _ = stream.write_all(b"\n").await;
        let _ = stream.flush().await;
    }
}
