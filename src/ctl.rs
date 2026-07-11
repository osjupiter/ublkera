//! Daemon control interface: newline-delimited JSON over a unix socket.
//! One request per connection keeps the protocol trivially robust.

use crate::manager::{DeviceManager, DeviceSpec};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;

pub const DEFAULT_SOCK: &str = "/run/ublkera/daemon.sock";

/// Selects one tracked device: by id, or by backing path (as stored at add).
#[derive(Deserialize)]
pub struct Target {
    #[serde(default)]
    pub dev_id: Option<u32>,
    #[serde(default)]
    pub backing: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Request {
    Add(DeviceSpec),
    Del {
        #[serde(flatten)]
        target: Target,
    },
    List,
    Status {
        #[serde(flatten)]
        target: Target,
    },
    Checkpoint {
        #[serde(flatten)]
        target: Target,
    },
    CheckpointAll,
    Dump {
        #[serde(flatten)]
        target: Target,
        #[serde(default)]
        since: u32,
        /// Expected tracking-history id (hex); mismatch is a hard error.
        #[serde(default)]
        generation: Option<String>,
    },
    Shutdown,
}

/// Bind the daemon socket. Fails if another live daemon already owns it.
pub fn bind(sock_path: &Path) -> Result<UnixListener> {
    if let Some(dir) = sock_path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    if UnixStream::connect(sock_path).is_ok() {
        anyhow::bail!(
            "another ublkera daemon is already listening on {}",
            sock_path.display()
        );
    }
    let _ = std::fs::remove_file(sock_path); // stale socket from a crashed daemon
    UnixListener::bind(sock_path).with_context(|| format!("bind {}", sock_path.display()))
}

/// Serve requests until a shutdown command arrives.
pub fn serve(listener: UnixListener, manager: Arc<DeviceManager>, sock_path: &Path) {
    log::info!("control socket at {}", sock_path.display());
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => match handle(&manager, stream) {
                Ok(shutdown) if shutdown => break,
                Ok(_) => {}
                Err(e) => log::warn!("ctl request failed: {e:#}"),
            },
            Err(e) => log::warn!("ctl accept failed: {e}"),
        }
    }
    log::info!("shutting down: detaching all devices");
    manager.shutdown_all();
    let _ = std::fs::remove_file(sock_path);
}

/// Returns Ok(true) when the daemon should shut down.
fn handle(manager: &DeviceManager, stream: UnixStream) -> Result<bool> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    let mut shutdown = false;
    let resp = match serde_json::from_str::<Request>(line.trim()) {
        Err(e) => json!({"ok": false, "error": format!("bad request: {e}")}),
        Ok(req) => {
            let result = match req {
                Request::Add(spec) => manager.add(spec),
                Request::Del { target } => {
                    manager.resolve(&target).and_then(|id| manager.del(id))
                }
                Request::List => Ok(manager.list()),
                Request::Status { target } => {
                    manager.resolve(&target).and_then(|id| manager.status(id))
                }
                Request::Checkpoint { target } => {
                    manager.resolve(&target).and_then(|id| manager.checkpoint(id))
                }
                Request::CheckpointAll => Ok(manager.checkpoint_all()),
                Request::Dump {
                    target,
                    since,
                    generation,
                } => manager
                    .resolve(&target)
                    .and_then(|id| manager.dump(id, since, generation.as_deref())),
                Request::Shutdown => {
                    shutdown = true;
                    Ok(json!({"ok": true, "shutdown": true}))
                }
            };
            result.unwrap_or_else(|e| json!({"ok": false, "error": format!("{e:#}")}))
        }
    };
    let mut stream = stream;
    stream.write_all(resp.to_string().as_bytes())?;
    stream.write_all(b"\n")?;
    Ok(shutdown)
}

/// Send one request to a running daemon and return its JSON reply.
pub fn client_request(sock_path: &Path, req: serde_json::Value) -> Result<serde_json::Value> {
    let mut stream = UnixStream::connect(sock_path).with_context(|| {
        format!(
            "connect {} (is `ublkera daemon` running?)",
            sock_path.display()
        )
    })?;
    stream.write_all(req.to_string().as_bytes())?;
    stream.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(serde_json::from_str(line.trim())?)
}
