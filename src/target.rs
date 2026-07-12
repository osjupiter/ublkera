//! The ublk target: a loop-like pass-through to a backing file or block
//! device, stamping the era map on every completed write.

use crate::era::EraState;
use anyhow::Result;
use ilog::IntLog;
use io_uring::{opcode, squeue, types};
use libublk::io::{BufDescList, UblkDev, UblkIOCtx, UblkQueue};
use libublk::{BufDesc, UblkError, UblkIORes};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::io::AsRawFd;
use std::rc::Rc;
use std::sync::Arc;

pub struct EraTgt {
    pub back_file_path: String,
    pub back_file: std::fs::File,
    pub direct_io: bool,
    /// advertise DISCARD upward; see `backing_supports_discard`
    pub discard: bool,
}

const BLK_IOCTL_TYPE: u8 = 0x12; // linux/fs.h
const BLKGETSIZE64_NR: u8 = 114;
const BLKSSZGET_NR: u8 = 104;
const BLKPBSZGET_NR: u8 = 123;

nix::ioctl_read!(ioctl_blkgetsize64, BLK_IOCTL_TYPE, BLKGETSIZE64_NR, u64);
nix::ioctl_read_bad!(
    ioctl_blksszget,
    nix::request_code_none!(BLK_IOCTL_TYPE, BLKSSZGET_NR),
    i32
);
nix::ioctl_read_bad!(
    ioctl_blkpbszget,
    nix::request_code_none!(BLK_IOCTL_TYPE, BLKPBSZGET_NR),
    u32
);

// io_uring registered-file ("fixed file") indices. libublk registers
// `/dev/ublkcN` at index 0, then target code's fds right after it (see
// `init_tgt`, which stores the backing fd next). Deriving BACKING from CDEV
// keeps "backing sits immediately after the control device" explicit.
/// registered-file index of the ublk control device `/dev/ublkcN`
const CDEV_FIXED_FD: u32 = 0;
/// registered-file index of the backing device/file
const BACKING_FIXED_FD: u32 = CDEV_FIXED_FD + 1;

/// ublk_cmd.h; missing from libublk's generated bindings (same gap as the
/// `seg` params field). Without this attr the kernel treats the device as
/// write-through and never sends FLUSH, so a consumer's fsync would be
/// acked without ever reaching the backing store.
const UBLK_ATTR_VOLATILE_CACHE: u32 = 1 << 2;

/// Whether the backing store can actually execute our DISCARD passthrough
/// (an fallocate PUNCH_HOLE). On a regular file that deallocates the extent —
/// always available. On a block device the kernel implements PUNCH_HOLE as
/// WRITE_ZEROES with no fallback, so it only works when the backing device
/// advertises write-zeroes; otherwise every discard would fail with
/// EOPNOTSUPP mid-flight. We probe sysfs and simply don't advertise DISCARD
/// when the backing can't take it — the consumer's blkdiscard then fails
/// cleanly upfront instead of erroring per-bio.
pub fn backing_supports_discard(f: &std::fs::File) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = f.metadata() else { return false };
    if meta.file_type().is_file() {
        return true;
    }
    if !meta.file_type().is_block_device() {
        return false;
    }
    let rdev = meta.rdev();
    let sys = format!("/sys/dev/block/{}:{}", libc::major(rdev), libc::minor(rdev));
    let Ok(canon) = std::fs::canonicalize(&sys) else { return false };
    // whole disks have queue/ directly; partitions sit one level below it
    for dir in [canon.join("queue"), canon.parent().map(|p| p.join("queue")).unwrap_or_default()] {
        if let Ok(s) = std::fs::read_to_string(dir.join("write_zeroes_max_bytes")) {
            return s.trim().parse::<u64>().map(|v| v > 0).unwrap_or(false);
        }
    }
    false
}

/// (size_bytes, logical_bs_shift, physical_bs_shift) of a file or block device.
pub fn backing_size(f: &std::fs::File) -> Result<(u64, u8, u8)> {
    let meta = f.metadata()?;
    if meta.file_type().is_block_device() {
        let fd = f.as_raw_fd();
        let mut cap = 0_u64;
        let mut ssz = 0_i32;
        let mut pbsz = 0_u32;
        unsafe {
            ioctl_blkgetsize64(fd, &mut cap as *mut u64)?;
            ioctl_blksszget(fd, &mut ssz as *mut i32)?;
            ioctl_blkpbszget(fd, &mut pbsz as *mut u32)?;
        }
        Ok((cap, ssz.log2() as u8, pbsz.log2() as u8))
    } else if meta.file_type().is_file() {
        Ok((meta.len(), 9, 12))
    } else {
        anyhow::bail!("backing target must be a regular file or block device")
    }
}

