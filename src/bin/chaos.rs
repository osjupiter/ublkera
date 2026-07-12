//! Seeded chaos harness for ublkera: drives a real device through random
//! writes, checkpoints, crashes (SIGKILL), re-attaches and metadata loss,
//! while simulating an incremental-backup consumer. Two invariants are
//! checked after every backup:
//!
//!   1. passthrough integrity — the device reads back exactly what was
//!      written to it (`actual == expected`);
//!   2. the CBT contract — a consumer that copies only the ranges reported
//!      by `dump --since <cursor>` (falling back to a full copy whenever the
//!      cursor is rejected) ends up with an identical image
//!      (`shadow == actual`). A single silently-missing chunk fails this.
//!
//! Everything is derived from --seed, so a failure reproduces by rerunning
//! with the same seed against the same binaries. The IO itself goes through
//! the kernel (ublk), so timing is not deterministic — the *operation
//! sequence* is.
//!
//! Data-path stress (fio --verify), conformance (blktests) and backing-error
//! injection (dm-flakey) are out of scope here; see docs/testing.md.

use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use serde_json::Value;
use std::os::unix::fs::{FileExt, FileTypeExt, OpenOptionsExt};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(about = "seeded chaos test for a ublkera device")]
struct Cli {
    /// ublkera binary to drive
    #[arg(long, default_value = "ublkera")]
    ublkera: PathBuf,
    /// working directory (backing image, metadata, socket, daemon log)
    #[arg(long)]
    dir: PathBuf,
    /// PRNG seed; the whole episode derives from it
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// number of chaos operations
    #[arg(long, default_value_t = 200)]
    ops: u32,
    /// device size in bytes (0 = derive a 32MiB-ish size from the seed)
    #[arg(long, default_value_t = 0)]
    size: u64,
    /// tracking granularity in bytes (0 = derive from the seed)
    #[arg(long, default_value_t = 0)]
    granularity: u64,
    /// pass --buffered to `add` (needed when the backing file sits on a
    /// filesystem without O_DIRECT, e.g. the initramfs rootfs)
    #[arg(long)]
    buffered: bool,
}

/// splitmix64: tiny, seedable, good enough for test-case generation.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let b = self.next().to_le_bytes();
            chunk.copy_from_slice(&b[..chunk.len()]);
        }
    }
}

/// 4096-aligned buffer for O_DIRECT IO on the ublk device.
struct DioBuf {
    store: Vec<u8>,
    off: usize,
    len: usize,
}

impl DioBuf {
    fn new(len: usize) -> Self {
        let store = vec![0u8; len + 4096];
        let off = store.as_ptr().align_offset(4096);
        DioBuf { store, off, len }
    }
    fn get(&self) -> &[u8] {
        &self.store[self.off..self.off + self.len]
    }
    fn get_mut(&mut self) -> &mut [u8] {
        let (off, len) = (self.off, self.len);
        &mut self.store[off..off + len]
    }
}

#[derive(Default)]
struct Counters {
    writes: u64,
    bursts: u64,
    backups: u64,
    full_copies: u64,
    checkpoints: u64,
    reattaches: u64,
    crashes: u64,
    inflight_crashes: u64,
    interrupted: u64,
    meta_losses: u64,
}

struct Harness {
    ublkera: PathBuf,
    socket: String,
    backing: PathBuf,
    meta: PathBuf,
    log_path: PathBuf,
    size: u64,
    gran: u64,
    queues: u16,
    buffered: bool,

    daemon: Option<Child>,
    dev_id: Option<u64>,
    dev: Option<std::fs::File>,
    generation: String,

    /// ground truth: what the device must contain (updated on every write)
    expected: Vec<u8>,
    /// the simulated consumer's backup image (updated only from dumps)
    shadow: Vec<u8>,
    /// the consumer's cursor: (generation, closed era) of its last backup
    cursor: Option<(String, u32)>,

    journal: Vec<String>,
    n: Counters,
}

const MAX_WRITE: u64 = 256 * 1024;

