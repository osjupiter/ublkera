#!/bin/busybox sh
# Runs as /init inside the QEMU guest (booted from the host kernel with a
# minimal initramfs). Exercises the full ublkera flow as root and reports
# via serial console markers: "E2E-VM-FAIL: ..." / "E2E-VM-PASS".
# busybox-only: no python/jq, JSON fields are picked out with grep.

/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs dev /dev 2>/dev/null
mkdir -p /tmp

fail() {
    echo "E2E-VM-FAIL: $*"
    poweroff -f
}

# first numeric value of a JSON field from stdin, e.g. jget dev_id
jget() {
    grep -o "\"$1\": *[0-9]*" | head -1 | grep -o '[0-9]*$'
}

SOCK=/tmp/daemon.sock
CTL="/ublkera --socket $SOCK"

wait_for() { # wait_for <what> <test-op> <path>
    i=0
    while ! test "$2" "$3" && [ $i -lt 100 ]; do sleep 0.1; i=$((i + 1)); done
    test "$2" "$3" || fail "$1 did not appear"
}

echo "E2E-VM: load ublk_drv"
insmod /ublk_drv.ko || fail "insmod ublk_drv"
wait_for /dev/ublk-control -e /dev/ublk-control

echo "E2E-VM: start daemon"
$CTL daemon --foreground &
wait_for "daemon socket" -S "$SOCK"

echo "E2E-VM: attach two backing files"
dd if=/dev/zero of=/a.img bs=1M count=1 seek=63 2>/dev/null || fail "create a.img"
dd if=/dev/zero of=/b.img bs=1M count=1 seek=31 2>/dev/null || fail "create b.img"
ID_A=$($CTL add -f /a.img -g 64K --meta /a.meta --buffered | jget dev_id)
[ -n "$ID_A" ] || fail "attach a.img"
ID_B=$($CTL add -f /b.img -g 64K --meta /b.meta --buffered | jget dev_id)
[ -n "$ID_B" ] || fail "attach b.img"
DEV_A=/dev/ublkb$ID_A
DEV_B=/dev/ublkb$ID_B
wait_for "$DEV_A" -b "$DEV_A"
wait_for "$DEV_B" -b "$DEV_B"

NR=$($CTL list | grep -c '"dev_id"')
[ "$NR" = 2 ] || fail "expected 2 devices, got $NR"

echo "E2E-VM: double-attach of the same backing must be rejected"
$CTL add -f /a.img 2>/dev/null && fail "double attach was accepted"

echo "E2E-VM: add with an in-use explicit id must fail without killing the device"
dd if=/dev/zero of=/z.img bs=1M count=1 seek=7 2>/dev/null || fail "create z.img"
$CTL add -f /z.img -n "$ID_A" 2>/dev/null && fail "explicit-id add over a live device was accepted"
[ -b "$DEV_A" ] || fail "device A vanished after the rejected explicit-id add"

echo "E2E-VM: per-device tracking is independent"
dd if=/dev/urandom of=$DEV_A bs=4096 count=1 2>/dev/null || fail "write A"
dd if=/dev/urandom of=$DEV_B bs=1M count=2 seek=10 2>/dev/null || fail "write B"
sync
NR=$($CTL dump -n "$ID_A" | grep -c '"offset"')
[ "$NR" = 1 ] || fail "device A: expected 1 dirty range, got $NR"
NR=$($CTL dump -n "$ID_B" | grep -c '"offset"')
[ "$NR" = 1 ] || fail "device B: expected 1 dirty range, got $NR"
OFF=$($CTL dump -n "$ID_B" | jget offset)
[ "$OFF" = $((10 * 1024 * 1024)) ] || fail "device B dirty offset: got $OFF"

echo "E2E-VM: devices can be addressed by backing path (-f)"
ID=$($CTL status -f /b.img | jget dev_id)
[ "$ID" = "$ID_B" ] || fail "status -f /b.img returned dev_id $ID, want $ID_B"
$CTL dump -f /a.img >/dev/null || fail "dump -f /a.img"

echo "E2E-VM: checkpoint --all"
NR=$($CTL checkpoint --all | grep -c '"closed_era"')
[ "$NR" = 2 ] || fail "checkpoint --all covered $NR devices"
[ -f /a.meta ] || fail "a.meta not saved on checkpoint"
[ -f /b.meta ] || fail "b.meta not saved on checkpoint"

echo "E2E-VM: era-2 write on A only visible via --since 1"
dd if=/dev/urandom of=$DEV_A bs=64K count=1 seek=512 2>/dev/null || fail "era-2 write A"
sync
NR=$($CTL dump -n "$ID_A" --since 1 | grep -c '"offset"')
[ "$NR" = 1 ] || fail "device A since 1: expected 1 range, got $NR"
OFF=$($CTL dump -n "$ID_A" --since 1 | jget offset)
[ "$OFF" = $((32 * 1024 * 1024)) ] || fail "device A era-2 offset: got $OFF"
NR=$($CTL dump -n "$ID_B" --since 1 | grep -c '"offset"')
[ "$NR" = 0 ] || fail "device B since 1: expected 0 ranges, got $NR"

