//! ublkfault: a fault-injecting in-memory ublk device (the SST lower layer,
//! see docs/sst.md). `serve` runs one device in the foreground and prints a
//! ready JSON line; the other subcommands talk to its control socket.
//!
//! Requests are normally served straight from the in-memory model. The two
//! completion-level faults live here rather than in the model:
//!
//! * hang: a request is parked without completion — the submitting process
//!   sits in D state until a `thaw` command releases every parked request
//!   (either performing it, or failing it with EIO). The wakeup travels as
//!   an eventfd POLL_ADD on the same io_uring the queue thread sleeps on.
//! * error: injected inside the model (an errored write may still leave a
//!   partial volatile entry).

mod model;
mod scenario;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use io_uring::{opcode, types};
use libublk::io::{BufDescList, UblkDev, UblkIOCtx, UblkQueue};
use libublk::{BufDesc, UblkError, UblkFlags, UblkIORes};
use model::{CrashMode, Model, Policy};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

/// pseudo-tag for the eventfd poll CQE; real tags are bounded by queue depth
const POLL_TAG: u16 = 0xFFF;

/// ublk_cmd.h; missing from libublk's generated bindings. Without
/// VOLATILE_CACHE the kernel treats the device as write-through and never
/// issues FLUSH — the whole volatile-cache model would be unreachable.
const UBLK_ATTR_VOLATILE_CACHE: u32 = 1 << 2;
/// ublk_cmd.h; also missing from the bindings. A capable disk model should
/// take WRITE_ZEROES: block-device fallocate(PUNCH_HOLE) — which is how a
/// passthrough SUT forwards DISCARD to us — is implemented with it and has
/// no fallback.
const UBLK_IO_OP_WRITE_ZEROES: u32 = 5;

#[derive(Parser)]
#[command(about = "fault-injecting in-memory ublk device for testing ublk targets")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// create the device and serve IO in the foreground (prints ready JSON)
    Serve {
        /// device size (bytes; K/M/G suffixes)
        #[arg(long, default_value = "64M", value_parser = parse_size)]
        size: u64,
        /// logical block size = torn granularity
        #[arg(long, default_value_t = 4096)]
        lbs: u32,
        /// seed for every fault decision
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// volatile cache capacity before autonomous writeback
        #[arg(long, default_value = "8M", value_parser = parse_size)]
        cache: u64,
        /// control socket path
        #[arg(long, default_value = "/tmp/ublkfault.sock")]
        socket: PathBuf,
        /// per-mill of WRITEs failed with EIO
        #[arg(long, default_value_t = 0)]
        error_pm: u32,
        /// per-mill of FLUSHes acked without committing
        #[arg(long, default_value_t = 0)]
        flush_lie_pm: u32,
        /// per-mill of requests parked without completion (D state)
        #[arg(long, default_value_t = 0)]
        hang_pm: u32,
        #[arg(long, default_value_t = 1)]
        queues: u16,
        #[arg(long, default_value_t = 64)]
        depth: u16,
    },
    /// show policy, stats, volatile/parked state
    Status {
        #[arg(long, default_value = "/tmp/ublkfault.sock")]
        socket: PathBuf,
    },
    /// adjust the fault policy at runtime
    Set {
        #[arg(long, default_value = "/tmp/ublkfault.sock")]
        socket: PathBuf,
        #[arg(long)]
        error_pm: Option<u32>,
        #[arg(long)]
        flush_lie_pm: Option<u32>,
        #[arg(long)]
        hang_pm: Option<u32>,
    },
    /// simulate power loss: only a subset of the volatile cache survives
    Crash {
        #[arg(long, default_value = "/tmp/ublkfault.sock")]
        socket: PathBuf,
        /// drop = lose the whole volatile cache; seeded = per-entry
        /// commit/drop/tear decided by the seed
        #[arg(long, default_value = "seeded")]
        mode: String,
    },
    /// release every parked (hung) request
    Thaw {
        #[arg(long, default_value = "/tmp/ublkfault.sock")]
        socket: PathBuf,
        /// ok = perform the request now; eio = fail it
        #[arg(long, default_value = "ok")]
        result: String,
    },
    /// run declarative scenario files against a device (see src/scenario.rs)
    Scenario {
        #[arg(long, default_value = "/tmp/ublkfault.sock")]
        socket: PathBuf,
        /// block device the steps read/write (the fault device itself, or a
        /// SUT stacked on top of one)
        #[arg(long)]
        dev: String,
        /// scenario files, executed in order
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
}

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mul) = match s.chars().last() {
        Some('K') | Some('k') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M') | Some('m') => (&s[..s.len() - 1], 1u64 << 20),
        Some('G') | Some('g') => (&s[..s.len() - 1], 1u64 << 30),
        _ => (s, 1),
    };
    num.parse::<u64>()
        .map(|n| n * mul)
        .map_err(|e| format!("bad size '{s}': {e}"))
}

