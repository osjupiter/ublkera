#!/usr/bin/env bash
# End-to-end test for ublkera (daemon + dynamic attach/detach).
# Requires root (ublk control device).
#   sudo ./scripts/e2e.sh
set -euo pipefail

cd "$(dirname "$0")/.."
BIN=${BIN:-target/release/ublkera}
[ -x "$BIN" ] || BIN=target/debug/ublkera
[ -x "$BIN" ] || { echo "build first: cargo build --release"; exit 1; }

WORK=$(mktemp -d /tmp/ublkera-e2e.XXXXXX)
SOCK="$WORK/daemon.sock"
CTL="$BIN --socket $SOCK"

cleanup() {
    set +e
    $CTL shutdown 2>/dev/null
    sleep 0.5
    rm -rf "$WORK"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
jsonget() { python3 -c "import json,sys; print(json.load(sys.stdin)$1)"; }

modprobe ublk_drv 2>/dev/null || true
[ -e /dev/ublk-control ] || fail "/dev/ublk-control missing (ublk_drv not loaded?)"

echo "== start daemon =="
$BIN daemon --socket "$SOCK" --foreground &
DAEMON_PID=$!
for i in $(seq 1 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
[ -S "$SOCK" ] || fail "daemon socket did not appear"

echo "== attach two backing files while daemon runs =="
truncate -s 64M "$WORK/a.img"
truncate -s 32M "$WORK/b.img"
ADD_A=$($CTL add -f "$WORK/a.img" -g 64K --meta "$WORK/a.meta")
ADD_B=$($CTL add -f "$WORK/b.img" -g 64K --meta "$WORK/b.meta")
echo "$ADD_A"; echo "$ADD_B"
ID_A=$(echo "$ADD_A" | jsonget "['dev_id']")
ID_B=$(echo "$ADD_B" | jsonget "['dev_id']")
DEV_A=/dev/ublkb$ID_A
DEV_B=/dev/ublkb$ID_B
for i in $(seq 1 50); do [ -b "$DEV_A" ] && [ -b "$DEV_B" ] && break; sleep 0.1; done
[ -b "$DEV_A" ] || fail "$DEV_A missing"
[ -b "$DEV_B" ] || fail "$DEV_B missing"

NR=$($CTL list | jsonget "['devices'].__len__()")
[ "$NR" -eq 2 ] || fail "expected 2 devices, got $NR"

echo "== double-attach of same backing is rejected =="
$CTL add -f "$WORK/a.img" 2>/dev/null && fail "double attach should fail"

echo "== per-device tracking is independent =="
dd if=/dev/urandom of="$DEV_A" bs=4096 count=1 oflag=direct 2>/dev/null
dd if=/dev/urandom of="$DEV_B" bs=1M count=2 seek=10 oflag=direct 2>/dev/null
NR_A=$($CTL dump -n "$ID_A" | jsonget "['ranges'].__len__()")
NR_B=$($CTL dump -n "$ID_B" | jsonget "['ranges'].__len__()")
[ "$NR_A" -eq 1 ] || fail "device A: expected 1 range, got $NR_A"
[ "$NR_B" -eq 1 ] || fail "device B: expected 1 range, got $NR_B"
OFF_B=$($CTL dump -n "$ID_B" | jsonget "['ranges'][0]['offset']")
[ "$OFF_B" -eq $((10*1024*1024)) ] || fail "device B offset: got $OFF_B"

echo "== checkpoint --all =="
CP=$($CTL checkpoint --all)
echo "$CP"
NR=$(echo "$CP" | jsonget "['devices'].__len__()")
[ "$NR" -eq 2 ] || fail "checkpoint --all covered $NR devices"
[ -f "$WORK/a.meta" ] || fail "a.meta not saved on checkpoint"
[ -f "$WORK/b.meta" ] || fail "b.meta not saved on checkpoint"

echo "== era-2 writes on A only visible via --since 1 =="
dd if=/dev/urandom of="$DEV_A" bs=64K count=1 seek=512 oflag=direct 2>/dev/null
NR=$($CTL dump -n "$ID_A" --since 1 | jsonget "['ranges'].__len__()")
[ "$NR" -eq 1 ] || fail "device A since 1: expected 1 range, got $NR"
NR=$($CTL dump -n "$ID_B" --since 1 | jsonget "['ranges'].__len__()")
[ "$NR" -eq 0 ] || fail "device B since 1: expected 0 ranges, got $NR"

echo "== detach A while B stays alive =="
$CTL del -n "$ID_A"
[ -b "$DEV_B" ] || fail "device B died when A was detached"
NR=$($CTL list | jsonget "['devices'].__len__()")
[ "$NR" -eq 1 ] || fail "expected 1 device after detach, got $NR"
dd if=/dev/urandom of="$DEV_B" bs=4096 count=1 oflag=direct 2>/dev/null || fail "B not writable after A detached"

echo "== re-attach A: metadata restored (era >= 2, old ranges intact) =="
ADD_A=$($CTL add -f "$WORK/a.img" -g 64K --meta "$WORK/a.meta")
ID_A=$(echo "$ADD_A" | jsonget "['dev_id']")
ERA=$($CTL status -n "$ID_A" | jsonget "['current_era']")
[ "$ERA" -ge 2 ] || fail "era not restored from metadata (got $ERA)"
NR=$($CTL dump -n "$ID_A" --since 1 | jsonget "['ranges'].__len__()")
[ "$NR" -eq 1 ] || fail "era-2 dirty range lost across re-attach"

echo "== attach a third device on the fly (CBT target added at runtime) =="
truncate -s 16M "$WORK/c.img"
ID_C=$($CTL add -f "$WORK/c.img" | jsonget "['dev_id']")
NR=$($CTL list | jsonget "['devices'].__len__()")
[ "$NR" -eq 3 ] || fail "expected 3 devices, got $NR"

echo "== data integrity through ublk =="
sync /dev/ublkb"$ID_A"
cmp /dev/ublkb"$ID_A" "$WORK/a.img" || fail "device A content differs from backing"

echo "== shutdown detaches everything =="
$CTL shutdown
wait "$DAEMON_PID" 2>/dev/null || true
for i in $(seq 1 50); do [ ! -b "$DEV_B" ] && break; sleep 0.1; done
[ ! -b "$DEV_B" ] || fail "devices still present after shutdown"

echo
echo "ALL TESTS PASSED"
