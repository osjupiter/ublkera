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
    t.params = libublk::sys::ublk_params {
        types: libublk::sys::UBLK_PARAM_TYPE_BASIC | libublk::sys::UBLK_PARAM_TYPE_DISCARD,
        basic: libublk::sys::ublk_param_basic {
            logical_bs_shift: bs_shifts.0,
            physical_bs_shift: bs_shifts.1,
            io_opt_shift: 12,
            io_min_shift: 9,
            max_sectors: dev.dev_info.max_io_buf_bytes >> 9,
            dev_sectors: dev_size >> 9,
            ..Default::default()
        },
        // Advertise DISCARD so the kernel actually forwards blkdiscard to us;
        // handle_io_cmd turns it into a hole-punch/discard on the backing store.
        // WRITE_ZEROES is left unadvertised (max_write_zeroes_sectors = 0).
        discard: libublk::sys::ublk_param_discard {
            discard_granularity: 1u32 << bs_shifts.0,
            max_discard_sectors: 1 << 22, // 2 GiB per request; kernel splits larger
            max_discard_segments: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    dev.set_target_json(serde_json::json!({
        "ublkera": { "backing": tgt.back_file_path, "direct_io": tgt.direct_io }
    }));
    Ok(())
}

/// Auto buffer registration descriptor for zero-copy: the driver registers
/// the request's pages as fixed buffer `tag` in this queue's ring at FETCH
/// time and unregisters them at COMMIT time.
fn auto_reg(tag: u16) -> libublk::sys::ublk_auto_buf_reg {
    libublk::sys::ublk_auto_buf_reg {
        index: tag,
        ..Default::default()
    }
}

/// The backing-store SQE for a passthrough op, or `None` for an op we don't
/// support (WRITE_ZEROES, zoned ops, ...). This is the single source of truth
/// for "which ops does ublkera pass through"; the caller turns `None` into
/// -EINVAL. `len` is a `u64` because DISCARD ranges can exceed the 512K IO
/// buffer; READ/WRITE lengths are bounded by `max_sectors` so the cast is safe.
///
/// `zc_tag`: in zero-copy mode the request's pages are registered as fixed
/// buffer `tag`, so READ/WRITE become the *Fixed variants addressing that
/// buffer (offset 0) instead of copying through our own buffer.
fn make_sqe(op: u32, off: u64, len: u64, buf: *mut u8, zc_tag: Option<u16>) -> Option<squeue::Entry> {
    let sqe = match op {
        libublk::sys::UBLK_IO_OP_FLUSH => opcode::Fsync::new(types::Fixed(BACKING_FIXED_FD))
            .build()
            .flags(squeue::Flags::FIXED_FILE),
        libublk::sys::UBLK_IO_OP_READ => match zc_tag {
            Some(tag) => opcode::ReadFixed::new(
                types::Fixed(BACKING_FIXED_FD),
                std::ptr::null_mut(),
                len as u32,
                tag,
            )
            .offset(off)
            .build()
            .flags(squeue::Flags::FIXED_FILE),
            None => opcode::Read::new(types::Fixed(BACKING_FIXED_FD), buf, len as u32)
                .offset(off)
                .build()
                .flags(squeue::Flags::FIXED_FILE),
        },
        libublk::sys::UBLK_IO_OP_WRITE => match zc_tag {
            Some(tag) => opcode::WriteFixed::new(
                types::Fixed(BACKING_FIXED_FD),
                std::ptr::null(),
                len as u32,
                tag,
            )
            .offset(off)
            .build()
            .flags(squeue::Flags::FIXED_FILE),
            None => opcode::Write::new(types::Fixed(BACKING_FIXED_FD), buf, len as u32)
                .offset(off)
                .build()
                .flags(squeue::Flags::FIXED_FILE),
        },
        // DISCARD => punch a hole in the backing range. On a block device this
        // issues a real discard; on a regular file it deallocates the extent.
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
fn handle_io_cmd(q: &UblkQueue, tag: u16, io: &UblkIOCtx, era: &EraState, buf: Option<&[u8]>) {
    let iod = q.get_iod(tag);
    let op = iod.op_flags & 0xff;
    let off = iod.start_sector << 9;
    let bytes = (iod.nr_sectors as u64) << 9;
    // Copy mode answers with our buffer; zero-copy answers with the same
    // auto-registration descriptor, which unregisters the request's pages.
    let desc = match buf {
        Some(b) => BufDesc::Slice(b),
        None => BufDesc::AutoReg(auto_reg(tag)),
    };

    // phase BACKEND: our backing-store IO completed.
    if io.is_tgt_io() {
        let res = io.result();
        if res != -libc::EAGAIN {
            // Stamp the era map once the backing store accepted a data-changing
            // op, so the map never claims less than what may have changed. WRITE
            // reports the byte count in `res`; DISCARD reports 0 on success, so
            // use the requested range length.
            match op {
                libublk::sys::UBLK_IO_OP_WRITE if res > 0 => era.mark_write(off, res as u64),
                libublk::sys::UBLK_IO_OP_DISCARD if res >= 0 => era.mark_write(off, bytes),
                _ => {}
            }
            q.complete_io_cmd_unified(tag, desc, Ok(UblkIORes::Result(res)))
                .unwrap();
            return;
        }
        // EAGAIN: fall through and resubmit the same backing IO.
    }

    // phase UBLK (or an EAGAIN retry): start the backing IO for a supported op,
    // otherwise reject it (WRITE_ZEROES, zoned ops, ...) with -EINVAL.
    let buf_ptr = buf.map_or(std::ptr::null_mut(), |b| b.as_ptr() as *mut u8);
    let zc_tag = buf.is_none().then_some(tag);
    match make_sqe(op, off, bytes, buf_ptr, zc_tag) {
        // Tag the SQE as a target IO so the next CQE for this tag lands in the
        // BACKEND phase above (is_tgt_io == true).
        Some(sqe) => {
            let data = UblkIOCtx::build_user_data(tag, op, 0, true);
            q.ublk_submit_sqe_sync(sqe.user_data(data)).unwrap();
        }
        None => {
            q.complete_io_cmd_unified(tag, desc, Ok(UblkIORes::Result(-libc::EINVAL)))
                .unwrap();
        }
    }
}

/// Per-queue handler: one OS thread runs a single io_uring, looping over every
/// completion (ublk commands and backing IOs share the ring) and dispatching
/// each to the state machine above. No async runtime, no per-tag tasks.
///
/// Zero-copy (UBLK_F_AUTO_BUF_REG, negotiated at device creation): the driver
/// registers each request's pages as fixed buffer `tag` in this ring, the
/// backing IO addresses them with ReadFixed/WriteFixed, and no per-tag copy
/// buffers exist at all. Otherwise: classic copy mode through per-tag buffers.
pub fn queue_fn(qid: u16, dev: &UblkDev, era: Arc<EraState>) {
    let zc = dev.dev_info.flags & (libublk::sys::UBLK_F_AUTO_BUF_REG as u64) != 0;

    if zc {
        let regs: Vec<libublk::sys::ublk_auto_buf_reg> =
            (0..dev.dev_info.queue_depth).map(auto_reg).collect();
        let handler = move |q: &UblkQueue, tag: u16, io: &UblkIOCtx| {
            handle_io_cmd(q, tag, io, &era, None);
        };
        let queue = match UblkQueue::new(qid, dev)
            .and_then(|q| q.submit_fetch_commands_unified(BufDescList::AutoRegs(&regs)))
        {
            Ok(q) => q,
            Err(e) => {
                log::error!("queue {qid} setup failed: {e}");
                return;
            }
        };
        queue.wait_and_handle_io(handler);
        return;
    }

    // One buffer per queue slot (index == tag); lives for the queue's lifetime.
    let bufs = Rc::new(dev.alloc_queue_io_bufs());
    let bufs_h = bufs.clone();

    let handler = move |q: &UblkQueue, tag: u16, io: &UblkIOCtx| {
        let buf = bufs_h[tag as usize].as_slice();
        handle_io_cmd(q, tag, io, &era, Some(buf));
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
