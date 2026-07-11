mod ctl;
mod era;
mod manager;
mod target;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

use crate::manager::DeviceManager;

/// dm-era-like ublk devices with changed-block tracking (CBT).
/// A single daemon manages many tracked devices; backing devices can be
/// attached and detached while the daemon is running.
#[derive(Parser)]
#[command(name = "ublkera", version, about)]
struct Cli {
    /// daemon control socket path
    #[arg(long, global = true, default_value = ctl::DEFAULT_SOCK)]
    socket: PathBuf,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon that hosts all tracked devices
    Daemon {
        /// stay in the foreground instead of daemonizing
        #[arg(long)]
        foreground: bool,
    },
    /// Attach a backing file/device as a new tracked ublk device
    Add {
        /// backing file or block device
        #[arg(short = 'f', long)]
        backing: String,
        /// change-tracking chunk size in bytes (power of two >= 4096; K/M/G suffix ok)
        #[arg(short = 'g', long, default_value = "64K", value_parser = parse_size)]
        granularity: u64,
        /// persist era metadata to this file (saved on checkpoint and detach)
        #[arg(long)]
        meta: Option<PathBuf>,
        /// device id (-1 = auto-allocate)
        #[arg(short = 'n', long, default_value_t = -1, allow_hyphen_values = true)]
        number: i32,
        /// number of hardware queues
        #[arg(short = 'q', long, default_value_t = 1)]
        queues: u16,
        /// queue depth
        #[arg(short = 'd', long, default_value_t = 64)]
        depth: u16,
        /// max io buffer size in bytes
        #[arg(short = 'b', long, default_value = "512K", value_parser = parse_size)]
        buf_size: u64,
        /// use buffered IO on the backing file instead of O_DIRECT
        #[arg(long)]
        buffered: bool,
    },
    /// Detach a tracked device (saves its metadata)
    Del {
        /// device id
        #[arg(short = 'n', long, required_unless_present = "backing", conflicts_with = "backing")]
        number: Option<u32>,
        /// backing file/device path (alternative to -n)
        #[arg(short = 'f', long)]
        backing: Option<String>,
    },
    /// List tracked devices
    List,
    /// Show era/device status of one tracked device
    Status {
        /// device id
        #[arg(short = 'n', long, required_unless_present = "backing", conflicts_with = "backing")]
        number: Option<u32>,
        /// backing file/device path (alternative to -n)
        #[arg(short = 'f', long)]
        backing: Option<String>,
    },
    /// Close the current era and start a new one (returns the closed era)
    Checkpoint {
        /// device id
        #[arg(short = 'n', long, required_unless_present_any = ["all", "backing"], conflicts_with_all = ["all", "backing"])]
        number: Option<u32>,
        /// backing file/device path (alternative to -n)
        #[arg(short = 'f', long, conflicts_with = "all")]
        backing: Option<String>,
        /// checkpoint every tracked device
        #[arg(long)]
        all: bool,
    },
    /// Dump changed ranges as JSON
    Dump {
        /// device id
        #[arg(short = 'n', long, required_unless_present = "backing", conflicts_with = "backing")]
        number: Option<u32>,
        /// backing file/device path (alternative to -n)
        #[arg(short = 'f', long)]
        backing: Option<String>,
        /// only ranges written in an era newer than this (0 = everything)
        #[arg(long, default_value_t = 0)]
        since: u32,
        /// tracking-history id (hex, from add/status); mismatch is an error
        #[arg(long)]
        generation: Option<String>,
    },
    /// Detach all devices and stop the daemon
    Shutdown,
}

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('K') | Some('k') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M') | Some('m') => (&s[..s.len() - 1], 1u64 << 20),
        Some('G') | Some('g') => (&s[..s.len() - 1], 1u64 << 30),
        _ => (s, 1),
    };
    num.parse::<u64>()
        .map(|n| n * mult)
        .map_err(|e| format!("invalid size '{s}': {e}"))
}

