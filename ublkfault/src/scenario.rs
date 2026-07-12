//! Declarative scenario runner: a scenario file is a list of steps executed
//! against a block device (O_DIRECT) plus the fault-device control socket.
//! The same file runs against a ublkfault device directly (does the model
//! honor the contract?) and against a SUT stacked on top of one (does the
//! SUT forward the contract?).
//!
//! Format: one step per line, `#` comments and blank lines ignored.
//!
//!   write       off=0 len=1M pattern=base [fsync]
//!   write-fails off=0 len=4K pattern=x        # the write must error
//!   expect      off=0 len=1M pattern=base     # pattern "zero" = all zeros
//!   flush                                     # fsync the device fd
//!   crash       mode=drop|seeded              # fault ctl: power loss
//!   set         error_pm=1000 flush_lie_pm=0  # fault ctl: policy
//!   thaw        result=ok|eio                 # fault ctl: release hangs
//!   sleep       ms=500
//!   run         <shell command>               # must exit 0
//!   fail        <shell command>               # must exit non-zero
//!
//! Pattern content is derived deterministically from the pattern name, so
//! `expect` needs only the name, never the bytes.

use crate::model::Rng;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;

#[derive(Debug, PartialEq)]
enum Step {
    Write { off: u64, len: u64, pattern: String, fsync: bool, expect_error: bool },
    Expect { off: u64, len: u64, pattern: String },
    Flush,
    Crash { mode: String },
    Set { pairs: Vec<(String, u64)> },
    Thaw { result: String },
    Sleep { ms: u64 },
    Run { cmd: String, expect_ok: bool },
}

fn parse_size(s: &str) -> Result<u64> {
    let (num, mul) = match s.chars().last() {
        Some('K') | Some('k') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M') | Some('m') => (&s[..s.len() - 1], 1u64 << 20),
        Some('G') | Some('g') => (&s[..s.len() - 1], 1u64 << 30),
        _ => (s, 1),
    };
    Ok(num.parse::<u64>().with_context(|| format!("bad size '{s}'"))? * mul)
}

fn parse_line(line: &str) -> Result<Option<Step>> {
    let line = line.split('#').next().unwrap_or("").trim();
    if line.is_empty() {
        return Ok(None);
    }
    let (verb, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();

    // run/fail take the rest of the line as a shell command
    if verb == "run" || verb == "fail" {
        if rest.is_empty() {
            bail!("{verb}: missing command");
        }
        return Ok(Some(Step::Run { cmd: rest.to_string(), expect_ok: verb == "run" }));
    }

    let mut args: HashMap<&str, &str> = HashMap::new();
    let mut flags: Vec<&str> = Vec::new();
    for tok in rest.split_whitespace() {
        match tok.split_once('=') {
            Some((k, v)) => {
                args.insert(k, v);
            }
            None => flags.push(tok),
        }
    }
    let need = |k: &str| -> Result<&str> {
        args.get(k).copied().with_context(|| format!("{verb}: missing {k}="))
    };

    let step = match verb {
        "write" | "write-fails" => Step::Write {
            off: parse_size(need("off")?)?,
            len: parse_size(need("len")?)?,
            pattern: need("pattern")?.to_string(),
            fsync: flags.contains(&"fsync"),
            expect_error: verb == "write-fails",
        },
        "expect" => Step::Expect {
            off: parse_size(need("off")?)?,
            len: parse_size(need("len")?)?,
            pattern: need("pattern")?.to_string(),
        },
        "flush" => Step::Flush,
        "crash" => Step::Crash { mode: need("mode")?.to_string() },
        "set" => {
            let mut pairs = Vec::new();
            for (k, v) in &args {
                pairs.push((k.to_string(), v.parse::<u64>().with_context(|| format!("set: {k}={v}"))?));
            }
            if pairs.is_empty() {
                bail!("set: no key=value pairs");
            }
            pairs.sort();
            Step::Set { pairs }
        }
        "thaw" => Step::Thaw { result: need("result")?.to_string() },
        "sleep" => Step::Sleep { ms: need("ms")?.parse()? },
        other => bail!("unknown verb '{other}'"),
    };
    Ok(Some(step))
}

/// Deterministic pattern bytes, anchored to the *device offset*: byte X of
/// the device always holds the same value for a given pattern name, no
/// matter how the writes and reads are split. So
/// `expect off=64K len=192K pattern=p` can check a subrange of an earlier
/// `write off=0 len=256K pattern=p`. "zero" is all zeros. `dev_off` must be
/// 4K-aligned (enforced by the caller).
fn fill_pattern(name: &str, dev_off: u64, buf: &mut [u8]) {
    if name == "zero" {
        buf.fill(0);
        return;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x1000_0000_01b3);
    }
    for (i, block) in buf.chunks_mut(4096).enumerate() {
        let mut rng = Rng(h ^ (dev_off / 4096 + i as u64));
        for chunk in block.chunks_mut(8) {
            let b = rng.next().to_le_bytes();
            chunk.copy_from_slice(&b[..chunk.len()]);
        }
    }
}

/// 4096-aligned buffer for O_DIRECT.
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

/// One request/response on the fault control socket.
pub fn ctl(socket: &Path, req: &Value) -> Result<Value> {
    let mut conn = std::os::unix::net::UnixStream::connect(socket)
        .with_context(|| format!("connect {} (is `serve` running?)", socket.display()))?;
    writeln!(conn, "{req}")?;
    let mut line = String::new();
    BufReader::new(&conn).read_line(&mut line)?;
    let resp: Value = serde_json::from_str(line.trim()).context("parse ctl response")?;
    if resp["ok"].as_bool() != Some(true) {
        bail!("ctl {req}: {resp}");
    }
    Ok(resp)
}