fn wait_until<F: FnMut() -> bool>(what: &str, timeout: Duration, mut f: F) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("timed out after {timeout:?} waiting for {what}");
}

impl Harness {
    fn note(&mut self, msg: String) {
        self.journal.push(msg);
    }

    fn ctl(&self, args: &[&str]) -> Result<Value> {
        let out = Command::new(&self.ublkera)
            .arg("--socket")
            .arg(&self.socket)
            .args(args)
            .output()
            .with_context(|| format!("spawn {}", self.ublkera.display()))?;
        if !out.status.success() {
            bail!(
                "ublkera {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        serde_json::from_slice(&out.stdout)
            .with_context(|| format!("parse JSON from `ublkera {}`", args.join(" ")))
    }

    fn start_daemon(&mut self) -> Result<()> {
        let _ = std::fs::remove_file(&self.socket);
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        let child = Command::new(&self.ublkera)
            .arg("--socket")
            .arg(&self.socket)
            .args(["daemon", "--foreground"])
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .context("spawn daemon")?;
        self.daemon = Some(child);
        let sock = self.socket.clone();
        wait_until("daemon socket", Duration::from_secs(10), || {
            std::os::unix::net::UnixStream::connect(&sock).is_ok()
        })
    }

    fn kill_daemon(&mut self) -> Result<()> {
        self.dev = None;
        let mut child = self.daemon.take().context("no daemon to kill")?;
        unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
        child.wait()?;
        self.wait_bdev_gone()
    }

    fn bdev(&self) -> Result<String> {
        Ok(format!("/dev/ublkb{}", self.dev_id.context("not attached")?))
    }

    fn wait_bdev_gone(&mut self) -> Result<()> {
        if let Some(id) = self.dev_id.take() {
            let path = format!("/dev/ublkb{id}");
            wait_until(&format!("{path} to disappear"), Duration::from_secs(15), || {
                !std::path::Path::new(&path).exists()
            })?;
        }
        Ok(())
    }

    /// Attach the backing file and open the resulting /dev/ublkbN O_DIRECT.
    /// Returns the raw `add` response for the caller's assertions.
    fn attach(&mut self) -> Result<Value> {
        let gran = self.gran.to_string();
        let queues = self.queues.to_string();
        let backing = self.backing.display().to_string();
        let meta = self.meta.display().to_string();
        let mut args = vec![
            "add", "-f", &backing, "-g", &gran, "-q", &queues, "--meta", &meta,
        ];
        if self.buffered {
            args.push("--buffered");
        }
        let v = self.ctl(&args)?;
        let id = v["dev_id"].as_u64().context("add: no dev_id")?;
        self.dev_id = Some(id);
        self.generation = v["generation"]
            .as_str()
            .context("add: no generation")?
            .to_string();
        let path = self.bdev()?;
        wait_until(&format!("{path} block node"), Duration::from_secs(15), || {
            std::fs::metadata(&path)
                .map(|m| m.file_type().is_block_device())
                .unwrap_or(false)
        })?;
        self.dev = Some(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_DIRECT)
                .open(&path)
                .with_context(|| format!("open {path} O_DIRECT"))?,
        );
        Ok(v)
    }

    fn detach(&mut self) -> Result<()> {
        self.dev = None;
        let id = self.dev_id.context("not attached")?;
        self.ctl(&["del", "-n", &id.to_string()])?;
        self.wait_bdev_gone()
    }

    fn read_device(&self) -> Result<Vec<u8>> {
        let dev = self.dev.as_ref().context("device not open")?;
        let mut out = vec![0u8; self.size as usize];
        let mut buf = DioBuf::new(1 << 20);
        let mut pos = 0usize;
        while pos < out.len() {
            let n = (out.len() - pos).min(1 << 20);
            dev.read_exact_at(&mut buf.get_mut()[..n], pos as u64)?;
            out[pos..pos + n].copy_from_slice(&buf.get()[..n]);
            pos += n;
        }
        Ok(out)
    }

    /// A burst of 1..=24 random-content writes at 4K-aligned offsets.
    fn op_write_burst(&mut self, rng: &mut Rng) -> Result<()> {
        let count = 1 + rng.below(24);
        let mut buf = DioBuf::new(MAX_WRITE as usize);
        for _ in 0..count {
            let blocks = self.size / 4096;
            let off = 4096 * rng.below(blocks);
            let max_len = MAX_WRITE.min(self.size - off);
            let len = (4096 * (1 + rng.below(max_len / 4096))) as usize;
            rng.fill(&mut buf.get_mut()[..len]);
            let dev = self.dev.as_ref().context("device not open")?;
            dev.write_all_at(&buf.get()[..len], off)
                .with_context(|| format!("write {len}B at {off}"))?;
            self.expected[off as usize..off as usize + len].copy_from_slice(&buf.get()[..len]);
            self.n.writes += 1;
        }
        self.n.bursts += 1;
        self.note(format!("write burst: {count} writes"));
        Ok(())
    }

    /// Checkpoint without a backup: widens the era gap the next dump spans.
    fn op_checkpoint(&mut self) -> Result<()> {
        let id = self.dev_id.context("not attached")?.to_string();
        let v = self.ctl(&["checkpoint", "-n", &id])?;
        self.n.checkpoints += 1;
        self.note(format!("checkpoint: closed era {}", v["closed_era"]));
        Ok(())
    }

    /// The consumer's incremental backup, then both invariant checks.
    fn op_backup_verify(&mut self) -> Result<()> {
        let id = self.dev_id.context("not attached")?.to_string();
        let v = self.ctl(&["checkpoint", "-n", &id])?;
        let closed = v["closed_era"].as_u64().context("no closed_era")? as u32;
        self.n.checkpoints += 1;

        let mut incremental = false;
        if let Some((gen, era)) = self.cursor.clone() {
            let since = era.to_string();
            match self.ctl(&["dump", "-n", &id, "--since", &since, "--generation", &gen]) {
                Ok(d) => {
                    let ranges = d["ranges"].as_array().context("dump: no ranges")?;
                    let mut copied = 0u64;
                    let mut buf = DioBuf::new(1 << 20);
                    for r in ranges {
                        let off = r["offset"].as_u64().context("range offset")?;
                        let len = r["len"].as_u64().context("range len")?;
                        let dev = self.dev.as_ref().context("device not open")?;
                        let mut pos = 0u64;
                        while pos < len {
                            let n = ((len - pos).min(1 << 20)) as usize;
                            dev.read_exact_at(&mut buf.get_mut()[..n], off + pos)?;
                            let dst = (off + pos) as usize;
                            self.shadow[dst..dst + n].copy_from_slice(&buf.get()[..n]);
                            pos += n as u64;
                        }
                        copied += len;
                    }
                    incremental = true;
                    self.note(format!(
                        "backup: incremental since ({gen}, {era}): {} ranges, {copied}B",
                        ranges.len()
                    ));
                }
                Err(e) => self.note(format!("backup: cursor rejected ({e:#}), full copy")),
            }
        }
        if !incremental {
            self.shadow = self.read_device()?;
            self.n.full_copies += 1;
            if self.journal.last().map_or(true, |l| !l.starts_with("backup:")) {
                self.note("backup: full copy (no cursor)".to_string());
            }
        }
        self.cursor = Some((self.generation.clone(), closed));
        self.n.backups += 1;
        self.verify()
    }

    /// Both invariants against a fresh read of the whole device.
    fn verify(&self) -> Result<()> {
        let actual = self.read_device()?;
        self.diff("passthrough (device vs expected)", &actual, &self.expected)?;
        self.diff("CBT (backup image vs device)", &actual, &self.shadow)?;
        Ok(())
    }

    fn diff(&self, what: &str, actual: &[u8], model: &[u8]) -> Result<()> {
        if actual == model {
            return Ok(());
        }
        let first = actual
            .iter()
            .zip(model.iter())
            .position(|(a, b)| a != b)
            .unwrap();
        let bad_chunks = (0..self.size)
            .step_by(self.gran as usize)
            .filter(|&off| {
                let end = (off + self.gran).min(self.size) as usize;
                actual[off as usize..end] != model[off as usize..end]
            })
            .count();
        bail!(
            "{what} MISMATCH: first differing byte at {first} \
             (chunk {}), {bad_chunks} of {} chunks differ",
            first as u64 / self.gran,
            self.size.div_ceil(self.gran)
        );
    }

    /// Graceful detach + re-attach: metadata must load clean, same history.
    fn op_reattach(&mut self) -> Result<()> {
        let old_gen = self.generation.clone();
        self.detach()?;
        let v = self.attach()?;
        ensure!(
            v["recovered_unclean"].as_bool() != Some(true),
            "clean detach was flagged recovered_unclean"
        );
        ensure!(
            self.generation == old_gen,
            "generation changed across clean re-attach ({old_gen} -> {})",
            self.generation
        );
        self.n.reattaches += 1;
        self.note("reattach: clean".to_string());
        Ok(())
    }

    /// SIGKILL the daemon mid-life, restart, re-attach: recovery must be
    /// flagged and the history (generation) preserved. The consumer's next
    /// backup then sees an everything-changed diff and stays correct.
    fn op_crash(&mut self) -> Result<()> {
        let old_gen = self.generation.clone();
        self.kill_daemon()?;
        self.start_daemon()?;
        let v = self.attach()?;
        ensure!(
            v["recovered_unclean"].as_bool() == Some(true),
            "crash recovery not flagged recovered_unclean"
        );
        ensure!(
            self.generation == old_gen,
            "generation changed across crash recovery ({old_gen} -> {})",
            self.generation
        );
        self.n.crashes += 1;
        self.note("crash: SIGKILL + recover (unclean flagged)".to_string());
        Ok(())
    }

    /// SIGKILL the daemon while writer threads have IO in flight — the data-
    /// path torn-write case. Contract being checked:
    ///
    /// * a write whose completion the application saw must survive the crash
    ///   byte-for-byte (the daemon completes to ublk only after the backing
    ///   store accepted the data);
    /// * a write still in flight may land fully, partially (torn) or not at
    ///   all — we adopt whatever is on the device for those ranges — and the
    ///   post-crash full-dirty fallback keeps the consumer's backup correct
    ///   regardless (checked by the next op_backup_verify).
    ///
    /// Each writer owns a disjoint slice of the device so successful writes
    /// can be replayed into `expected` in a known order.
    fn op_crash_inflight(&mut self, rng: &mut Rng) -> Result<()> {
        const WRITERS: u64 = 4;
        let slice = (self.size / WRITERS / 4096) * 4096;
        if slice == 0 {
            return self.op_crash(); // device too small to split; plain crash
        }
        let old_gen = self.generation.clone();
        let path = self.bdev()?;
        let stop = Arc::new(AtomicBool::new(false));

        type WriterLog = (Vec<(u64, Vec<u8>)>, Vec<(u64, u64)>); // (ok, undefined)
        let mut handles = Vec::new();
        for t in 0..WRITERS {
            let base = t * slice;
            let seed = rng.next();
            let path = path.clone();
            let stop = stop.clone();
            handles.push(std::thread::spawn(move || -> WriterLog {
                let mut rng = Rng(seed);
                let mut ok = Vec::new();
                let mut undefined = Vec::new();
                let dev = match std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_DIRECT)
                    .open(&path)
                {
                    Ok(d) => d,
                    Err(_) => return (ok, undefined), // device already gone
                };
                let mut buf = DioBuf::new(MAX_WRITE as usize);
                while !stop.load(Ordering::Acquire) {
                    let off = base + 4096 * rng.below(slice / 4096);
                    let max_len = MAX_WRITE.min(base + slice - off);
                    let len = (4096 * (1 + rng.below(max_len / 4096))) as usize;
                    rng.fill(&mut buf.get_mut()[..len]);
                    match dev.write_all_at(&buf.get()[..len], off) {
                        Ok(()) => ok.push((off, buf.get()[..len].to_vec())),
                        Err(_) => {
                            // the write the kill interrupted: content undefined
                            undefined.push((off, len as u64));
                            break;
                        }
                    }
                }
                (ok, undefined)
            }));
        }

        // let IO build up, then murder the daemon mid-flight
        std::thread::sleep(Duration::from_millis(50 + rng.below(250)));
        self.kill_daemon()?;
        stop.store(true, Ordering::Release);

        let mut completed = 0u64;
        let mut undefined = Vec::new();
        for h in handles {
            let (ok, undef) = h.join().map_err(|_| anyhow::anyhow!("writer panicked"))?;
            for (off, data) in &ok {
                self.expected[*off as usize..*off as usize + data.len()].copy_from_slice(data);
            }
            completed += ok.len() as u64;
            undefined.extend(undef);
        }
        self.n.writes += completed;

        self.start_daemon()?;
        let v = self.attach()?;
        ensure!(
            v["recovered_unclean"].as_bool() == Some(true),
            "in-flight crash recovery not flagged recovered_unclean"
        );
        ensure!(
            self.generation == old_gen,
            "generation changed across in-flight crash ({old_gen} -> {})",
            self.generation
        );

        // in-flight ranges: old, new or torn are all legal — adopt reality
        let dev = self.dev.as_ref().context("device not open")?;
        for (off, len) in &undefined {
            let mut buf = DioBuf::new(*len as usize);
            dev.read_exact_at(buf.get_mut(), *off)?;
            self.expected[*off as usize..(*off + *len) as usize].copy_from_slice(buf.get());
        }
        // everything the application saw complete must have survived
        let actual = self.read_device()?;
        self.diff("completed writes across in-flight SIGKILL", &actual, &self.expected)?;

        self.n.inflight_crashes += 1;
        self.n.interrupted += undefined.len() as u64;
        self.note(format!(
            "in-flight crash: {completed} completed writes survived, {} interrupted",
            undefined.len()
        ));
        Ok(())
    }

