//! dm-era-like change tracking: the device is divided into fixed-size chunks
//! ("granularity"); each chunk records the era in which it was last written.
//! A checkpoint bumps the current era, so "changed since checkpoint N" is
//! simply "chunks whose era > N".

use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

const META_MAGIC: &[u8; 8] = b"UBLKERA1";
const META_VERSION: u32 = 4;
/// header: magic8 + version4 + granularity8 + dev_size8 + era4 + chunks8 +
/// clean4 + generation8 + crc4, then chunks*4 bytes of per-chunk eras
const HDR_LEN: usize = 56;
/// CRC32 over the whole file with this field zeroed. Catches external bit rot
/// and truncation of the payload — a crash cannot corrupt the file (save
/// replaces it atomically via rename), but disks and copy mistakes can, and a
/// corrupted era array would silently under-report changes.
const CRC_OFF: usize = 52;

/// CRC32 (IEEE, reflected) over the concatenation of `parts`; table-driven,
/// no dependency.
fn crc32(parts: &[&[u8]]) -> u32 {
    let mut table = [0u32; 256];
    for (i, e) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *e = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for part in parts {
        for &b in *part {
            crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
        }
    }
    !crc
}

pub struct EraState {
    pub granularity: u64,
    pub dev_size: u64,
    /// Identity of this tracking history. Era numbers are small naturals that
    /// restart at 1 whenever tracking starts fresh, so a bare era cursor can
    /// collide with a *different* history and silently mean the wrong thing.
    /// The generation is random per history and survives save/load, so a
    /// cursor is safe when carried as (generation, era).
    pub generation: u64,
    current_era: AtomicU32,
    eras: Vec<AtomicU32>,
}

/// A contiguous byte range of the device that changed.
#[derive(serde::Serialize)]
pub struct DirtyRange {
    pub offset: u64,
    pub len: u64,
}