struct Shared {
    model: Mutex<Model>,
    parked: Mutex<Vec<u16>>,
    /// 0 = thaw performs the request; negative errno = thaw fails it
    thaw_result: AtomicI32,
    efd: i32,
}

fn main() -> Result<()> {
    env_logger::init();
    match Cli::parse().cmd {
        Cmd::Serve {
            size,
            lbs,
            seed,
            cache,
            socket,
            error_pm,
            flush_lie_pm,
            hang_pm,
            queues,
            depth,
        } => {
            if size == 0 || size % lbs as u64 != 0 || !lbs.is_power_of_two() || lbs < 512 {
                bail!("size must be a non-zero multiple of lbs (power of two >= 512)");
            }
            let policy = Policy { error_pm, flush_lie_pm, hang_pm };
            serve(size, lbs, seed, cache, &socket, policy, queues, depth)
        }
        Cmd::Status { socket } => request(&socket, json!({"cmd": "status"})),
        Cmd::Set { socket, error_pm, flush_lie_pm, hang_pm } => request(
            &socket,
            json!({
                "cmd": "set",
                "error_pm": error_pm,
                "flush_lie_pm": flush_lie_pm,
                "hang_pm": hang_pm,
            }),
        ),
        Cmd::Crash { socket, mode } => request(&socket, json!({"cmd": "crash", "mode": mode})),
        Cmd::Thaw { socket, result } => {
            request(&socket, json!({"cmd": "thaw", "result": result}))
        }
        Cmd::Scenario { socket, dev, files } => {
            for f in &files {
                scenario::run_file(f, &dev, &socket)?;
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- client

fn request(socket: &PathBuf, req: Value) -> Result<()> {
    let mut conn = UnixStream::connect(socket)
        .with_context(|| format!("connect {} (is `serve` running?)", socket.display()))?;
    writeln!(conn, "{req}")?;
    let mut line = String::new();
    BufReader::new(&conn).read_line(&mut line)?;
    let resp: Value = serde_json::from_str(line.trim()).context("parse response")?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    if resp["ok"].as_bool() != Some(true) {
        bail!("request failed");
    }
    Ok(())
}

// ---------------------------------------------------------------- server

#[allow(clippy::too_many_arguments)]
fn serve(
    size: u64,
    lbs: u32,
    seed: u64,
    cache: u64,
    socket: &PathBuf,
    policy: Policy,
    queues: u16,
    depth: u16,
) -> Result<()> {
    let efd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    if efd < 0 {
        bail!("eventfd: {}", std::io::Error::last_os_error());
    }
    let shared = Arc::new(Shared {
        model: Mutex::new(Model::new(size, lbs, cache, seed, policy)),
        parked: Mutex::new(Vec::new()),
        thaw_result: AtomicI32::new(0),
        efd,
    });

    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("bind control socket {}", socket.display()))?;
    {
        let shared = shared.clone();
        std::thread::spawn(move || control_loop(listener, shared));
    }

    let ctrl = libublk::ctrl::UblkCtrlBuilder::default()
        .name("ublkfault")
        .id(-1)
        .nr_queues(queues)
        .depth(depth)
        .io_buf_bytes(512 * 1024)
        .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV)
        .build()
        .map_err(|e| anyhow::anyhow!("create ublk control device: {e} (root or ublk_drv missing?)"))?;

    let lbs_shift = lbs.trailing_zeros() as u8;
    let init = move |dev: &mut UblkDev| -> Result<(), UblkError> {
        let t = &mut dev.tgt;
        t.dev_size = size;
        t.params = libublk::sys::ublk_params {
            types: libublk::sys::UBLK_PARAM_TYPE_BASIC | libublk::sys::UBLK_PARAM_TYPE_DISCARD,
            basic: libublk::sys::ublk_param_basic {
                attrs: UBLK_ATTR_VOLATILE_CACHE,
                logical_bs_shift: lbs_shift,
                physical_bs_shift: lbs_shift,
                io_opt_shift: 12,
                io_min_shift: lbs_shift,
                max_sectors: dev.dev_info.max_io_buf_bytes >> 9,
                dev_sectors: size >> 9,
                ..Default::default()
            },
            discard: libublk::sys::ublk_param_discard {
                discard_granularity: lbs,
                max_discard_sectors: 1 << 22,
                max_write_zeroes_sectors: 1 << 22,
                max_discard_segments: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        Ok(())
    };

    let q_shared = shared.clone();
    let queue_handler = move |qid: u16, dev: &UblkDev| queue_fn(qid, dev, q_shared.clone());

    let ready = move |ctrl: &libublk::ctrl::UblkCtrl| {
        let id = ctrl.dev_info().dev_id;
        println!(
            "{}",
            json!({
                "ok": true,
                "dev_id": id,
                "bdev": format!("/dev/ublkb{id}"),
                "size": size,
                "lbs": lbs,
                "seed": seed,
            })
        );
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
    };

    ctrl.run_target(init, queue_handler, ready)
        .map_err(|e| anyhow::anyhow!("run_target: {e}"))?;
    Ok(())
}

fn control_loop(listener: UnixListener, shared: Arc<Shared>) {
    for conn in listener.incoming().flatten() {
        let mut reader = BufReader::new(match conn.try_clone() {
            Ok(c) => c,
            Err(_) => continue,
        });
        let mut conn = conn;
        let mut line = String::new();
        while {
            line.clear();
            reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false)
        } {
            let resp = handle_ctl(&shared, line.trim());
            if writeln!(conn, "{resp}").is_err() {
                break;
            }
        }
    }
}

fn handle_ctl(shared: &Shared, line: &str) -> Value {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "error": format!("bad request: {e}")}),
    };
    match req["cmd"].as_str() {
        Some("status") => {
            let m = shared.model.lock().unwrap();
            json!({
                "ok": true,
                "policy": m.policy,
                "stats": m.stats,
                "volatile_bytes": m.volatile_bytes(),
                "parked": shared.parked.lock().unwrap().len(),
            })
        }
        Some("set") => {
            let mut m = shared.model.lock().unwrap();
            for (key, field) in [
                ("error_pm", &mut m.policy.error_pm as *mut u32),
                ("flush_lie_pm", &mut m.policy.flush_lie_pm as *mut u32),
                ("hang_pm", &mut m.policy.hang_pm as *mut u32),
            ] {
                if let Some(v) = req[key].as_u64() {
                    if v > 1000 {
                        return json!({"ok": false, "error": format!("{key} > 1000")});
                    }
                    unsafe { *field = v as u32 };
                }
            }
            json!({"ok": true, "policy": m.policy})
        }
        Some("crash") => {
            let mode = match req["mode"].as_str() {
                Some("drop") => CrashMode::Drop,
                Some("seeded") | None => CrashMode::Seeded,
                Some(other) => {
                    return json!({"ok": false, "error": format!("unknown mode '{other}'")})
                }
            };
            let report = shared.model.lock().unwrap().crash(mode);
            json!({"ok": true, "report": report})
        }
        Some("thaw") => {
            let res = match req["result"].as_str() {
                Some("ok") | None => 0,
                Some("eio") => -libc::EIO,
                Some(other) => {
                    return json!({"ok": false, "error": format!("unknown result '{other}'")})
                }
            };
            let parked = shared.parked.lock().unwrap().len();
            shared.thaw_result.store(res, Ordering::Release);
            let one: u64 = 1;
            unsafe { libc::write(shared.efd, &one as *const u64 as *const libc::c_void, 8) };
            json!({"ok": true, "parked": parked})
        }
        _ => json!({"ok": false, "error": "unknown cmd (status/set/crash/thaw)"}),
    }
}

