//! gamescope-idle — controller-aware idle blanking for Steam Gaming Mode.
//!
//! See the README for the "why": gamescope exposes no idle protocol and
//! controllers never reach the compositor, so idle must be detected from evdev
//! and the panel blanked with a black overlay (+ optional CEC standby).

mod cec;
mod config;
mod control;
mod daemon;
mod hid;
mod inhibit;
mod input;
mod overlay;

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use config::Config;
use control::Command as CtlCommand;

#[derive(Parser)]
#[command(name = "gamescope-idle", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the idle daemon (started by the systemd user unit).
    Daemon {
        /// Config file path (default: $XDG_CONFIG_HOME/gamescope-idle/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Blank the screen now (straight to black).
    Blank,
    /// Wake the screen now.
    Wake,
    /// Print the daemon's current state (active/dim/black).
    Status,
    /// Run a command while holding a logind idle inhibitor (prevents blanking).
    Inhibit {
        /// Reason shown in `systemd-inhibit --list`.
        #[arg(long, default_value = "gamescope-idle inhibitor")]
        why: String,
        /// The command to run, e.g. `-- couchcast`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<OsString>,
    },
    /// Hidden: show the overlay for N seconds (used to validate compositor support).
    #[command(hide = true)]
    OverlayTest {
        #[arg(long, default_value_t = 1.0)]
        alpha: f64,
        #[arg(long, default_value_t = 3)]
        seconds: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Daemon { config } => {
            let cfg = Config::load(config)?;
            daemon::run(cfg).await
        }
        Cmd::Blank => print_reply(CtlCommand::Blank).await,
        Cmd::Wake => print_reply(CtlCommand::Wake).await,
        Cmd::Status => print_reply(CtlCommand::Status).await,
        Cmd::Inhibit { why, command } => {
            let mut it = command.into_iter();
            let program = it.next().expect("required=true guarantees one arg");
            let args: Vec<OsString> = it.collect();
            let code = inhibit::run_inhibited(&why, program, args).await?;
            std::process::exit(code);
        }
        Cmd::OverlayTest { alpha, seconds } => {
            let handle = overlay::spawn()?;
            handle.show(alpha);
            tracing::info!("overlay shown at alpha={alpha} for {seconds}s");
            tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
            handle.quit();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            Ok(())
        }
    }
}

async fn print_reply(cmd: CtlCommand) -> Result<()> {
    let reply = control::send(cmd).await?;
    println!("{reply}");
    Ok(())
}