pub fn init_tgt(dev: &mut UblkDev, tgt: &EraTgt, dev_size: u64, bs_shifts: (u8, u8)) -> Result<(), UblkError> {
    if tgt.direct_io {
        unsafe {
            libc::fcntl(tgt.back_file.as_raw_fd(), libc::F_SETFL, libc::O_DIRECT);
        }
    }

    let t = &mut dev.tgt;
    let nr_fds = t.nr_fds;
    t.fds[nr_fds as usize] = tgt.back_file.as_raw_fd();
    t.nr_fds = nr_fds + 1;

    t.dev_size = dev_size;
    let mut types = libublk::sys::UBLK_PARAM_TYPE_BASIC;
    if tgt.discard {
        types |= libublk::sys::UBLK_PARAM_TYPE_DISCARD;
    }
    t.params = libublk::sys::ublk_params {
        types,
        basic: libublk::sys::ublk_param_basic {
            // Declare a volatile cache so the kernel delivers FLUSH to us;
            // handle_io_cmd forwards it as an fsync on the backing store.
            attrs: UBLK_ATTR_VOLATILE_CACHE,
            logical_bs_shift: bs_shifts.0,
            physical_bs_shift: bs_shifts.1,
            io_opt_shift: 12,
            io_min_shift: 9,
            max_sectors: dev.dev_info.max_io_buf_bytes >> 9,
            dev_sectors: dev_size >> 9,
            ..Default::default()
        },
        // Advertise DISCARD (when the backing can execute it) so the kernel
        // forwards blkdiscard to us; handle_io_cmd turns it into a hole punch
        // on the backing store. WRITE_ZEROES is left unadvertised
        // (max_write_zeroes_sectors = 0).
        discard: if tgt.discard {
            libublk::sys::ublk_param_discard {
                discard_granularity: 1u32 << bs_shifts.0,
                max_discard_sectors: 1 << 22, // 2 GiB per request; kernel splits larger
                max_discard_segments: 1,
                ..Default::default()
            }
        } else {
            Default::default()
        },
        ..Default::default()
    };
    dev.set_target_json(serde_json::json!({
        "ublkera": { "backing": tgt.back_file_path, "direct_io": tgt.direct_io }
    }));
    Ok(())
}

/// The backing-store SQE for a passthrough op, or `None` for an op we don't
/// support (WRITE_ZEROES, zoned ops, ...). This is the single source of truth
/// for "which ops does ublkera pass through"; the caller turns `None` into
/// -EINVAL. `len` is a `u64` because DISCARD ranges can exceed the 512K IO
/// buffer; READ/WRITE lengths are bounded by `max_sectors` so the cast is safe.
fn make_sqe(op: u32, off: u64, len: u64, buf: *mut u8) -> Option<squeue::Entry> {
    let sqe = match op {
        libublk::sys::UBLK_IO_OP_FLUSH => opcode::Fsync::new(types::Fixed(BACKING_FIXED_FD))
            .build()
            .flags(squeue::Flags::FIXED_FILE),
        libublk::sys::UBLK_IO_OP_READ => {
            opcode::Read::new(types::Fixed(BACKING_FIXED_FD), buf, len as u32)
                .offset(off)
                .build()
                .flags(squeue::Flags::FIXED_FILE)
        }
        libublk::sys::UBLK_IO_OP_WRITE => {
            opcode::Write::new(types::Fixed(BACKING_FIXED_FD), buf, len as u32)
                .offset(off)
                .build()
                .flags(squeue::Flags::FIXED_FILE)
        }
        // DISCARD => punch a hole in the backing range: extent deallocation
        // on a regular file, WRITE_ZEROES on a block device. Only reachable
        // when `backing_supports_discard` said the backing can take it.
        libublk::sys::UBLK_IO_OP_DISCARD => {
            opcode::Fallocate::new(types::Fixed(BACKING_FIXED_FD), len)
                .offset(off)
                .mode(libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE)
                .build()
                .flags(squeue::Flags::FIXED_FILE)
        }
        _ => return None,
    };
    Some(sqe)
}

