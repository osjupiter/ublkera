#!/usr/bin/env bash
# Rootless E2E: boot a QEMU VM with the host kernel and a minimal initramfs
# (busybox + ublkfault + ublk_drv.ko) and exercise the fault device through
# the real kernel: FLUSH durability, crash volatility, flush lies, EIO and
# D-state hangs. No root needed on the host (KVM used when accessible).
#   ./scripts/e2e-vm.sh
set -euo pipefail

cd "$(dirname "$0")/.."
# the workspace target dir sits at the repo root, one level up
BIN=${BIN:-../target/release/ublkfault}
[ -x "$BIN" ] || BIN=../target/debug/ublkfault
[ -x "$BIN" ] || { echo "build first: cargo build --release (at the repo root)"; exit 1; }

fail() { echo "FAIL: $*" >&2; exit 1; }
skip() { echo "SKIP: $*" >&2; exit 0; }

command -v qemu-system-x86_64 >/dev/null || skip "qemu-system-x86_64 not installed"
command -v cpio >/dev/null || skip "cpio not installed"

KREL=$(uname -r)
KERNEL="/boot/vmlinuz-$KREL"
[ -r "$KERNEL" ] || skip "$KERNEL not readable (fix: sudo chmod +r /boot/vmlinuz-*)"

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

WORK=$(mktemp -d /tmp/ublkfault-e2e.XXXXXX)
trap 'rm -rf "$WORK"' EXIT

echo "== build initramfs (busybox + ublkfault + ublk_drv.ko) =="
IR="$WORK/initramfs"
mkdir -p "$IR"/{bin,dev,proc,sys}
cp "$BUSYBOX" "$IR/bin/busybox"
cp "$BIN" "$IR/ublkfault"
cp scripts/guest-init.sh "$IR/init"
mkdir -p "$IR/scenarios"
cp scenarios/*.scen "$IR/scenarios/"
chmod +x "$IR/init" "$IR/ublkfault"

ldd "$BIN" | grep -o '/[^ ]*' | sort -u | while read -r lib; do
    [ -f "$lib" ] || continue
    mkdir -p "$IR$(dirname "$lib")"
    cp "$lib" "$IR$lib"
done

case "$MOD" in
    *.zst) command -v zstd >/dev/null || skip "zstd needed to unpack $MOD"
           zstd -d -q -c "$MOD" > "$IR/ublk_drv.ko" ;;
    *.xz)  xz -d -c "$MOD" > "$IR/ublk_drv.ko" ;;
    *)     cp "$MOD" "$IR/ublk_drv.ko" ;;
esac

(cd "$IR" && find . | cpio -o -H newc --quiet | gzip -1) > "$WORK/initrd.img"

echo "== boot VM and run the fault-device tests inside it =="
LOG="$WORK/console.log"
set +e
timeout "$QEMU_TIMEOUT" qemu-system-x86_64 \
    $KVM_ARGS -m 768 -smp 2 -nographic -no-reboot \
    -kernel "$KERNEL" -initrd "$WORK/initrd.img" \
    -append "console=ttyS0 rdinit=/init panic=-1 loglevel=4" \
    </dev/null 2>&1 | tee "$LOG" | grep --line-buffered "FAULT-VM"
QRC=${PIPESTATUS[0]}
set -e

if grep -q "FAULT-VM-PASS" "$LOG"; then
    echo
    echo "ALL FAULT-DEVICE TESTS PASSED"
elif grep -q "FAULT-VM-FAIL" "$LOG"; then
    echo; echo "---- last guest console lines ----"; tail -30 "$LOG"
    fail "guest test failed: $(grep 'FAULT-VM-FAIL' "$LOG" | head -1)"
else
    echo; echo "---- last guest console lines ----"; tail -30 "$LOG"
    [ "$QRC" -eq 124 ] && fail "VM timed out after ${QEMU_TIMEOUT}s"
    fail "VM exited (rc=$QRC) without a verdict"
fi
