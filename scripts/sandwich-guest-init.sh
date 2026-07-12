#!/bin/busybox sh
# Runs as /init inside the QEMU guest: the SST sandwich for ublkera.
#
#   workload  →  /dev/ublkb1 (SUT: ublkera)  →  /dev/ublkb0 (ublkfault)
#
# Layers of checks:
#   * /scenarios/  — declarative scenario files owned by this repo (see
#     scenarios/): the block-layer contract through the SUT (FLUSH
#     durability, power loss, flush lies, EIO, discard) plus CBT guards.
#     Device ids are deterministic: fault=0, SUT=1.
#   * shell below  — cases that need dynamic values or timing: dump
#     coverage of a crash-lost write, D-state hang of the backing
# Markers: "SANDWICH-VM-FAIL: ..." / "SANDWICH-VM-PASS".

/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs dev /dev 2>/dev/null
mkdir -p /tmp

fail() {
    echo "SANDWICH-VM-FAIL: $*"
    poweroff -f
}

jget() {
    grep -o "\"$1\": *[0-9]*" | head -1 | grep -o '[0-9]*$'
}

insmod /ublk_drv.ko || fail "insmod ublk_drv"
i=0
while [ ! -e /dev/ublk-control ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i + 1)); done

FSOCK=/tmp/fault.sock
FAULT="/ublkfault"
ERA="/ublkera --socket /tmp/era.sock"

echo "SANDWICH-VM: lower layer: ublkfault (32M in-memory model)"
$FAULT serve --size 32M --seed 7 --socket $FSOCK >/tmp/ready.json 2>/tmp/fault.log &
i=0
while ! grep -q '"bdev"' /tmp/ready.json 2>/dev/null && [ $i -lt 100 ]; do sleep 0.1; i=$((i + 1)); done
DEVF=$(grep -o '"bdev":"[^"]*"' /tmp/ready.json | cut -d'"' -f4)
[ -b "$DEVF" ] || fail "lower device did not appear ($(cat /tmp/fault.log))"

echo "SANDWICH-VM: SUT: ublkera on top of $DEVF (explicit id 1)"
$ERA daemon --foreground 2>/tmp/era.log &
i=0
while [ ! -S /tmp/era.sock ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i + 1)); done
ID=$($ERA add -f "$DEVF" -g 64K --meta /tmp/era.meta -n 1 | jget dev_id)
[ "$ID" = 1 ] || fail "SUT attach failed or unexpected id '$ID' ($(tail -3 /tmp/era.log))"
DEVS=/dev/ublkb1
i=0
while [ ! -b "$DEVS" ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i + 1)); done

WC=$(cat /sys/block/ublkb1/queue/write_cache 2>/dev/null)
[ "$WC" = "write back" ] || fail "SUT does not advertise a volatile cache (write_cache='$WC'): FLUSH is never delivered to it"

echo "SANDWICH-VM: contract scenarios through the SUT"
$FAULT scenario --socket $FSOCK --dev $DEVS /scenarios/*.scen || fail "contract scenario through SUT"

echo "SANDWICH-VM: a write lost by lower power loss must still appear in dump"
dd if=/dev/urandom of=/p1 bs=1M count=1 2>/dev/null
dd if=/dev/urandom of=/p2 bs=1M count=1 2>/dev/null
dd if=/p1 of=$DEVS bs=1M count=1 oflag=direct conv=fsync 2>/dev/null || fail "establish base p1"
ERA_CUR=$($ERA checkpoint -n 1 | jget closed_era)
dd if=/p2 of=$DEVS bs=1M count=1 oflag=direct 2>/dev/null || fail "write p2 through SUT"
$FAULT crash --socket $FSOCK --mode drop >/dev/null
dd if=$DEVS bs=1M count=1 iflag=direct 2>/dev/null | cmp - /p1 || fail "expected pre-crash content p1"
# the lost write's range must still be reported changed (over-report is fine,
# silence is not)
BYTES=$($ERA dump -n 1 --since "$ERA_CUR" | jget dirty_bytes)
[ -n "$BYTES" ] && [ "$BYTES" -ge 1048576 ] || fail "dump does not cover the lost write (dirty_bytes=$BYTES)"
dd if=/p1 of=$DEVS bs=4096 count=1 oflag=direct 2>/dev/null || fail "SUT unhealthy after lower crash"

# expects the write's 64K chunk (and nothing more) in `dump --since $2`
covers_chunk() { # covers_chunk <byte-offset-of-write> <since>
    CHUNK=$(($1 / 65536 * 65536))
    D=$($ERA dump -n 1 --since "$2")
    [ "$(echo "$D" | jget dirty_bytes)" = 65536 ] || return 1
    [ "$(echo "$D" | jget offset)" = "$CHUNK" ] || return 1
}

echo "SANDWICH-VM: an EIO'd write must still appear in dump"
CUR=$($ERA checkpoint -n 1 | jget closed_era)
$FAULT set --socket $FSOCK --error-pm 1000 >/dev/null
dd if=/p1 of=$DEVS bs=4096 count=1 oflag=direct seek=400 2>/dev/null && fail "write succeeded despite lower EIO"
$FAULT set --socket $FSOCK --error-pm 0 >/dev/null
covers_chunk $((400 * 4096)) "$CUR" || fail "EIO'd write missing from dump (silent false negative)"

echo "SANDWICH-VM: lower hang parks the SUT's backing IO; control plane stays alive"
CUR=$($ERA checkpoint -n 1 | jget closed_era)
$FAULT set --socket $FSOCK --hang-pm 1000 >/dev/null
dd if=/p1 of=$DEVS bs=4096 count=1 oflag=direct seek=500 2>/dev/null &
DDPID=$!
sleep 1
kill -0 $DDPID 2>/dev/null || fail "writer finished although the lower device is hung"
$ERA status -n 1 >/dev/null || fail "SUT control plane dead while backing IO is hung"
$ERA dump -n 1 >/dev/null || fail "dump dead while backing IO is hung"
$FAULT set --socket $FSOCK --hang-pm 0 >/dev/null
$FAULT thaw --socket $FSOCK --result ok >/dev/null
wait $DDPID || fail "thawed write failed through SUT"
covers_chunk $((500 * 4096)) "$CUR" || fail "hung-then-completed write missing from dump"

echo "SANDWICH-VM: a hung write released as EIO must also appear in dump"
CUR=$($ERA checkpoint -n 1 | jget closed_era)
$FAULT set --socket $FSOCK --hang-pm 1000 >/dev/null
dd if=/p1 of=$DEVS bs=4096 count=1 oflag=direct seek=600 2>/dev/null &
DDPID=$!
sleep 1
$FAULT set --socket $FSOCK --hang-pm 0 >/dev/null
$FAULT thaw --socket $FSOCK --result eio >/dev/null
wait $DDPID 2>/dev/null && fail "thaw eio: write reported success"
covers_chunk $((600 * 4096)) "$CUR" || fail "hung-then-EIO'd write missing from dump"

$ERA del -n 1 >/dev/null || fail "SUT detach"
echo "SANDWICH-VM-PASS"
poweroff -f