    /// Delete the metadata while detached: a fresh history must begin and the
    /// consumer's old cursor must be rejected (it then does a full copy).
    fn op_lose_meta(&mut self) -> Result<()> {
        let old_gen = self.generation.clone();
        self.detach()?;
        std::fs::remove_file(&self.meta).context("remove metadata")?;
        let v = self.attach()?;
        ensure!(
            v["recovered_unclean"].as_bool() != Some(true),
            "fresh attach after metadata loss was flagged recovered_unclean"
        );
        ensure!(
            self.generation != old_gen,
            "generation must change when metadata is lost"
        );
        if let Some((gen, era)) = self.cursor.clone() {
            let id = self.dev_id.context("not attached")?.to_string();
            let since = era.to_string();
            let r = self.ctl(&["dump", "-n", &id, "--since", &since, "--generation", &gen]);
            ensure!(
                r.is_err(),
                "cursor from the lost history was accepted by dump"
            );
        }
        self.n.meta_losses += 1;
        self.note("meta loss: fresh generation, old cursor rejected".to_string());
        Ok(())
    }
}

fn setup(cli: &Cli, rng: &mut Rng) -> Result<Harness> {
    let gran = if cli.granularity != 0 {
        cli.granularity
    } else {
        [4096, 16384, 65536, 262144][rng.below(4) as usize]
    };
    // A deliberately odd size (not a granularity multiple) keeps the partial
    // tail chunk in play.
    let size = if cli.size != 0 {
        cli.size
    } else {
        32 * (1 << 20) + 4096 * rng.below(16)
    };
    ensure!(size % 4096 == 0, "--size must be a multiple of 4096");
    let queues = [1u16, 2, 4][rng.below(3) as usize];
    println!(
        "CHAOS: seed={} ops={} size={size} granularity={gran} queues={queues} buffered={}",
        cli.seed, cli.ops, cli.buffered
    );

    std::fs::create_dir_all(&cli.dir)?;
    let backing = cli.dir.join("backing.img");
    std::fs::File::create(&backing)?.set_len(size)?;

    Ok(Harness {
        ublkera: cli.ublkera.clone(),
        socket: cli.dir.join("daemon.sock").display().to_string(),
        backing,
        meta: cli.dir.join("meta.bin"),
        log_path: cli.dir.join("daemon.log"),
        size,
        gran,
        queues,
        buffered: cli.buffered,
        daemon: None,
        dev_id: None,
        dev: None,
        generation: String::new(),
        expected: vec![0u8; size as usize],
        shadow: vec![0u8; size as usize],
        cursor: None,
        journal: Vec::new(),
        n: Counters::default(),
    })
}

