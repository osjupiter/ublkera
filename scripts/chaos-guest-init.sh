#!/bin/busybox sh
# Runs as /init inside the QEMU guest (host kernel + minimal initramfs):
# seeded chaos episodes against a real ublk device (see src/bin/chaos.rs).
# Verdict via serial console markers: "CHAOS-VM-FAIL: ..." / "CHAOS-VM-PASS".
# Parameters come in on the kernel command line: chaos_seed / chaos_episodes /
# chaos_ops.

/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs dev /dev 2>/dev/null
mkdir -p /tmp

fail() {
    echo "CHAOS-VM-FAIL: $*"
    poweroff -f
}

insmod /ublk_drv.ko || fail "insmod ublk_drv"
i=0
while [ ! -e /dev/ublk-control ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i + 1)); done
[ -e /dev/ublk-control ] || fail "/dev/ublk-control did not appear"

SEED=1
EPISODES=5
OPS=150
for w in $(cat /proc/cmdline); do
    case "$w" in
        chaos_seed=*) SEED=${w#*=} ;;
        chaos_episodes=*) EPISODES=${w#*=} ;;
        chaos_ops=*) OPS=${w#*=} ;;
    esac
done

# --buffered: the backing file lives on the initramfs rootfs (ramfs), which
# has no O_DIRECT. The device itself is still opened O_DIRECT by chaos.
n=0
while [ $n -lt $EPISODES ]; do
    s=$((SEED + n))
    echo "CHAOS-VM: episode $((n + 1))/$EPISODES (seed=$s, ops=$OPS)"
    /chaos --ublkera /ublkera --dir "/tmp/chaos-$s" --seed "$s" --ops "$OPS" --buffered ||
        fail "episode seed=$s"
    n=$((n + 1))
done

echo "CHAOS-VM-PASS"
poweroff -f
