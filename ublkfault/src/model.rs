//! The storage contract model: an in-memory "disk" with an explicit
//! committed / volatile-cache split (in-flight requests live in the device
//! layer, which parks them before they ever reach the model).
//!
//! Semantics, chosen so that normal operation is contract-perfect and all
//! weirdness is legal volatile-cache behavior revealed only by `crash`:
//!
//! * a completed write is immediately visible to reads (committed overlaid
//!   with the volatile entries in arrival order) — read-your-writes holds;
//! * `flush` moves the whole volatile cache into committed (unless the
//!   flush-lie fault acks without doing anything);
//! * the volatile cache writes itself back oldest-first past a capacity
//!   limit, like a real disk cache — so a crash never loses "everything
//!   since boot", only the recent window;
//! * `crash` (power loss) applies a seed-chosen subset of the volatile
//!   entries to committed and discards the rest. Dropping entry N while
//!   keeping entry N+1 models persistence reordering; keeping only some
//!   logical blocks of an entry models a torn write;
//! * an errored write may still leave a partial entry in the cache (a failed
//!   write can change the medium).

use serde::{Deserialize, Serialize};

/// splitmix64; all fault decisions come from here so a run is reproducible
/// up to kernel timing.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    /// roll a per-mill probability
    pub fn hit(&mut self, pm: u32) -> bool {
        pm > 0 && self.below(1000) < pm as u64
    }
}

/// Fault policy, adjustable at runtime via the control socket.
/// All probabilities are per-mill (0..=1000).
#[derive(Clone, Copy, Default, Serialize, Deserialize)]
pub struct Policy {
    /// WRITE requests that fail with EIO (possibly leaving a partial entry)
    #[serde(default)]
    pub error_pm: u32,
    /// FLUSH requests that are acked without committing anything
    #[serde(default)]
    pub flush_lie_pm: u32,
    /// requests parked without completion — the submitter sits in D state
    /// until `thaw` (handled in the device layer, recorded here for status)
    #[serde(default)]
    pub hang_pm: u32,
}

#[derive(Default, Serialize)]
pub struct Stats {
    pub reads: u64,
    pub writes: u64,
    pub flushes: u64,
    pub discards: u64,
    pub errors_injected: u64,
    pub flush_lies: u64,
    pub hangs: u64,
    pub crashes: u64,
    pub autonomous_writebacks: u64,
}

#[derive(Serialize)]
pub struct CrashReport {
    pub committed: u64,
    pub dropped: u64,
    pub torn: u64,
}

pub struct Model {
    /// torn granularity: an entry is kept/dropped in units of this many bytes
    pub lbs: u32,
    cache_cap: u64,
    committed: Vec<u8>,
    volatile: Vec<(u64, Vec<u8>)>,
    volatile_bytes: u64,
    pub rng: Rng,
    pub policy: Policy,
    pub stats: Stats,
}

pub enum CrashMode {
    /// drop the whole volatile cache (maximum legal loss)
    Drop,
    /// per entry: commit fully / drop / tear, decided by the seed
    Seeded,
}

impl Model {
    pub fn new(size: u64, lbs: u32, cache_cap: u64, seed: u64, policy: Policy) -> Self {
        Model {
            lbs,
            cache_cap,
            committed: vec![0u8; size as usize],
            volatile: Vec::new(),
            volatile_bytes: 0,
            rng: Rng(seed),
            policy,
            stats: Stats::default(),
        }
    }

    pub fn volatile_bytes(&self) -> u64 {
        self.volatile_bytes
    }

    pub fn read(&mut self, off: u64, out: &mut [u8]) {
        self.stats.reads += 1;
        let start = off as usize;
        out.copy_from_slice(&self.committed[start..start + out.len()]);
        for (voff, data) in &self.volatile {
            overlay(out, off, *voff, data);
        }
    }

