#!/usr/bin/env bash
# The SST sandwich for ublkera, rootless: boot a QEMU VM with the host kernel
# and run  workload → ublkera (SUT) → ublkfault (in-memory contract model)
# inside it. See scripts/sandwich-guest-init.sh for what is checked.
#
# ublkfault lives in this repo (ublkfault/, a workspace member), so a plain
# `cargo build --release` provides both binaries.
#
# All scenario files live in this repo (./scenarios/): the block-layer
# contract they encode (FLUSH forwarding, power-loss semantics, EIO
# propagation, discard) is exactly ublkera's passthrough duty.
set -euo pipefail

cd "$(dirname "$0")/.."
BIN=${BIN:-target/release/ublkera}
[ -x "$BIN" ] || { echo "build first: cargo build --release"; exit 1; }
FAULT_BIN=${FAULT_BIN:-target/release/ublkfault}
[ -x "$FAULT_BIN" ] || { echo "ublkfault not built: $FAULT_BIN (cargo build --release)"; exit 1; }

fail() { echo "FAIL: $*" >&2; exit 1; }
skip() { echo "SKIP: $*" >&2; exit 0; }

command -v qemu-system-x86_64 >/dev/null || skip "qemu-system-x86_64 not installed"
command -v cpio >/dev/null || skip "cpio not installed"

KREL=$(uname -r)
KERNEL="/boot/vmlinuz-$KREL"
[ -r "$KERNEL" ] || skip "$KERNEL not readable"

MOD=""
for m in "/lib/modules/$KREL/kernel/drivers/block/ublk_drv.ko" \
         "/lib/modules/$KREL/kernel/drivers/block/ublk_drv.ko.zst" \
         "/lib/modules/$KREL/kernel/drivers/block/ublk_drv.ko.xz"; do
    [ -f "$m" ] && { MOD=$m; break; }
done
[ -n "$MOD" ] || skip "ublk_drv module not found for $KREL"

BUSYBOX=""
for c in /bin/busybox /usr/bin/busybox /usr/lib/initramfs-tools/bin/busybox; do
    [ -x "$c" ] && file "$c" | grep -q "statically linked" && { BUSYBOX=$c; break; }
done
[ -n "$BUSYBOX" ] || skip "no statically linked busybox found (apt install busybox-static)"

KVM_ARGS=""
QEMU_TIMEOUT=${QEMU_TIMEOUT:-180}
if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
    KVM_ARGS="-enable-kvm -cpu host"
else
    echo "note: /dev/kvm not accessible, using TCG (slow boot)"
    QEMU_TIMEOUT=${QEMU_TIMEOUT:-900}
fi

WORK=$(mktemp -d /tmp/ublkera-sandwich.XXXXXX)
trap 'rm -rf "$WORK"' EXIT

echo "== build initramfs (busybox + ublkera + ublkfault + scenarios + ublk_drv.ko) =="
IR="$WORK/initramfs"
mkdir -p "$IR"/{bin,dev,proc,sys,scenarios}
cp "$BUSYBOX" "$IR/bin/busybox"
cp "$BIN" "$IR/ublkera"
cp "$FAULT_BIN" "$IR/ublkfault"
cp scripts/sandwich-guest-init.sh "$IR/init"
cp scenarios/*.scen "$IR/scenarios/"
chmod +x "$IR/init" "$IR/ublkera" "$IR/ublkfault"

for bin in "$BIN" "$FAULT_BIN"; do
    ldd "$bin" | grep -o '/[^ ]*' | sort -u | while read -r lib; do
        [ -f "$lib" ] || continue
        mkdir -p "$IR$(dirname "$lib")"
        cp "$lib" "$IR$lib"
    done
done

case "$MOD" in
    *.zst) command -v zstd >/dev/null || skip "zstd needed to unpack $MOD"
           zstd -d -q -c "$MOD" > "$IR/ublk_drv.ko" ;;
    *.xz)  xz -d -c "$MOD" > "$IR/ublk_drv.ko" ;;
    *)     cp "$MOD" "$IR/ublk_drv.ko" ;;
esac

(cd "$IR" && find . | cpio -o -H newc --quiet | gzip -1) > "$WORK/initrd.img"

echo "== boot VM and run the sandwich inside it =="
LOG="$WORK/console.log"
set +e
timeout "$QEMU_TIMEOUT" qemu-system-x86_64 \
    $KVM_ARGS -m 1024 -smp 2 -nographic -no-reboot \
    -kernel "$KERNEL" -initrd "$WORK/initrd.img" \
    -append "console=ttyS0 rdinit=/init panic=-1 loglevel=4" \
    </dev/null 2>&1 | tee "$LOG" | grep --line-buffered -E "SANDWICH-VM|SCENARIO"
QRC=${PIPESTATUS[0]}
set -e

if grep -q "SANDWICH-VM-PASS" "$LOG"; then
    echo
    echo "SANDWICH TESTS PASSED"
elif grep -q "SANDWICH-VM-FAIL" "$LOG"; then
    echo; echo "---- last guest console lines ----"; tail -30 "$LOG"
    fail "sandwich failed: $(grep 'SANDWICH-VM-FAIL' "$LOG" | head -1)"
else
    echo; echo "---- last guest console lines ----"; tail -30 "$LOG"
    [ "$QRC" -eq 124 ] && fail "VM timed out after ${QEMU_TIMEOUT}s"
    fail "VM exited (rc=$QRC) without a verdict"
fi
