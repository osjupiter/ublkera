//! Runtime registry of tracked devices. Each device runs on its own
//! supervisor thread (libublk keeps its control/queue io_urings in
//! thread-locals, so devices are fully independent within one process),
//! which lets devices be attached and detached while the daemon lives.

use crate::era::EraState;
use crate::target::{self, EraTgt};
use anyhow::{bail, Context, Result};
use libublk::ctrl::UblkCtrl;
use libublk::UblkFlags;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

fn default_granularity() -> u64 {
    64 * 1024
}
fn default_dev_id() -> i32 {
    -1
}
fn default_queues() -> u16 {
    1
}
fn default_depth() -> u16 {
    64
}
fn default_buf_size() -> u32 {
    512 * 1024
}

#[derive(Clone, Deserialize)]
pub struct DeviceSpec {
    pub backing: String,
    #[serde(default = "default_granularity")]
    pub granularity: u64,
    #[serde(default)]
    pub meta: Option<PathBuf>,
    #[serde(default = "default_dev_id")]
    pub dev_id: i32,
    #[serde(default = "default_queues")]
    pub queues: u16,
    #[serde(default = "default_depth")]
    pub depth: u16,
    #[serde(default = "default_buf_size")]
    pub buf_size: u32,
    #[serde(default)]
    pub buffered: bool,
}

struct Managed {
    dev_id: u32,
    backing: String,
    meta: Option<PathBuf>,
    state: Arc<EraState>,
    supervisor: JoinHandle<()>,
}

#[derive(Default)]
pub struct DeviceManager {
    devices: Mutex<HashMap<u32, Managed>>,
}

impl DeviceManager {
    /// Reap devices whose supervisor thread exited (e.g. the ublk device was
    /// deleted externally with another tool); metadata was already saved by
    /// the supervisor on its way out.
    fn reap(&self) {
        let mut devs = self.devices.lock().unwrap();
        let dead: Vec<u32> = devs
            .iter()
            .filter(|(_, m)| m.supervisor.is_finished())
            .map(|(id, _)| *id)
            .collect();
        for id in dead {
            log::info!("reaping device {id} (supervisor exited)");
            let m = devs.remove(&id).unwrap();
            let _ = m.supervisor.join();
        }
    }

    pub fn add(&self, spec: DeviceSpec) -> Result<Value> {
        self.reap();

        let back_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&spec.backing)
            .with_context(|| format!("open backing '{}'", spec.backing))?;
        let (dev_size, lbs_shift, pbs_shift) = target::backing_size(&back_file)?;
        if dev_size == 0 || dev_size % 512 != 0 {
            bail!("backing size {dev_size} must be a non-zero multiple of 512");
        }
        {
            let devs = self.devices.lock().unwrap();
            if devs.values().any(|m| m.backing == spec.backing) {
                bail!("'{}' is already attached", spec.backing);
            }
        }

        let state = Arc::new(match &spec.meta {
            Some(path) if path.exists() => {
                let s = EraState::load(path, dev_size, spec.granularity)?;
                log::info!(
                    "loaded metadata from {} (era {}, {} written chunks)",
                    path.display(),
                    s.current_era(),
                    s.written_chunks()
                );
                s
            }
            _ => EraState::new(dev_size, spec.granularity)?,
        });

        // The supervisor owns the whole device lifetime. It reports readiness
        // (or the startup error) exactly once through this channel.
        let (ready_tx, ready_rx) = mpsc::channel::<Result<u32>>();
        let sup_state = state.clone();
        let sup_spec = spec.clone();
        let supervisor = std::thread::Builder::new()
            .name(format!("ublkera-dev-{}", spec.backing))
            .spawn(move || {
                supervise_device(sup_spec, back_file, dev_size, (lbs_shift, pbs_shift), sup_state, ready_tx)
            })
            .context("spawn supervisor thread")?;