impl EraState {
    pub fn new(dev_size: u64, granularity: u64) -> Result<Self> {
        if !granularity.is_power_of_two() || granularity < 4096 {
            bail!("granularity must be a power of two >= 4096 (got {granularity})");
        }
        let nr_chunks = dev_size.div_ceil(granularity);
        let mut eras = Vec::with_capacity(nr_chunks as usize);
        eras.resize_with(nr_chunks as usize, || AtomicU32::new(0));
        let mut gen = [0u8; 8];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut gen))
            .context("read /dev/urandom for generation id")?;
        Ok(EraState {
            granularity,
            dev_size,
            generation: u64::from_le_bytes(gen),
            current_era: AtomicU32::new(1),
            eras,
        })
    }

    pub fn nr_chunks(&self) -> u64 {
        self.eras.len() as u64
    }

    pub fn current_era(&self) -> u32 {
        self.current_era.load(Ordering::Acquire)
    }

    /// Record that [offset, offset+len) was written, stamping the chunks with
    /// the era current at (write) completion time. fetch_max keeps the newest
    /// era if a concurrent checkpoint or write races with us.
    ///
    /// If a checkpoint advances the era *while* we are stamping, we re-stamp
    /// with the newer era (the retry below). This closes the otherwise-possible
    /// window where a write that races a checkpoint gets stamped with the
    /// just-closed era and is then skipped by a concurrent `dump --since`: the
    /// re-stamp guarantees that when this call returns, the chunks carry an era
    /// >= the newest era that any checkpoint completed before we returned. So a
    /// racing (still in-flight) write is at worst pushed into the *next* diff,
    /// never lost from both. See docs/concurrency.md §3.
    pub fn mark_write(&self, offset: u64, len: u64) {
        if len == 0 {
            return;
        }
        let first = (offset / self.granularity) as usize;
        if first >= self.eras.len() {
            return; // beyond device end; ublk never sends this
        }
        let last = ((offset + len - 1) / self.granularity) as usize;
        let chunks = &self.eras[first..=last.min(self.eras.len() - 1)];
        loop {
            let era = self.current_era.load(Ordering::Acquire);
            for chunk in chunks {
                chunk.fetch_max(era, Ordering::AcqRel);
            }
            // No checkpoint slipped in between the two loads (current_era only
            // ever increases), so `era` was current throughout the stamp.
            if self.current_era.load(Ordering::Acquire) == era {
                break;
            }
        }
    }

    /// Close the current era and start a new one.
    /// Returns (closed_era, new_current_era).
    pub fn checkpoint(&self) -> (u32, u32) {
        let old = self.current_era.fetch_add(1, Ordering::AcqRel);
        (old, old + 1)
    }

    pub fn written_chunks(&self) -> u64 {
        self.eras
            .iter()
            .filter(|e| e.load(Ordering::Relaxed) > 0)
            .count() as u64
    }

    /// Chunks written in an era newer than `since`, merged into contiguous
    /// byte ranges. `since = 0` returns everything ever written.
    pub fn ranges_since(&self, since: u32) -> Vec<DirtyRange> {
        let mut ranges: Vec<DirtyRange> = Vec::new();
        for (idx, e) in self.eras.iter().enumerate() {
            if e.load(Ordering::Acquire) <= since {
                continue;
            }
            let offset = idx as u64 * self.granularity;
            let len = self.granularity.min(self.dev_size - offset);
            match ranges.last_mut() {
                Some(r) if r.offset + r.len == offset => r.len += len,
                _ => ranges.push(DirtyRange { offset, len }),
            }
        }
        ranges
    }

    /// Treat every chunk as written in the current era. Used when metadata
    /// turns out to be stale (unclean shutdown): any `dump --since <older era>`
    /// then reports the whole device, so the consumer's next "incremental"
    /// backup is automatically a full copy.
    pub fn mark_all_dirty(&self) {
        let era = self.current_era();
        for chunk in &self.eras {
            chunk.fetch_max(era, Ordering::AcqRel);
        }
    }

    /// Persist to `path` atomically (write temp file + rename). `clean` records
    /// whether this snapshot is complete: false while the device is attached
    /// (writes keep landing after the save), true only on a final save when no
    /// more writes can happen. A crash leaves the last save marked unclean,
    /// which `load` turns into "everything changed".
    pub fn save(&self, path: &Path, clean: bool) -> Result<()> {
        let tmp = path.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp)
                .with_context(|| format!("create {}", tmp.display()))?;
            let mut buf = Vec::with_capacity(HDR_LEN + self.eras.len() * 4);
            buf.extend_from_slice(META_MAGIC);
            buf.extend_from_slice(&META_VERSION.to_le_bytes());
            buf.extend_from_slice(&self.granularity.to_le_bytes());
            buf.extend_from_slice(&self.dev_size.to_le_bytes());
            buf.extend_from_slice(&self.current_era().to_le_bytes());
            buf.extend_from_slice(&self.nr_chunks().to_le_bytes());
            buf.extend_from_slice(&(clean as u32).to_le_bytes());
            buf.extend_from_slice(&self.generation.to_le_bytes());
            buf.extend_from_slice(&0u32.to_le_bytes()); // crc, filled in below
            for e in &self.eras {
                buf.extend_from_slice(&e.load(Ordering::Acquire).to_le_bytes());
            }
            let crc = crc32(&[&buf]);
            buf[CRC_OFF..CRC_OFF + 4].copy_from_slice(&crc.to_le_bytes());
            f.write_all(&buf)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Load a previously saved state; granularity and device size must match.
    /// Returns the state and whether the save was clean (complete).
    /// Any corruption (bit rot, truncation, trailing garbage) fails the CRC
    /// and is a hard error: metadata is disposable, so the caller's remedy is
    /// to delete the file, never to trust a damaged era map.
    pub fn load(path: &Path, dev_size: u64, granularity: u64) -> Result<(Self, bool)> {
        let raw = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        if raw.len() < HDR_LEN {
            bail!("{}: truncated metadata file", path.display());
        }
        if &raw[0..8] != META_MAGIC {
            bail!("{}: not a ublkera metadata file", path.display());
        }
        let version = u32::from_le_bytes(raw[8..12].try_into().unwrap());
        if version != META_VERSION {
            bail!("{}: unsupported metadata version {version}", path.display());
        }
        // Integrity before meaning: everything below trusts these bytes.
        let stored = u32::from_le_bytes(raw[CRC_OFF..CRC_OFF + 4].try_into().unwrap());
        let mut hdr = raw[..HDR_LEN].to_vec();
        hdr[CRC_OFF..CRC_OFF + 4].fill(0);
        if stored != crc32(&[&hdr, &raw[HDR_LEN..]]) {
            bail!(
                "{}: checksum mismatch (corrupted metadata); delete it to start a \
                 fresh history (consumers then fall back to a full copy)",
                path.display()
            );
        }
        let m_gran = u64::from_le_bytes(raw[12..20].try_into().unwrap());
        let m_size = u64::from_le_bytes(raw[20..28].try_into().unwrap());
        let m_era = u32::from_le_bytes(raw[28..32].try_into().unwrap());
        let m_chunks = u64::from_le_bytes(raw[32..40].try_into().unwrap());
        let m_clean = u32::from_le_bytes(raw[40..44].try_into().unwrap()) != 0;
        let m_gen = u64::from_le_bytes(raw[44..52].try_into().unwrap());
        if m_gran != granularity {
            bail!(
                "metadata granularity {m_gran} does not match requested {granularity}; \
                 delete {} to start fresh",
                path.display()
            );
        }
        if m_size != dev_size {
            bail!(
                "metadata device size {m_size} does not match backing size {dev_size}; \
                 delete {} to start fresh",
                path.display()
            );
        }
        let mut state = EraState::new(dev_size, granularity)?;
        state.generation = m_gen;
        if m_chunks != state.nr_chunks() {
            bail!("metadata chunk count mismatch");
        }
        let body = &raw[HDR_LEN..];
        if body.len() != state.eras.len() * 4 {
            bail!("metadata body length mismatch");
        }
        for (i, e) in state.eras.iter().enumerate() {
            let v = u32::from_le_bytes(body[i * 4..i * 4 + 4].try_into().unwrap());
            e.store(v, Ordering::Relaxed);
        }
        state.current_era.store(m_era, Ordering::Release);
        Ok((state, m_clean))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_and_query() {
        let s = EraState::new(1 << 20, 65536).unwrap(); // 16 chunks
        assert_eq!(s.nr_chunks(), 16);
        s.mark_write(0, 100); // chunk 0
        s.mark_write(65536 * 2 + 10, 65536); // chunks 2,3
        let r = s.ranges_since(0);
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].offset, r[0].len), (0, 65536));
        assert_eq!((r[1].offset, r[1].len), (65536 * 2, 65536 * 2));

        let (closed, now) = s.checkpoint();
        assert_eq!((closed, now), (1, 2));
        assert!(s.ranges_since(closed).is_empty());
        s.mark_write(65536 * 5, 1);
        let r = s.ranges_since(closed);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].offset, 65536 * 5);
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ublkera-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("meta.bin");

        let s = EraState::new(1 << 20, 65536).unwrap();
        s.mark_write(65536 * 7, 4096);
        s.checkpoint();
        s.save(&path, true).unwrap();

        let (l, clean) = EraState::load(&path, 1 << 20, 65536).unwrap();
        assert!(clean);
        assert_eq!(l.current_era(), 2);
        let r = l.ranges_since(0);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].offset, 65536 * 7);
        assert!(EraState::load(&path, 1 << 21, 65536).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An unclean save (crash while attached) must be detectable, and
    /// mark_all_dirty must turn it into "everything changed since any
    /// older era" — the automatic full-copy fallback.
    #[test]
    fn unclean_load_falls_back_to_full_dirty() {
        let dir = std::env::temp_dir().join(format!("ublkera-test-uc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("meta.bin");

        let s = EraState::new(1 << 20, 65536).unwrap();
        s.mark_write(0, 1);
        s.checkpoint(); // era 2 now current; cursor 1 is a valid consumer cursor
        s.save(&path, false).unwrap(); // attached marker: crash would leave this

        let (l, clean) = EraState::load(&path, 1 << 20, 65536).unwrap();
        assert!(!clean);
        l.mark_all_dirty();
        let dirty: u64 = l.ranges_since(1).iter().map(|r| r.len).sum();
        assert_eq!(dirty, 1 << 20, "whole device must be reported changed");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Simulate a crash at every byte of a checkpoint save. `save` writes a
    /// tmp file and renames, so a crash can only leave: the previous file plus
    /// a torn tmp (any prefix, including empty and complete), or the renamed
    /// new file. Recovery must load a parseable file in every one of those
    /// states, see it marked unclean (both the attach marker and mid-attach
    /// checkpoint saves are), and degrade to full-dirty. Payload corruption of
    /// the *main* file is outside this model: rename never exposes a torn main
    /// file, and there is no checksum to catch external bit rot.
    #[test]
    fn crash_at_every_byte_of_a_save_degrades_safely() {
        let dir = std::env::temp_dir().join(format!("ublkera-test-torn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("meta.bin");
        let tmp = path.with_extension("tmp");

        // The attach marker that a checkpoint save would be replacing.
        let s = EraState::new(1 << 20, 65536).unwrap();
        s.mark_write(0, 1);
        s.save(&path, false).unwrap();
        let marker = std::fs::read(&path).unwrap();

        // The bytes a subsequent checkpoint save would write.
        s.checkpoint();
        s.mark_write(65536 * 3, 1);
        let scratch = dir.join("scratch.bin");
        s.save(&scratch, false).unwrap();
        let newer = std::fs::read(&scratch).unwrap();

        for cut in 0..=newer.len() {
            std::fs::write(&path, &marker).unwrap();
            std::fs::write(&tmp, &newer[..cut]).unwrap();

            // What manager::add does on the next attach.
            let (l, clean) = EraState::load(&path, 1 << 20, 65536)
                .unwrap_or_else(|e| panic!("cut={cut}: recovery load failed: {e:#}"));
            assert!(!clean, "cut={cut}: marker must be unclean");
            l.mark_all_dirty();
            let dirty: u64 = l.ranges_since(0).iter().map(|r| r.len).sum();
            assert_eq!(dirty, 1 << 20, "cut={cut}: full-dirty fallback");

            // And the next save must succeed over the leftover tmp file.
            l.save(&path, false).unwrap_or_else(|e| panic!("cut={cut}: re-save failed: {e:#}"));
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A truncated metadata file (external corruption; a crash cannot produce
    /// one, see above) must fail to load at every length — never load with a
    /// short chunk array or partial header.
    #[test]
    fn truncated_metadata_never_loads() {
        let dir = std::env::temp_dir().join(format!("ublkera-test-trunc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("meta.bin");
        let s = EraState::new(1 << 20, 65536).unwrap();
        s.mark_write(0, 1);
        s.save(&path, true).unwrap();
        let full = std::fs::read(&path).unwrap();

        for cut in 0..full.len() {
            std::fs::write(&path, &full[..cut]).unwrap();
            assert!(
                EraState::load(&path, 1 << 20, 65536).is_err(),
                "truncation at {cut}/{} bytes must not load",
                full.len()
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// External bit rot: corrupt every byte of the file in turn (and append
    /// trailing garbage); the CRC must reject each one. A corrupted era array
    /// that loaded silently would under-report changes — the one forbidden
    /// failure.
    #[test]
    fn corrupted_metadata_never_loads() {
        let dir = std::env::temp_dir().join(format!("ublkera-test-rot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("meta.bin");
        let s = EraState::new(1 << 20, 65536).unwrap();
        s.mark_write(65536 * 3, 1);
        s.checkpoint();
        s.save(&path, true).unwrap();
        let full = std::fs::read(&path).unwrap();

        for i in 0..full.len() {
            let mut bad = full.clone();
            bad[i] ^= 0xFF;
            std::fs::write(&path, &bad).unwrap();
            assert!(
                EraState::load(&path, 1 << 20, 65536).is_err(),
                "flipped byte {i}/{} must not load",
                full.len()
            );
        }
        let mut bad = full.clone();
        bad.push(0);
        std::fs::write(&path, &bad).unwrap();
        assert!(
            EraState::load(&path, 1 << 20, 65536).is_err(),
            "trailing garbage must not load"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tail_chunk_clamped() {
        // 1MiB + 4KiB device: last chunk is partial
        let s = EraState::new((1 << 20) + 4096, 65536).unwrap();
        assert_eq!(s.nr_chunks(), 17);
        s.mark_write(1 << 20, 4096);
        let r = s.ranges_since(0);
        assert_eq!(r[0].len, 4096);
    }

    /// A write racing a checkpoint must never fall into the gap where it is
    /// stamped with an already-closed era and thus skipped by *every* live
    /// `dump --since` cursor. We emulate the incremental-backup workflow: a
    /// writer stamps each chunk once while a checkpointer takes back-to-back
    /// checkpoints and, right after each, records the live `ranges_since(prev)`
    /// diff. The union of all diffs (plus a final sweep) must cover every chunk.
    /// Without the retry in `mark_write` this loses chunks under contention.
    #[test]
    fn no_write_lost_across_concurrent_checkpoints() {
        use std::collections::HashSet;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        const GRAN: u64 = 4096;
        const N: usize = 4096; // one chunk per write

        let s = Arc::new(EraState::new(GRAN * N as u64, GRAN).unwrap());
        let done = Arc::new(AtomicBool::new(false));

        let s_w = s.clone();
        let done_w = done.clone();
        let writer = std::thread::spawn(move || {
            for i in 0..N {
                s_w.mark_write(i as u64 * GRAN, 1);
                if i % 8 == 0 {
                    std::thread::yield_now();
                }
            }
            done_w.store(true, Ordering::Release);
        });

        let s_c = s.clone();
        let done_c = done.clone();
        let checker = std::thread::spawn(move || {
            let mut covered: HashSet<usize> = HashSet::new();
            let mut accumulate = |st: &EraState, since: u32| {
                for r in st.ranges_since(since) {
                    let start = (r.offset / GRAN) as usize;
                    let end = ((r.offset + r.len) / GRAN) as usize;
                    covered.extend(start..end);
                }
            };
            let mut prev = 0u32;
            loop {
                let (closed, _) = s_c.checkpoint();
                accumulate(&s_c, prev);
                prev = closed;
                if done_c.load(Ordering::Acquire) {
                    accumulate(&s_c, prev); // catch writes after the last checkpoint
                    break;
                }
                std::thread::yield_now();
            }
            covered
        });

        writer.join().unwrap();
        let covered = checker.join().unwrap();
        for i in 0..N {
            assert!(covered.contains(&i), "chunk {i} was lost from every incremental diff");
        }
    }
}
