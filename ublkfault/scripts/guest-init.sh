#!/bin/busybox sh
# Runs as /init inside the QEMU guest (host kernel + minimal initramfs):
# exercises ublkfault through the real kernel. Verdict via console markers
# "FAULT-VM-FAIL: ..." / "FAULT-VM-PASS".

/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs dev /dev 2>/dev/null
mkdir -p /tmp

fail() {
    echo "FAULT-VM-FAIL: $*"
    poweroff -f
}

insmod /ublk_drv.ko || fail "insmod ublk_drv"
i=0
while [ ! -e /dev/ublk-control ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i + 1)); done

SOCK=/tmp/fault.sock
CTL="/ublkfault"

echo "FAULT-VM: serve a 32M in-memory device"
$CTL serve --size 32M --seed 7 --socket $SOCK >/tmp/ready.json 2>/tmp/serve.log &
i=0
while ! grep -q '"bdev"' /tmp/ready.json 2>/dev/null && [ $i -lt 100 ]; do sleep 0.1; i=$((i + 1)); done
DEV=$(grep -o '"bdev":"[^"]*"' /tmp/ready.json | cut -d'"' -f4)
[ -b "$DEV" ] || fail "device did not appear (see serve.log: $(cat /tmp/serve.log))"

dd if=/dev/urandom of=/p1 bs=1M count=1 2>/dev/null
dd if=/dev/urandom of=/p2 bs=1M count=1 2>/dev/null

echo "FAULT-VM: read-your-writes before any flush"
dd if=/p1 of=$DEV bs=1M count=1 oflag=direct 2>/dev/null || fail "write p1"
dd if=$DEV bs=1M count=1 iflag=direct 2>/dev/null | cmp - /p1 || fail "readback != p1"

echo "FAULT-VM: power loss drops unflushed writes"
$CTL crash --socket $SOCK --mode drop >/dev/null || fail "crash cmd"
ZSUM=$(dd if=/dev/zero bs=1M count=1 2>/dev/null | md5sum | cut -d" " -f1)
SUM=$(dd if=$DEV bs=1M count=1 iflag=direct 2>/dev/null | md5sum | cut -d" " -f1)
[ "$SUM" = "$ZSUM" ] || fail "unflushed data survived a drop crash"

echo "FAULT-VM: FLUSH makes writes crash-durable"
dd if=/p1 of=$DEV bs=1M count=1 oflag=direct conv=fsync 2>/dev/null || fail "write p1 + fsync"
$CTL crash --socket $SOCK --mode drop >/dev/null || fail "crash cmd"
dd if=$DEV bs=1M count=1 iflag=direct 2>/dev/null | cmp - /p1 || fail "flushed data lost by crash"

echo "FAULT-VM: flush lies defeat fsync durability"
$CTL set --socket $SOCK --flush-lie-pm 1000 >/dev/null || fail "set flush-lie"
dd if=/p2 of=$DEV bs=1M count=1 oflag=direct conv=fsync 2>/dev/null || fail "write p2 + fsync"
$CTL crash --socket $SOCK --mode drop >/dev/null
dd if=$DEV bs=1M count=1 iflag=direct 2>/dev/null | cmp - /p1 || fail "flush lie: expected old data p1"
$CTL set --socket $SOCK --flush-lie-pm 0 >/dev/null

echo "FAULT-VM: EIO injection"
$CTL set --socket $SOCK --error-pm 1000 >/dev/null || fail "set error"
dd if=/p1 of=$DEV bs=4096 count=1 oflag=direct 2>/dev/null && fail "write succeeded despite error-pm=1000"
$CTL set --socket $SOCK --error-pm 0 >/dev/null

echo "FAULT-VM: hang puts the writer in D state; thaw ok releases it"
$CTL set --socket $SOCK --hang-pm 1000 >/dev/null || fail "set hang"
dd if=/p2 of=$DEV bs=4096 count=1 oflag=direct seek=100 2>/dev/null &
DDPID=$!
sleep 1
kill -0 $DDPID 2>/dev/null || fail "dd finished although it should be parked"
STATE=$(awk '{print $3}' /proc/$DDPID/stat 2>/dev/null)
[ "$STATE" = "D" ] || fail "parked writer is in state '$STATE', expected D"
$CTL set --socket $SOCK --hang-pm 0 >/dev/null
$CTL thaw --socket $SOCK --result ok >/dev/null || fail "thaw"
wait $DDPID || fail "thawed-ok write did not succeed"
head -c 4096 /p2 > /tmp/first4k
dd if=$DEV bs=4096 count=1 skip=100 iflag=direct 2>/dev/null | cmp - /tmp/first4k || fail "thawed write content wrong"

echo "FAULT-VM: hang then thaw eio fails the writer"
$CTL set --socket $SOCK --hang-pm 1000 >/dev/null
dd if=/p2 of=$DEV bs=4096 count=1 oflag=direct seek=200 2>/dev/null &
DDPID=$!
sleep 1
STATE=$(awk '{print $3}' /proc/$DDPID/stat 2>/dev/null)
[ "$STATE" = "D" ] || fail "second parked writer is in state '$STATE', expected D"
$CTL set --socket $SOCK --hang-pm 0 >/dev/null
$CTL thaw --socket $SOCK --result eio >/dev/null
wait $DDPID 2>/dev/null && fail "thaw eio: write reported success"

echo "FAULT-VM: discard reads back as zeros"
blkdiscard -o 0 -l 65536 $DEV || fail "blkdiscard"
ZSUM=$(dd if=/dev/zero bs=64K count=1 2>/dev/null | md5sum | cut -d" " -f1)
SUM=$(dd if=$DEV bs=64K count=1 iflag=direct 2>/dev/null | md5sum | cut -d" " -f1)
[ "$SUM" = "$ZSUM" ] || fail "discarded range not zero"

echo "FAULT-VM: status reports stats"
$CTL status --socket $SOCK | grep -q '"crashes": *3' || fail "status: crash count wrong"

echo "FAULT-VM: scenario runner self-test"
$CTL scenario --socket $SOCK --dev $DEV /scenarios/selftest.scen || fail "scenario self-test"

echo "FAULT-VM-PASS"
poweroff -f