fn run_daemon(sock_path: PathBuf, foreground: bool) -> Result<()> {
    // Bind before daemonizing so "already running" is reported to the caller.
    let listener = ctl::bind(&sock_path)?;

    if !foreground {
        // stdio must be detached (the daemonize default: /dev/null): keeping
        // the launching terminal's fds makes any later write fail with EIO
        // once that terminal goes away, and a failed println! panics.
        daemonize::Daemonize::new()
            .start()
            .context("daemonize failed")?;
    }

    let manager = Arc::new(DeviceManager::default());

    // SIGTERM/SIGINT detach everything (saving metadata) before exiting.
    let sig_manager = manager.clone();
    let sig_sock = sock_path.clone();
    let mut signals = signal_hook::iterator::Signals::new([libc::SIGTERM, libc::SIGINT])?;
    std::thread::spawn(move || {
        if let Some(sig) = signals.forever().next() {
            log::info!("signal {sig}: detaching all devices");
            sig_manager.shutdown_all();
            let _ = std::fs::remove_file(&sig_sock);
            std::process::exit(0);
        }
    });

    ctl::serve(listener, manager, &sock_path);
    Ok(())
}

fn ctl_call(sock_path: &std::path::Path, req: serde_json::Value) -> Result<()> {
    let resp = ctl::client_request(sock_path, req)?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    if resp.get("ok") == Some(&json!(false)) {
        std::process::exit(1);
    }
    Ok(())
}

fn main() -> Result<()> {
    env_logger::builder()
        .format_target(false)
        .format_timestamp(None)
        .init();

    let cli = Cli::parse();
    let sock = cli.socket.as_path();
    match cli.command {
        Cmd::Daemon { foreground } => run_daemon(sock.to_path_buf(), foreground),
        Cmd::Add {
            backing,
            granularity,
            meta,
            number,
            queues,
            depth,
            buf_size,
            buffered,
        } => ctl_call(
            sock,
            json!({
                "cmd": "add",
                "backing": canonical_path(backing),
                "granularity": granularity,
                "meta": meta,
                "dev_id": number,
                "queues": queues,
                "depth": depth,
                "buf_size": buf_size,
                "buffered": buffered,
            }),
        ),
        Cmd::Del { number, backing } => ctl_call(sock, target_req("del", number, backing)),
        Cmd::List => ctl_call(sock, json!({"cmd": "list"})),
        Cmd::Status { number, backing } => ctl_call(sock, target_req("status", number, backing)),
        Cmd::Checkpoint { number, backing, all } => {
            if all {
                ctl_call(sock, json!({"cmd": "checkpoint_all"}))
            } else {
                ctl_call(sock, target_req("checkpoint", number, backing))
            }
        }
        Cmd::Dump { number, backing, since, generation } => {
            let mut req = target_req("dump", number, backing);
            req["since"] = json!(since);
            if let Some(g) = generation {
                req["generation"] = json!(g);
            }
            ctl_call(sock, req)
        }
        Cmd::Shutdown => ctl_call(sock, json!({"cmd": "shutdown"})),
    }
}

/// Request body for commands that address one device: by id or backing path.
/// Paths are canonicalized so relative paths and symlinks match what the
/// daemon has stored.
fn target_req(cmd: &str, number: Option<u32>, backing: Option<String>) -> serde_json::Value {
    let mut req = json!({ "cmd": cmd });
    if let Some(n) = number {
        req["dev_id"] = json!(n);
    }
    if let Some(b) = backing {
        req["backing"] = json!(canonical_path(b));
    }
    req
}

/// The daemon runs with cwd `/`, so paths must be absolute by the time they
/// reach it; canonicalizing also makes `-f` lookups match through symlinks.
fn canonical_path(p: String) -> String {
    std::fs::canonicalize(&p).map_or(p, |c| c.display().to_string())
}