    /// Returns the byte count on success or a negative errno.
    pub fn write(&mut self, off: u64, data: &[u8]) -> i32 {
        self.stats.writes += 1;
        let error = self.rng.hit(self.policy.error_pm);
        if error {
            self.stats.errors_injected += 1;
            // the failed write may still have partially reached the cache
            let blocks = data.len() as u64 / self.lbs as u64;
            let keep = self.rng.below(blocks + 1) * self.lbs as u64;
            if keep > 0 {
                self.push_volatile(off, data[..keep as usize].to_vec());
            }
            return -libc::EIO;
        }
        self.push_volatile(off, data.to_vec());
        data.len() as i32
    }

    /// DISCARD: contract here is "reads as zeros afterwards"; it enters the
    /// volatile cache like any write, so an unflushed discard can be undone
    /// by a crash.
    pub fn discard(&mut self, off: u64, len: u64) {
        self.stats.discards += 1;
        self.push_volatile(off, vec![0u8; len as usize]);
    }

    pub fn flush(&mut self) {
        self.stats.flushes += 1;
        if self.rng.hit(self.policy.flush_lie_pm) {
            self.stats.flush_lies += 1;
            return;
        }
        let entries = std::mem::take(&mut self.volatile);
        self.volatile_bytes = 0;
        for (off, data) in entries {
            apply(&mut self.committed, off, &data);
        }
    }

    /// Power loss: a seed-chosen subset of the volatile cache survives.
    pub fn crash(&mut self, mode: CrashMode) -> CrashReport {
        self.stats.crashes += 1;
        let mut report = CrashReport { committed: 0, dropped: 0, torn: 0 };
        let entries = std::mem::take(&mut self.volatile);
        self.volatile_bytes = 0;
        for (off, data) in entries {
            match mode {
                CrashMode::Drop => report.dropped += 1,
                CrashMode::Seeded => match self.rng.below(100) {
                    0..=39 => {
                        apply(&mut self.committed, off, &data);
                        report.committed += 1;
                    }
                    40..=69 => report.dropped += 1,
                    _ => {
                        // torn: each logical block independently persists
                        for (i, block) in data.chunks(self.lbs as usize).enumerate() {
                            if self.rng.below(2) == 0 {
                                apply(
                                    &mut self.committed,
                                    off + (i * self.lbs as usize) as u64,
                                    block,
                                );
                            }
                        }
                        report.torn += 1;
                    }
                },
            }
        }
        report
    }

    fn push_volatile(&mut self, off: u64, data: Vec<u8>) {
        self.volatile_bytes += data.len() as u64;
        self.volatile.push((off, data));
        // a real disk cache drains on its own; oldest-first keeps the crash
        // window recent instead of "everything since boot"
        while self.volatile_bytes > self.cache_cap {
            let (voff, vdata) = self.volatile.remove(0);
            self.volatile_bytes -= vdata.len() as u64;
            apply(&mut self.committed, voff, &vdata);
            self.stats.autonomous_writebacks += 1;
        }
    }
}

fn apply(committed: &mut [u8], off: u64, data: &[u8]) {
    committed[off as usize..off as usize + data.len()].copy_from_slice(data);
}

/// Copy the part of `data` (device offset `doff`) that overlaps the window
/// [`woff`, `woff` + out.len()) into `out`.
fn overlay(out: &mut [u8], woff: u64, doff: u64, data: &[u8]) {
    let wend = woff + out.len() as u64;
    let dend = doff + data.len() as u64;
    let start = woff.max(doff);
    let end = wend.min(dend);
    if start >= end {
        return;
    }
    out[(start - woff) as usize..(end - woff) as usize]
        .copy_from_slice(&data[(start - doff) as usize..(end - doff) as usize]);
}

#[cfg(test)]
mod tests {
    use super::*;

    const LBS: u32 = 4096;

    fn model(seed: u64, policy: Policy) -> Model {
        Model::new(1 << 20, LBS, 1 << 19, seed, policy) // 1MiB dev, 512K cache
    }

    fn read_at(m: &mut Model, off: u64, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        m.read(off, &mut buf);
        buf
    }