        let dev_id = match ready_rx.recv_timeout(std::time::Duration::from_secs(15)) {
            Ok(Ok(id)) => id,
            Ok(Err(e)) => {
                let _ = supervisor.join();
                return Err(e.context(format!("start device for '{}'", spec.backing)));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // The supervisor died without reporting: a panic during startup.
                let _ = supervisor.join();
                bail!(
                    "supervisor for '{}' died during startup (panicked)",
                    spec.backing
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                bail!("device for '{}' did not come up within 15s", spec.backing)
            }
        };

        let mut devs = self.devices.lock().unwrap();
        devs.insert(
            dev_id,
            Managed {
                dev_id,
                backing: spec.backing.clone(),
                meta: spec.meta.clone(),
                state: state.clone(),
                supervisor,
            },
        );
        Ok(json!({
            "ok": true,
            "dev_id": dev_id,
            "bdev": format!("/dev/ublkb{dev_id}"),
            "backing": spec.backing,
            "dev_size": dev_size,
            "granularity": spec.granularity,
            "current_era": state.current_era(),
        }))
    }

    /// Resolve a request target to a device id: a dev_id passes through, a
    /// backing path is looked up among tracked devices (canonicalized so
    /// symlinks and relative paths still match).
    pub fn resolve(&self, target: &crate::ctl::Target) -> Result<u32> {
        match (target.dev_id, target.backing.as_deref()) {
            (Some(id), None) => Ok(id),
            (Some(_), Some(_)) => bail!("specify either dev_id or backing, not both"),
            (None, Some(b)) => {
                self.reap();
                let canon = std::fs::canonicalize(b).ok();
                let devs = self.devices.lock().unwrap();
                devs.values()
                    .find(|m| {
                        m.backing == b
                            || canon.is_some()
                                && std::fs::canonicalize(&m.backing).ok() == canon
                    })
                    .map(|m| m.dev_id)
                    .with_context(|| format!("no tracked device with backing '{b}'"))
            }
            (None, None) => bail!("request needs dev_id or backing"),
        }
    }

    pub fn del(&self, dev_id: u32) -> Result<Value> {
        self.reap();
        let m = {
            let mut devs = self.devices.lock().unwrap();
            devs.remove(&dev_id)
                .with_context(|| format!("device {dev_id} is not managed by this daemon"))?
        };
        // Same path an external `ublk del` would take; the supervisor sees the
        // queues go down, saves metadata and exits.
        UblkCtrl::new_simple(dev_id as i32)
            .and_then(|c| c.del_dev())
            .map_err(|e| anyhow::anyhow!("delete ublk device {dev_id}: {e}"))?;
        m.supervisor
            .join()
            .map_err(|_| anyhow::anyhow!("supervisor thread for device {dev_id} panicked"))?;
        Ok(json!({"ok": true, "dev_id": dev_id, "backing": m.backing}))
    }

    pub fn list(&self) -> Value {
        self.reap();
        let devs = self.devices.lock().unwrap();
        let mut list: Vec<Value> = devs.values().map(device_status).collect();
        list.sort_by_key(|v| v["dev_id"].as_u64());
        json!({"ok": true, "devices": list})
    }

    pub fn status(&self, dev_id: u32) -> Result<Value> {
        self.reap();
        let devs = self.devices.lock().unwrap();
        let m = lookup(&devs, dev_id)?;
        let mut v = device_status(m);
        v["ok"] = json!(true);
        Ok(v)
    }

    pub fn checkpoint(&self, dev_id: u32) -> Result<Value> {
        self.reap();
        let (state, meta, backing) = {
            let devs = self.devices.lock().unwrap();
            let m = lookup(&devs, dev_id)?;
            (m.state.clone(), m.meta.clone(), m.backing.clone())
        };
        Ok(checkpoint_one(dev_id, &backing, &state, meta.as_deref()))
    }

    pub fn checkpoint_all(&self) -> Value {
        self.reap();
        let targets: Vec<(u32, String, Arc<EraState>, Option<PathBuf>)> = {
            let devs = self.devices.lock().unwrap();
            devs.values()
                .map(|m| (m.dev_id, m.backing.clone(), m.state.clone(), m.meta.clone()))
                .collect()
        };
        let results: Vec<Value> = targets
            .iter()
            .map(|(id, backing, state, meta)| checkpoint_one(*id, backing, state, meta.as_deref()))
            .collect();
        json!({"ok": true, "devices": results})
    }

    pub fn dump(&self, dev_id: u32, since: u32) -> Result<Value> {
        self.reap();
        let state = {
            let devs = self.devices.lock().unwrap();
            lookup(&devs, dev_id)?.state.clone()
        };
        let ranges = state.ranges_since(since);
        let dirty_bytes: u64 = ranges.iter().map(|r| r.len).sum();
        Ok(json!({
            "ok": true,
            "dev_id": dev_id,
            "current_era": state.current_era(),
            "since": since,
            "granularity": state.granularity,
            "dirty_bytes": dirty_bytes,
            "ranges": ranges,
        }))
    }