/// One turn of the per-tag state machine, driven by a single CQE.
///
/// Each IO tag walks a two-state cycle and `wait_and_handle_io` calls us once
/// per io_uring completion. `io.is_tgt_io()` is the "phase" bit that tells the
/// two states apart:
///
/// * phase UBLK (`!is_tgt_io`): a fresh IO command arrived from the ublk
///   driver (a FETCH completed). We validate it and submit the matching
///   backing-store IO onto the *same* ring, tagged as a target IO.
/// * phase BACKEND (`is_tgt_io`): the backing IO we submitted has finished.
///   We stamp the era map (writes only) and commit the result back to the
///   driver with COMMIT_AND_FETCH_REQ, which in one command both reports this
///   result and re-arms the fetch for the next IO on this tag.
fn handle_io_cmd(q: &UblkQueue, tag: u16, io: &UblkIOCtx, era: &EraState, buf: &[u8]) {
    let iod = q.get_iod(tag);
    let op = iod.op_flags & 0xff;
    let off = iod.start_sector << 9;
    let bytes = (iod.nr_sectors as u64) << 9;

    // phase BACKEND: our backing-store IO completed.
    if io.is_tgt_io() {
        let res = io.result();
        if res != -libc::EAGAIN {
            // Stamp the era map for every data-changing op, including failed or
            // short ones: an errored WRITE/DISCARD may still have reached the
            // medium partially, and the map must never claim less than what may
            // have changed. Over-reporting a failed op that changed nothing only
            // costs the consumer an extra copy.
            match op {
                libublk::sys::UBLK_IO_OP_WRITE | libublk::sys::UBLK_IO_OP_DISCARD => {
                    era.mark_write(off, bytes)
                }
                _ => {}
            }
            q.complete_io_cmd_unified(tag, BufDesc::Slice(buf), Ok(UblkIORes::Result(res)))
                .unwrap();
            return;
        }
        // EAGAIN: fall through and resubmit the same backing IO.
    }

    // phase UBLK (or an EAGAIN retry): start the backing IO for a supported op,
    // otherwise reject it (WRITE_ZEROES, zoned ops, ...) with -EINVAL.
    match make_sqe(op, off, bytes, buf.as_ptr() as *mut u8) {
        // Tag the SQE as a target IO so the next CQE for this tag lands in the
        // BACKEND phase above (is_tgt_io == true).
        Some(sqe) => {
            let data = UblkIOCtx::build_user_data(tag, op, 0, true);
            q.ublk_submit_sqe_sync(sqe.user_data(data)).unwrap();
        }
        None => {
            q.complete_io_cmd_unified(tag, BufDesc::Slice(buf), Ok(UblkIORes::Result(-libc::EINVAL)))
                .unwrap();
        }
    }
}

/// Per-queue handler: one OS thread runs a single io_uring, looping over every
/// completion (ublk commands and backing IOs share the ring) and dispatching
/// each to the state machine above. No async runtime, no per-tag tasks.
pub fn queue_fn(qid: u16, dev: &UblkDev, era: Arc<EraState>) {
    // Name this queue thread after the device node + queue ("ublkb0-q1"):
    // the kernel truncates comm to 15 chars, so the inherited supervisor
    // name ("ublkera-dev-/de...") is both cut off and identical across
    // queues in tools like pidstat.
    if let Ok(name) = std::ffi::CString::new(format!("ublkb{}-q{}", dev.dev_info.dev_id, qid)) {
        unsafe { libc::prctl(libc::PR_SET_NAME, name.as_ptr()) };
    }

    // One buffer per queue slot (index == tag); lives for the queue's lifetime.
    let bufs = Rc::new(dev.alloc_queue_io_bufs());
    let bufs_h = bufs.clone();

    let handler = move |q: &UblkQueue, tag: u16, io: &UblkIOCtx| {
        let buf = bufs_h[tag as usize].as_slice();
        handle_io_cmd(q, tag, io, &era, buf);
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

    // Blocks in io_uring_enter, handling CQEs until the queue goes down.
    queue.wait_and_handle_io(handler);
}