    #[test]
    fn read_your_writes_before_flush() {
        let mut m = model(1, Policy::default());
        m.write(8192, &[7u8; 4096]);
        assert_eq!(read_at(&mut m, 8192, 4096), vec![7u8; 4096]);
        // overlapping later write wins
        m.write(8192, &[9u8; 4096]);
        assert_eq!(read_at(&mut m, 8192, 4096), vec![9u8; 4096]);
    }

    #[test]
    fn unflushed_writes_may_vanish_on_crash_flushed_may_not() {
        let mut m = model(1, Policy::default());
        m.write(0, &[1u8; 4096]);
        m.flush();
        m.write(4096, &[2u8; 4096]);
        m.crash(CrashMode::Drop);
        assert_eq!(read_at(&mut m, 0, 4096), vec![1u8; 4096], "flushed must survive");
        assert_eq!(read_at(&mut m, 4096, 4096), vec![0u8; 4096], "unflushed dropped");
    }

    #[test]
    fn flush_lie_defeats_durability() {
        let mut m = model(
            1,
            Policy { flush_lie_pm: 1000, ..Policy::default() },
        );
        m.write(0, &[1u8; 4096]);
        m.flush(); // lie: acked, nothing committed
        assert_eq!(m.stats.flush_lies, 1);
        m.crash(CrashMode::Drop);
        assert_eq!(read_at(&mut m, 0, 4096), vec![0u8; 4096]);
    }

    #[test]
    fn seeded_crash_is_deterministic_and_tears_on_block_boundaries() {
        let run = |seed| {
            let mut m = model(seed, Policy::default());
            for i in 0..32u64 {
                let fill = (i + 1) as u8;
                m.write(i * 8192, &vec![fill; 8192]); // 2 blocks per entry
            }
            let _ = m.crash(CrashMode::Seeded);
            read_at(&mut m, 0, 1 << 20)
        };
        assert_eq!(run(42), run(42), "same seed, same post-crash state");
        assert_ne!(run(42), run(43), "different seed, different state");

        // every logical block is either fully old (0) or fully new
        let img = run(42);
        for (b, block) in img.chunks(LBS as usize).enumerate() {
            let uniform = block.iter().all(|&x| x == block[0]);
            assert!(uniform, "block {b} is torn inside the logical block");
        }
    }

    #[test]
    fn errored_write_may_partially_persist() {
        let mut m = model(
            7,
            Policy { error_pm: 1000, ..Policy::default() },
        );
        let res = m.write(0, &[5u8; 32768]);
        assert_eq!(res, -libc::EIO);
        m.flush();
        // whatever partial prefix entered the cache is now committed; each
        // block is uniformly old or new
        let img = read_at(&mut m, 0, 32768);
        for block in img.chunks(LBS as usize) {
            assert!(block.iter().all(|&x| x == block[0]));
        }
    }

    #[test]
    fn cache_cap_forces_autonomous_writeback() {
        let mut m = model(1, Policy::default());
        // 768K written into a 512K cache: the oldest ~256K must self-commit
        for i in 0..6u64 {
            m.write(i * (1 << 17), &vec![(i + 1) as u8; 1 << 17]);
        }
        assert!(m.stats.autonomous_writebacks > 0);
        m.crash(CrashMode::Drop);
        assert_eq!(
            read_at(&mut m, 0, 1 << 17),
            vec![1u8; 1 << 17],
            "oldest write was written back before the crash"
        );
    }

    #[test]
    fn discard_reads_zero_but_can_be_undone_by_crash() {
        let mut m = model(1, Policy::default());
        m.write(0, &[3u8; 8192]);
        m.flush();
        m.discard(0, 8192);
        assert_eq!(read_at(&mut m, 0, 8192), vec![0u8; 8192]);
        m.crash(CrashMode::Drop); // discard was volatile
        assert_eq!(read_at(&mut m, 0, 8192), vec![3u8; 8192]);
    }
}
