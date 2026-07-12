#!/usr/bin/env bash
# Block-layer stress for a ublkera device. Run as root inside the noble VM
# (./scripts/vm-ubuntu.sh stress). Three parts:
#   1. fio --verify   self-verifying data patterns through the whole stack
#                     (sequential 1M exercises request splitting, random 4k
#                     at depth 32 exercises tag/slot reuse)
#   2. ext4 + fsck    a real filesystem is the best generator of ordering-
#                     sensitive IO (journal, FLUSH); fsck must come back clean
#   3. dm-flakey      backing errors injected while writing: every attempted
#                     block must still appear in `dump` — a failed write may
#                     have changed the medium, so dropping it would be a
#                     silent false negative
# Markers: BLKSTRESS-FAIL / BLKSTRESS-PASS
set -euo pipefail

fail() { echo "BLKSTRESS-FAIL: $*"; exit 1; }
jget() { python3 -c "import json,sys; print(json.load(sys.stdin)['$1'])"; }

[ "$(id -u)" = 0 ] || fail "run as root"
command -v fio >/dev/null || DEBIAN_FRONTEND=noninteractive apt-get install -qy fio >/dev/null
command -v dmsetup >/dev/null || DEBIAN_FRONTEND=noninteractive apt-get install -qy dmsetup >/dev/null

DISK=$(readlink -f /dev/disk/by-id/virtio-ublkera-data) || fail "virtio data disk missing"
if [ ! -b "${DISK}1" ]; then
    printf 'label: gpt\n,4GiB\n,\n' | sfdisk -q "$DISK"
    partprobe "$DISK"; sleep 1
fi
PA=${DISK}1 # parts 1+2: fio / ext4
PB=${DISK}2 # part 3: dm-flakey

MNT=/mnt/ublkera-stress
cleanup() {
    umount "$MNT" 2>/dev/null || true
    # detach everything before removing the dm device it may hold open
    ublkera shutdown >/dev/null 2>&1 || true
    sleep 1
    dmsetup remove --retry flaky0 2>/dev/null || true
}
trap cleanup EXIT

# leftovers from a previous (possibly failed) run
cleanup
ublkera daemon
sleep 1

echo "== 1. fio --verify through /dev/ublkbN (4 queues) =="
ID=$(ublkera add -f "$PA" -g 64K -q 4 | jget dev_id)
sleep 0.5
DEV=/dev/ublkb$ID

fio --name=seqv --filename="$DEV" --rw=write --bs=1M --size=2G --direct=1 \
    --verify=crc32c --verify_backlog=256 --do_verify=1 \
    --output-format=terse >/dev/null || fail "fio seq 1M verify"
echo "  seq 1M + crc32c verify: ok"

fio --name=randv --filename="$DEV" --rw=randwrite --bs=4k --size=512M --direct=1 \
    --ioengine=libaio --iodepth=32 --verify=crc32c --verify_backlog=1024 \
    --do_verify=1 --output-format=terse >/dev/null || fail "fio rand 4k qd32 verify"
echo "  rand 4k qd32 + crc32c verify: ok"

echo "== 2. ext4 on the device, verified load, fsck =="
mkfs.ext4 -Fq "$DEV" || fail "mkfs.ext4"
mkdir -p "$MNT"
mount "$DEV" "$MNT" || fail "mount"
fio --name=fsv --directory="$MNT" --rw=randwrite --bs=16k --size=256M \
    --nrfiles=8 --verify=crc32c --do_verify=1 \
    --output-format=terse >/dev/null || fail "fio on ext4"
ublkera checkpoint -n "$ID" >/dev/null || fail "checkpoint under FS load"
umount "$MNT"
fsck.ext4 -fn "$DEV" >/dev/null || fail "fsck found errors"
echo "  ext4 + verified load + fsck: ok"

ublkera del -n "$ID" >/dev/null

echo "== 3. dm-flakey backing: failed writes must still appear in dump =="
modprobe dm-flakey 2>/dev/null || true
SECT=$(blockdev --getsz "$PB")
# up=0/down=1: permanently down, every write fails with EIO. Writing each
# chunk exactly once means NO write succeeds — a "stamp only successful
# writes" implementation would report nothing changed, which is exactly the
# false negative this guards against (a failed write may still have changed
# the medium).
dmsetup create flaky0 --table "0 $SECT flakey $PB 0 0 1 1 error_writes" || fail "dmsetup create flakey"
ID=$(ublkera add -f /dev/mapper/flaky0 -g 64K | jget dev_id)
sleep 0.5
DEV=/dev/ublkb$ID

CUR=$(ublkera checkpoint -n "$ID" | jget closed_era)
FAILED=0
for i in $(seq 0 63); do
    dd if=/dev/urandom of="$DEV" bs=64K count=1 seek=$i oflag=direct conv=notrunc 2>/dev/null ||
        FAILED=$((FAILED + 1))
done
[ "$FAILED" = 64 ] || fail "expected all 64 writes to fail, got $FAILED (flakey not engaged)"

# all 64 attempted (and failed) chunks must be reported changed
ublkera dump -n "$ID" --since "$CUR" | python3 -c '
import json, sys
d = json.load(sys.stdin)
covered = set()
for r in d["ranges"]:
    for off in range(r["offset"], r["offset"] + r["len"], 65536):
        covered.add(off // 65536)
missing = [c for c in range(64) if c not in covered]
if missing:
    sys.exit(f"chunks missing from dump despite attempted writes: {missing}")
print("  all 64 failed-write chunks present in dump")
' || fail "dump lost chunks whose writes failed"

# heal the backing and confirm the device is still fully usable
dmsetup suspend flaky0
dmsetup reload flaky0 --table "0 $SECT linear $PB 0"
dmsetup resume flaky0
dd if=/dev/urandom of=/tmp/blkstress.pat bs=64K count=1 2>/dev/null
dd if=/tmp/blkstress.pat of="$DEV" bs=64K count=1 seek=7 oflag=direct conv=notrunc 2>/dev/null ||
    fail "write still failing after backing healed"
cmp <(dd if="$DEV" bs=64K count=1 skip=7 iflag=direct 2>/dev/null) /tmp/blkstress.pat ||
    fail "read-back mismatch after backing healed"
echo "  device healthy again after error storm"

ublkera del -n "$ID" >/dev/null
sleep 1
dmsetup remove --retry flaky0

echo "BLKSTRESS-PASS"