    /// Detach everything (metadata is saved by each supervisor).
    pub fn shutdown_all(&self) {
        let ids: Vec<u32> = self.devices.lock().unwrap().keys().copied().collect();
        for id in ids {
            if let Err(e) = self.del(id) {
                log::error!("shutdown: detach device {id} failed: {e:#}");
            }
        }
    }
}

fn lookup(devs: &HashMap<u32, Managed>, dev_id: u32) -> Result<&Managed> {
    devs.get(&dev_id)
        .with_context(|| format!("device {dev_id} is not managed by this daemon"))
}

fn device_status(m: &Managed) -> Value {
    json!({
        "dev_id": m.dev_id,
        "bdev": format!("/dev/ublkb{}", m.dev_id),
        "backing": m.backing,
        "dev_size": m.state.dev_size,
        "granularity": m.state.granularity,
        "current_era": m.state.current_era(),
        "nr_chunks": m.state.nr_chunks(),
        "written_chunks": m.state.written_chunks(),
        "meta_path": m.meta.as_ref().map(|p| p.display().to_string()),
    })
}

fn checkpoint_one(
    dev_id: u32,
    backing: &str,
    state: &EraState,
    meta: Option<&std::path::Path>,
) -> Value {
    let (closed, current) = state.checkpoint();
    let mut resp = json!({
        "ok": true,
        "dev_id": dev_id,
        "backing": backing,
        "closed_era": closed,
        "current_era": current,
    });
    if let Some(path) = meta {
        match state.save(path) {
            Ok(()) => resp["meta_saved"] = json!(true),
            Err(e) => {
                log::error!("metadata save for device {dev_id} failed: {e:#}");
                resp["meta_saved"] = json!(false);
                resp["meta_error"] = json!(format!("{e:#}"));
            }
        }
    }
    resp
}

/// Runs for the whole lifetime of one ublk device. Creating the UblkCtrl
/// here (not on the control thread) gives this device its own thread-local
/// control io_uring, keeping devices independent.
fn supervise_device(
    spec: DeviceSpec,
    back_file: std::fs::File,
    dev_size: u64,
    bs_shifts: (u8, u8),
    state: Arc<EraState>,
    ready_tx: mpsc::Sender<Result<u32>>,
) {
    let tgt = EraTgt {
        back_file_path: spec.backing.clone(),
        back_file,
        direct_io: !spec.buffered,
    };

    let ctrl = match libublk::ctrl::UblkCtrlBuilder::default()
        .name("ublkera")
        .id(spec.dev_id)
        .nr_queues(spec.queues)
        .depth(spec.depth)
        .io_buf_bytes(spec.buf_size)
        .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = ready_tx.send(Err(anyhow::anyhow!(
                "create ublk control device: {e} (root or ublk_drv missing?)"
            )));
            return;
        }
    };

    let q_state = state.clone();
    let queue_handler = move |qid: u16, dev: &_| target::queue_fn(qid, dev, q_state.clone());

    // run_target consumes the ready hook; if startup fails before the hook
    // fires, report the error through the same (still unused) sender.
    let ready_cell = Arc::new(Mutex::new(Some(ready_tx)));
    let hook_cell = ready_cell.clone();
    let device_ready = move |d_ctrl: &UblkCtrl| {
        if let Some(tx) = hook_cell.lock().unwrap().take() {
            let _ = tx.send(Ok(d_ctrl.dev_info().dev_id));
        }
    };

    let res = ctrl.run_target(
        |dev| target::init_tgt(dev, &tgt, dev_size, bs_shifts),
        queue_handler,
        device_ready,
    );
    if let Err(e) = res {
        if let Some(tx) = ready_cell.lock().unwrap().take() {
            let _ = tx.send(Err(anyhow::anyhow!("run_target failed: {e}")));
        } else {
            log::error!("device for '{}' died: {e}", spec.backing);
        }
    }

    if let Some(path) = &spec.meta {
        if let Err(e) = state.save(path) {
            log::error!("final metadata save for '{}' failed: {e:#}", spec.backing);
        }
    }
    log::info!("device for '{}' shut down", spec.backing);
}