pub fn run_file(path: &Path, dev_path: &str, socket: &Path) -> Result<()> {
    let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("scenario");
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read scenario {}", path.display()))?;
    let dev = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT)
        .open(dev_path)
        .with_context(|| format!("open {dev_path} O_DIRECT"))?;

    let mut steps = 0;
    for (lno, line) in text.lines().enumerate() {
        let step = match parse_line(line) {
            Ok(Some(s)) => s,
            Ok(None) => continue,
            Err(e) => bail!("{name}:{}: {e:#}", lno + 1),
        };
        exec(&step, &dev, socket)
            .with_context(|| format!("{name}:{} `{}`", lno + 1, line.trim()))?;
        steps += 1;
    }
    println!("SCENARIO {name}: {steps} steps ok");
    Ok(())
}

fn exec(step: &Step, dev: &std::fs::File, socket: &Path) -> Result<()> {
    match step {
        Step::Write { off, len, pattern, fsync, expect_error } => {
            if off % 4096 != 0 {
                bail!("off must be 4K-aligned for pattern anchoring");
            }
            let mut buf = DioBuf::new(*len as usize);
            fill_pattern(pattern, *off, buf.get_mut());
            let res = dev.write_all_at(buf.get(), *off);
            match (res, expect_error) {
                (Ok(()), false) => {
                    if *fsync {
                        dev.sync_all().context("fsync")?;
                    }
                }
                (Err(e), false) => return Err(e).context("write"),
                (Err(_), true) => {}
                (Ok(()), true) => bail!("write succeeded but was expected to fail"),
            }
        }
        Step::Expect { off, len, pattern } => {
            if off % 4096 != 0 {
                bail!("off must be 4K-aligned for pattern anchoring");
            }
            let mut want = vec![0u8; *len as usize];
            fill_pattern(pattern, *off, &mut want);
            let mut buf = DioBuf::new((*len as usize).min(1 << 20));
            let mut pos = 0usize;
            while pos < want.len() {
                let n = (want.len() - pos).min(1 << 20);
                dev.read_exact_at(&mut buf.get_mut()[..n], off + pos as u64)
                    .context("read")?;
                if buf.get()[..n] != want[pos..pos + n] {
                    let first = buf.get()[..n]
                        .iter()
                        .zip(&want[pos..pos + n])
                        .position(|(a, b)| a != b)
                        .unwrap();
                    bail!(
                        "content mismatch: expected pattern '{pattern}', first differing \
                         byte at device offset {}",
                        off + (pos + first) as u64
                    );
                }
                pos += n;
            }
        }
        Step::Flush => dev.sync_all().context("flush")?,
        Step::Crash { mode } => {
            ctl(socket, &json!({"cmd": "crash", "mode": mode}))?;
        }
        Step::Set { pairs } => {
            let mut req = json!({"cmd": "set"});
            for (k, v) in pairs {
                req[k] = json!(v);
            }
            ctl(socket, &req)?;
        }
        Step::Thaw { result } => {
            ctl(socket, &json!({"cmd": "thaw", "result": result}))?;
        }
        Step::Sleep { ms } => std::thread::sleep(std::time::Duration::from_millis(*ms)),
        Step::Run { cmd, expect_ok } => {
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .status()
                .context("spawn sh")?;
            if status.success() != *expect_ok {
                bail!(
                    "command exited {} but was expected to {}",
                    status.code().map_or("by signal".into(), |c| c.to_string()),
                    if *expect_ok { "succeed" } else { "fail" }
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_basic_verbs() {
        assert_eq!(
            parse_line("write off=4K len=1M pattern=base fsync").unwrap(),
            Some(Step::Write {
                off: 4096,
                len: 1 << 20,
                pattern: "base".into(),
                fsync: true,
                expect_error: false,
            })
        );
        assert_eq!(
            parse_line("crash mode=drop # power loss").unwrap(),
            Some(Step::Crash { mode: "drop".into() })
        );
        assert_eq!(
            parse_line("set error_pm=1000").unwrap(),
            Some(Step::Set { pairs: vec![("error_pm".into(), 1000)] })
        );
        assert_eq!(
            parse_line("run ublkera checkpoint -n 0").unwrap(),
            Some(Step::Run { cmd: "ublkera checkpoint -n 0".into(), expect_ok: true })
        );
        assert_eq!(parse_line("  # comment only ").unwrap(), None);
        assert_eq!(parse_line("").unwrap(), None);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse_line("write len=1M pattern=x").is_err()); // missing off
        assert!(parse_line("frobnicate off=0").is_err());
        assert!(parse_line("set").is_err());
        assert!(parse_line("run").is_err());
    }

    #[test]
    fn patterns_are_deterministic_and_distinct() {
        let mut a1 = vec![0u8; 8192];
        let mut a2 = vec![0u8; 8192];
        let mut b = vec![0u8; 8192];
        fill_pattern("alpha", 0, &mut a1);
        fill_pattern("alpha", 0, &mut a2);
        fill_pattern("beta", 0, &mut b);
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
        let mut z = vec![7u8; 64];
        fill_pattern("zero", 0, &mut z);
        assert_eq!(z, vec![0u8; 64]);
    }

    /// A subrange expectation must agree with a larger write that covered it:
    /// pattern bytes are anchored to the device offset, not the buffer.
    #[test]
    fn patterns_are_offset_anchored() {
        let mut whole = vec![0u8; 256 * 1024];
        fill_pattern("p", 0, &mut whole);
        let mut sub = vec![0u8; 192 * 1024];
        fill_pattern("p", 64 * 1024, &mut sub);
        assert_eq!(&whole[64 * 1024..], &sub[..]);
    }
}
