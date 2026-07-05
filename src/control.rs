//! Tiny line-based control protocol over a unix socket at
//! `$XDG_RUNTIME_DIR/gamescope-idle.sock`. Used by the `blank`, `wake`, and
//! `status` subcommands to talk to a running daemon.

use std::fmt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// The blanking state the daemon is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Active,
    Dim,
    Black,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            State::Active => "active",
            State::Dim => "dim",
            State::Black => "black",
        };
        f.write_str(s)
    }
}

/// A command sent from a control client to the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Blank,
    Wake,
    Status,
}

impl Command {
    pub fn as_str(self) -> &'static str {
        match self {
            Command::Blank => "blank",
            Command::Wake => "wake",
            Command::Status => "status",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "blank" => Some(Command::Blank),
            "wake" => Some(Command::Wake),
            "status" => Some(Command::Status),
            _ => None,
        }
    }
}

/// Socket path: `$XDG_RUNTIME_DIR/gamescope-idle.sock`, falling back to `/tmp`.
pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    dir.join("gamescope-idle.sock")
}

/// Client: connect, send one command, return the daemon's single-line reply.
pub async fn send(cmd: Command) -> Result<String> {
    let path = socket_path();
    let stream = UnixStream::connect(&path).await.with_context(|| {
        format!(
            "connecting to {} (is the gamescope-idle daemon running?)",
            path.display()
        )
    })?;
    let (read, mut write) = stream.into_split();
    write.write_all(cmd.as_str().as_bytes()).await?;
    write.write_all(b"\n").await?;
    write.flush().await?;

    let mut reader = BufReader::new(read);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    Ok(line.trim_end().to_string())
}
