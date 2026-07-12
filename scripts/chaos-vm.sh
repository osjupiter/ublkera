#!/usr/bin/env bash
# Rootless chaos runs: boot a QEMU VM with the host kernel and a minimal
# initramfs (busybox + ublkera + chaos + ublk_drv.ko) and run seeded chaos
# episodes as root INSIDE the guest (see src/bin/chaos.rs for what one
# episode does and which invariants it checks).
#   ./scripts/chaos-vm.sh                 # 5 episodes, seeds 1..5, 150 ops
#   SEED=42 EPISODES=20 OPS=300 ./scripts/chaos-vm.sh
set -euo pipefail

cd "$(dirname "$0")/.."
BIN=${BIN:-target/release/ublkera}
CHAOS=${CHAOS:-target/release/chaos}
[ -x "$BIN" ] && [ -x "$CHAOS" ] || { echo "build first: cargo build --release"; exit 1; }

SEED=${SEED:-1}
EPISODES=${EPISODES:-5}
OPS=${OPS:-150}

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
QEMU_TIMEOUT=${QEMU_TIMEOUT:-900}
if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
    KVM_ARGS="-enable-kvm -cpu host"
else
    echo "note: /dev/kvm not accessible, using TCG (slow)"
fi

WORK=$(mktemp -d /tmp/ublkera-chaos-vm.XXXXXX)
trap 'rm -rf "$WORK"' EXIT

echo "== build initramfs (busybox + ublkera + chaos + ublk_drv.ko) =="
IR="$WORK/initramfs"
mkdir -p "$IR"/{bin,dev,proc,sys}
cp "$BUSYBOX" "$IR/bin/busybox"
cp "$BIN" "$IR/ublkera"
cp "$CHAOS" "$IR/chaos"
cp scripts/chaos-guest-init.sh "$IR/init"
chmod +x "$IR/init" "$IR/ublkera" "$IR/chaos"

for bin in "$BIN" "$CHAOS"; do
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

echo "== boot VM: $EPISODES episodes, seeds $SEED.., $OPS ops each =="
LOG="$WORK/console.log"
set +e
timeout "$QEMU_TIMEOUT" qemu-system-x86_64 \
    $KVM_ARGS -m 1024 -smp 2 -nographic -no-reboot \
    -kernel "$KERNEL" -initrd "$WORK/initrd.img" \
    -append "console=ttyS0 rdinit=/init panic=-1 loglevel=4 chaos_seed=$SEED chaos_episodes=$EPISODES chaos_ops=$OPS" \
    </dev/null 2>&1 | tee "$LOG" | grep --line-buffered "CHAOS"
QRC=${PIPESTATUS[0]}
set -e

if grep -q "CHAOS-VM-PASS" "$LOG"; then
    echo
    echo "ALL CHAOS EPISODES PASSED"
elif grep -q "CHAOS-VM-FAIL" "$LOG"; then
    echo; echo "---- last guest console lines ----"; tail -40 "$LOG"
    fail "chaos failed: $(grep 'CHAOS-VM-FAIL' "$LOG" | head -1) (reproduce with the printed seed)"
else
    echo; echo "---- last guest console lines ----"; tail -40 "$LOG"
    [ "$QRC" -eq 124 ] && fail "VM timed out after ${QEMU_TIMEOUT}s"
    fail "VM exited (rc=$QRC) without a verdict"
fi