// ---------------------------------------------------------------- queue

fn submit_poll(q: &UblkQueue, efd: i32) {
    let sqe = opcode::PollAdd::new(types::Fd(efd), libc::POLLIN as u32)
        .build()
        .user_data(UblkIOCtx::build_user_data(POLL_TAG, 0, 0, true));
    q.ublk_submit_sqe_sync(sqe).expect("submit eventfd poll");
}

/// Serve one request from the model; returns the completion result.
fn perform(m: &mut Model, op: u32, off: u64, len: u64, buf: &mut [u8]) -> i32 {
    match op {
        libublk::sys::UBLK_IO_OP_READ => {
            m.read(off, &mut buf[..len as usize]);
            len as i32
        }
        libublk::sys::UBLK_IO_OP_WRITE => m.write(off, &buf[..len as usize]),
        libublk::sys::UBLK_IO_OP_FLUSH => {
            m.flush();
            0
        }
        // both zero the range in the model; an unflushed discard/zero-out can
        // still be undone by a crash, like any volatile write
        libublk::sys::UBLK_IO_OP_DISCARD | UBLK_IO_OP_WRITE_ZEROES => {
            m.discard(off, len);
            0
        }
        _ => -libc::EINVAL,
    }
}

fn queue_fn(qid: u16, dev: &UblkDev, shared: Arc<Shared>) {
    if let Ok(name) = std::ffi::CString::new(format!("ublkfault-q{qid}")) {
        unsafe { libc::prctl(libc::PR_SET_NAME, name.as_ptr()) };
    }

    let bufs = Rc::new(dev.alloc_queue_io_bufs());
    // Raw per-tag pointers: the kernel DMAs into these buffers, and we both
    // read (WRITE payload) and fill (READ) them; each tag's buffer has a
    // single owner at any time (the request state machine).
    let ptrs: Vec<(*mut u8, usize)> = bufs.iter().map(|b| (b.as_mut_ptr(), b.len())).collect();
    let bufs_h = bufs.clone();
    let efd = shared.efd;

    let handler = move |q: &UblkQueue, tag: u16, io: &UblkIOCtx| {
        // control event: the eventfd fired — thaw every parked request
        if io.is_tgt_io() && tag == POLL_TAG {
            let mut n: u64 = 0;
            unsafe { libc::read(shared.efd, &mut n as *mut u64 as *mut libc::c_void, 8) };
            let thaw = shared.thaw_result.load(Ordering::Acquire);
            let tags: Vec<u16> = shared.parked.lock().unwrap().drain(..).collect();
            for t in tags {
                let iod = q.get_iod(t);
                let op = iod.op_flags & 0xff;
                let off = iod.start_sector << 9;
                let len = (iod.nr_sectors as u64) << 9;
                let (ptr, blen) = ptrs[t as usize];
                let buf = unsafe { std::slice::from_raw_parts_mut(ptr, blen) };
                let res = if thaw == 0 {
                    perform(&mut shared.model.lock().unwrap(), op, off, len, buf)
                } else {
                    thaw
                };
                q.complete_io_cmd_unified(t, BufDesc::Slice(bufs_h[t as usize].as_slice()), Ok(UblkIORes::Result(res)))
                    .unwrap();
            }
            submit_poll(q, shared.efd); // POLL_ADD is one-shot; re-arm
            return;
        }

        let iod = q.get_iod(tag);
        let op = iod.op_flags & 0xff;
        let off = iod.start_sector << 9;
        let len = (iod.nr_sectors as u64) << 9;
        let (ptr, blen) = ptrs[tag as usize];
        let buf = unsafe { std::slice::from_raw_parts_mut(ptr, blen) };

        let res = {
            let mut m = shared.model.lock().unwrap();
            let hang_pm = m.policy.hang_pm;
            if m.rng.hit(hang_pm) {
                // park without completing: the submitter sits in D state
                // until a `thaw` control command releases it
                m.stats.hangs += 1;
                drop(m);
                shared.parked.lock().unwrap().push(tag);
                return;
            }
            perform(&mut m, op, off, len, buf)
        };
        q.complete_io_cmd_unified(tag, BufDesc::Slice(bufs_h[tag as usize].as_slice()), Ok(UblkIORes::Result(res)))
            .unwrap();
    };

    let queue = match UblkQueue::new(qid, dev)
        .and_then(|q| q.submit_fetch_commands_unified(BufDescList::Slices(Some(&bufs))))
    {
        Ok(q) => q,
        Err(e) => {
            log::error!("queue {qid} setup failed: {e}");
            return;
        }
    };
    submit_poll(&queue, efd);
    queue.wait_and_handle_io(handler);
}