echo "E2E-VM: DISCARD is passed through and recorded in the era map"
# B has no changes since era 1 and offset 0 is untouched, so any dirty range
# there after a blkdiscard must be the discard itself (proves passthrough + era).
blkdiscard -o 0 -l $((64 * 1024)) $DEV_B || fail "blkdiscard rejected (discard not advertised?)"
NR=$($CTL dump -n "$ID_B" --since 1 | grep -c '"offset"')
[ "$NR" = 1 ] || fail "discard: expected 1 changed range since era 1, got $NR"
OFF=$($CTL dump -n "$ID_B" --since 1 | jget offset)
[ "$OFF" = 0 ] || fail "discard: changed-range offset expected 0, got $OFF"
# a region written non-zero must read back as zeros once discarded (hole punched)
dd if=/dev/urandom of=$DEV_B bs=64K count=1 seek=1 2>/dev/null || fail "discard: seed write"
blkdiscard -o $((64 * 1024)) -l $((64 * 1024)) $DEV_B || fail "blkdiscard (region 2) rejected"
sync
SUM=$(dd if=$DEV_B bs=64K count=1 skip=1 2>/dev/null | cksum | cut -d' ' -f1)
ZSUM=$(dd if=/dev/zero bs=64K count=1 2>/dev/null | cksum | cut -d' ' -f1)
[ "$SUM" = "$ZSUM" ] || fail "discarded region did not read back as zeros"

echo "E2E-VM: data integrity through ublk"
cmp $DEV_A /a.img || fail "device A content differs from backing"

echo "E2E-VM: detach A while B stays alive"
$CTL del -n "$ID_A" >/dev/null || fail "detach A"
dd if=/dev/urandom of=$DEV_B bs=4096 count=1 2>/dev/null || fail "B not writable after A detached"
NR=$($CTL list | grep -c '"dev_id"')
[ "$NR" = 1 ] || fail "expected 1 device after detach, got $NR"

echo "E2E-VM: re-attach A restores metadata"
ID_A=$($CTL add -f /a.img -g 64K --meta /a.meta --buffered | jget dev_id)
[ -n "$ID_A" ] || fail "re-attach a.img"
ERA=$($CTL status -n "$ID_A" | jget current_era)
[ "$ERA" -ge 2 ] || fail "era not restored from metadata (got $ERA)"
NR=$($CTL dump -n "$ID_A" --since 1 | grep -c '"offset"')
[ "$NR" = 1 ] || fail "era-2 dirty range lost across re-attach"

echo "E2E-VM: attach a third device at runtime"
dd if=/dev/zero of=/c.img bs=1M count=1 seek=15 2>/dev/null || fail "create c.img"
$CTL add -f /c.img --buffered >/dev/null || fail "attach c.img"
NR=$($CTL list | grep -c '"dev_id"')
[ "$NR" = 3 ] || fail "expected 3 devices, got $NR"

echo "E2E-VM: shutdown detaches everything"
$CTL shutdown >/dev/null || fail "shutdown"
i=0
while ls /dev/ublkb* >/dev/null 2>&1 && [ $i -lt 100 ]; do sleep 0.1; i=$((i + 1)); done
ls /dev/ublkb* >/dev/null 2>&1 && fail "devices still present after shutdown"

if [ -x /ublkera-go ]; then
    echo "E2E-VM: go implementation (libublksrv/cgo): attach and track"
    dd if=/dev/zero of=/g.img bs=1M count=1 seek=31 2>/dev/null || fail "create g.img"
    /ublkera-go -f /g.img -g 65536 -socket /tmp/go.sock &
    GO_PID=$!
    wait_for "go socket" -S /tmp/go.sock
    i=0
    while ! ls /dev/ublkb* >/dev/null 2>&1 && [ $i -lt 100 ]; do sleep 0.1; i=$((i + 1)); done
    GDEV=$(ls /dev/ublkb* 2>/dev/null | head -1)
    [ -n "$GDEV" ] || fail "go: ublk device did not appear"

    dd if=/dev/urandom of=$GDEV bs=4096 count=1 2>/dev/null || fail "go: write"
    sync
    NR=$(/ublkera-go -ctl dump -socket /tmp/go.sock | grep -c '"offset"')
    [ "$NR" = 1 ] || fail "go: expected 1 dirty range, got $NR"

    echo "E2E-VM: go implementation: checkpoint and era-2 diff"
    CLOSED=$(/ublkera-go -ctl checkpoint -socket /tmp/go.sock | jget closed_era)
    [ "$CLOSED" = 1 ] || fail "go: closed_era = $CLOSED"
    dd if=/dev/urandom of=$GDEV bs=4096 count=1 seek=256 2>/dev/null || fail "go: era-2 write"
    sync
    OFF=$(/ublkera-go -ctl dump -since 1 -socket /tmp/go.sock | jget offset)
    [ "$OFF" = $((1024 * 1024)) ] || fail "go: era-2 offset: got $OFF"
    cmp $GDEV /g.img || fail "go: device content differs from backing"

    echo "E2E-VM: go implementation: clean shutdown on SIGTERM"
    kill -TERM $GO_PID
    wait $GO_PID
    i=0
    while ls /dev/ublkb* >/dev/null 2>&1 && [ $i -lt 100 ]; do sleep 0.1; i=$((i + 1)); done
    ls /dev/ublkb* >/dev/null 2>&1 && fail "go: device still present after SIGTERM"
fi

echo "E2E-VM-PASS"
poweroff -f