fn run(cli: &Cli, h: &mut Harness, rng: &mut Rng) -> Result<()> {
    h.start_daemon()?;
    h.attach()?;

    for i in 0..cli.ops {
        let step = match rng.below(100) {
            0..=44 => h.op_write_burst(rng),
            45..=59 => h.op_backup_verify(),
            60..=69 => h.op_checkpoint(),
            70..=79 => h.op_reattach(),
            80..=87 => h.op_crash(),
            88..=95 => h.op_crash_inflight(rng),
            _ => h.op_lose_meta(),
        };
        step.with_context(|| format!("op {}/{} failed", i + 1, cli.ops))?;
        if (i + 1) % 50 == 0 {
            println!(
                "CHAOS: op {}/{} (writes={} backups={} crashes={} reattaches={} meta_losses={})",
                i + 1, cli.ops, h.n.writes, h.n.backups, h.n.crashes,
                h.n.reattaches, h.n.meta_losses
            );
        }
    }

    // Wind down: one last verified backup, then the detached image itself
    // must equal the model (passthrough persisted everything), then a final
    // clean re-attach and daemon shutdown.
    h.op_backup_verify().context("final backup")?;
    h.detach()?;
    let on_disk = std::fs::read(&h.backing)?;
    ensure!(
        on_disk == h.expected,
        "backing file after detach differs from everything written"
    );
    let v = h.attach()?;
    ensure!(
        v["recovered_unclean"].as_bool() != Some(true),
        "final re-attach was flagged recovered_unclean"
    );
    h.detach()?;
    h.ctl(&["shutdown"])?;
    if let Some(mut d) = h.daemon.take() {
        wait_until("daemon exit", Duration::from_secs(10), || {
            d.try_wait().map(|s| s.is_some()).unwrap_or(true)
        })?;
    }
    Ok(())
}

fn main() {
    let cli = Cli::parse();
    let mut rng = Rng(cli.seed);
    let mut h = match setup(&cli, &mut rng) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("CHAOS-FAIL seed={}: setup: {e:#}", cli.seed);
            std::process::exit(1);
        }
    };
    match run(&cli, &mut h, &mut rng) {
        Ok(()) => {
            let n = &h.n;
            println!(
                "CHAOS-PASS seed={}: {} writes in {} bursts, {} backups \
                 ({} full), {} checkpoints, {} reattaches, {} crashes \
                 (+{} in-flight, {} writes interrupted), {} meta losses",
                cli.seed, n.writes, n.bursts, n.backups, n.full_copies,
                n.checkpoints, n.reattaches, n.crashes, n.inflight_crashes,
                n.interrupted, n.meta_losses
            );
        }
        Err(e) => {
            eprintln!("CHAOS-FAIL seed={}: {e:#}", cli.seed);
            eprintln!("last operations before the failure:");
            let tail = h.journal.len().saturating_sub(20);
            for line in &h.journal[tail..] {
                eprintln!("  {line}");
            }
            if let Some(mut d) = h.daemon.take() {
                let _ = d.kill();
                let _ = d.wait();
            }
            std::process::exit(1);
        }
    }
}
