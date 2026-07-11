#!/usr/bin/env bash
# Full-image Ubuntu VM for interactive ublkera verification.
# Boots the official Ubuntu cloud image under QEMU (rootless, user-mode
# networking), installs linux-modules-extra so ublk_drv is available, and
# pushes the host-built ublkera binary + scripts into the guest.
#
#   ./scripts/vm-ubuntu.sh up        # download image, boot VM, push binary
#   ./scripts/vm-ubuntu.sh ssh       # log in (or: ssh -- <command>)
#   ./scripts/vm-ubuntu.sh push      # re-push a rebuilt binary/scripts
#   ./scripts/vm-ubuntu.sh test      # run scripts/e2e.sh inside the guest
#   ./scripts/vm-ubuntu.sh console   # follow the serial console log
#   ./scripts/vm-ubuntu.sh status    # is the VM running?
#   ./scripts/vm-ubuntu.sh down      # power off
#   ./scripts/vm-ubuntu.sh destroy   # power off and delete the VM disk
#
# Tunables (env): UBUNTU_SERIES=noble SSH_PORT=2222 MEM=2048 CPUS=2 DISK=16G VM_STATE=.vm
set -euo pipefail

cd "$(dirname "$0")/.."
REPO=$PWD
STATE=${VM_STATE:-$REPO/.vm}
CACHE=${XDG_CACHE_HOME:-$HOME/.cache}/ublkera

UBUNTU_SERIES=${UBUNTU_SERIES:-noble}
SSH_PORT=${SSH_PORT:-2222}
MEM=${MEM:-2048}
CPUS=${CPUS:-2}
DISK=${DISK:-16G}

BASE_IMG=$CACHE/$UBUNTU_SERIES-server-cloudimg-amd64.img
BASE_URL=https://cloud-images.ubuntu.com/$UBUNTU_SERIES/current/$UBUNTU_SERIES-server-cloudimg-amd64.img
DISK_IMG=$STATE/disk.qcow2
SEED_IMG=$STATE/seed.img
SSH_KEY=$STATE/id_ed25519
PIDFILE=$STATE/qemu.pid
PORTFILE=$STATE/ssh-port
CONSOLE=$STATE/console.log

fail() { echo "FAIL: $*" >&2; exit 1; }

vm_pid() {
    [ -f "$PIDFILE" ] || return 1
    local pid
    pid=$(cat "$PIDFILE") 2>/dev/null || return 1
    [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null && echo "$pid"
}

ssh_opts() {
    local port
    port=$(cat "$PORTFILE" 2>/dev/null || echo "$SSH_PORT")
    echo "-i $SSH_KEY -p $port -o StrictHostKeyChecking=no \
          -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
          -o ConnectTimeout=5"
}

vm_ssh() { ssh $(ssh_opts) ubuntu@127.0.0.1 "$@"; }

pick_port() {
    # reuse the port of a running VM; otherwise take SSH_PORT or the next free one
    if vm_pid >/dev/null && [ -f "$PORTFILE" ]; then cat "$PORTFILE"; return; fi
    local p=$SSH_PORT
    while ss -ltn 2>/dev/null | grep -q ":$p "; do p=$((p + 1)); done
    echo "$p"
}

cmd_up() {
    command -v qemu-system-x86_64 >/dev/null || fail "qemu-system-x86_64 not installed"
    command -v qemu-img >/dev/null || fail "qemu-img not installed"
    command -v cloud-localds >/dev/null || fail "cloud-localds not installed (apt install cloud-image-utils)"

    if vm_pid >/dev/null; then
        echo "VM already running (pid $(vm_pid), ssh port $(cat "$PORTFILE"))"
        return 0
    fi

    mkdir -p "$STATE" "$CACHE"

    if [ ! -f "$BASE_IMG" ]; then
        echo "== download Ubuntu $UBUNTU_SERIES cloud image =="
        wget -O "$BASE_IMG.part" "$BASE_URL" || fail "download $BASE_URL"
        mv "$BASE_IMG.part" "$BASE_IMG"
    fi

    [ -f "$SSH_KEY" ] || ssh-keygen -q -t ed25519 -N "" -f "$SSH_KEY"

    if [ ! -f "$DISK_IMG" ]; then
        echo "== create VM disk (overlay on cloud image) and cloud-init seed =="
        qemu-img create -q -f qcow2 -b "$BASE_IMG" -F qcow2 "$DISK_IMG" "$DISK"
        cat > "$STATE/user-data" <<EOF
#cloud-config
hostname: ublkera-vm
ssh_pwauth: true
package_update: true
users:
  - name: ubuntu
    plain_text_passwd: ubuntu
    lock_passwd: false
    shell: /bin/bash
    sudo: ALL=(ALL) NOPASSWD:ALL
    ssh_authorized_keys:
      - $(cat "$SSH_KEY.pub")
write_files:
  - path: /etc/modules-load.d/ublk_drv.conf
    content: "ublk_drv\n"
runcmd:
  - [sh, -c, 'DEBIAN_FRONTEND=noninteractive apt-get install -y "linux-modules-extra-\$(uname -r)"']
  - [modprobe, ublk_drv]
EOF
        printf 'instance-id: ublkera-vm\nlocal-hostname: ublkera-vm\n' > "$STATE/meta-data"
        cloud-localds "$SEED_IMG" "$STATE/user-data" "$STATE/meta-data"
    fi

    # experiment data disk (virtio, host page cache bypassed for honest benches):
    # /dev/disk/by-id/virtio-ublkera-data in the guest
    DATA_IMG=$STATE/data.qcow2
    [ -f "$DATA_IMG" ] || qemu-img create -q -f qcow2 "$DATA_IMG" 8G

    local port kvm_args=""
    port=$(pick_port)
    echo "$port" > "$PORTFILE"
    if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
        kvm_args="-enable-kvm -cpu host"
    else
        echo "note: /dev/kvm not accessible, using TCG (very slow boot)"
    fi

    echo "== boot VM (ssh port $port, serial log: $CONSOLE) =="
    qemu-system-x86_64 \
        $kvm_args -m "$MEM" -smp "$CPUS" \
        -drive file="$DISK_IMG",if=none,id=os0,format=qcow2 \
        -device virtio-blk-pci,drive=os0,bootindex=0 \
        -drive file="$SEED_IMG",if=virtio,format=raw,readonly=on \
        -drive file="$DATA_IMG",if=none,id=data0,format=qcow2,cache=none,aio=io_uring \
        -device virtio-blk-pci,drive=data0,serial=ublkera-data \
        -netdev user,id=n0,hostfwd=tcp:127.0.0.1:$port-:22 \
        -device virtio-net-pci,netdev=n0 \
        -display none -daemonize \
        -serial "file:$CONSOLE" \
        -pidfile "$PIDFILE"

    echo "== wait for ssh =="
    local i=0
    until vm_ssh true 2>/dev/null; do
        i=$((i + 1))
        [ $i -lt 120 ] || fail "ssh did not come up in 10min (see $CONSOLE)"
        sleep 5
    done

    echo "== wait for cloud-init (installs linux-modules-extra on first boot) =="
    vm_ssh cloud-init status --wait >/dev/null || true
    vm_ssh test -e /dev/ublk-control || fail "/dev/ublk-control missing in guest (see $CONSOLE)"

    cmd_push
    echo
    echo "READY. Log in and play:"
    echo "  ./scripts/vm-ubuntu.sh ssh"
    echo "  sudo ~/ublkera/target/release/ublkera daemon"
}

cmd_push() {
    local bin=${BIN:-target/release/ublkera}
    [ -x "$bin" ] || bin=target/debug/ublkera
    [ -x "$bin" ] || fail "build first: cargo build --release"
    vm_pid >/dev/null || fail "VM not running (./scripts/vm-ubuntu.sh up)"

    echo "== push $bin and scripts/ into the guest (~/ublkera) =="
    # mirror the repo layout so scripts/e2e.sh works unchanged in the guest
    tar -cf - scripts "$bin" | vm_ssh 'mkdir -p ublkera && tar -xf - -C ublkera'
    vm_ssh 'sudo install -m 755 ~/ublkera/target/*/ublkera /usr/local/bin/ublkera' 2>/dev/null || true
}

cmd_test() {
    cmd_push
    echo "== run e2e suite inside the guest =="
    vm_ssh -t 'cd ~/ublkera && sudo ./scripts/e2e.sh'
}

cmd_down() {
    local pid
    pid=$(vm_pid) || { echo "VM not running"; return 0; }
    echo "== power off =="
    vm_ssh 'sudo poweroff' 2>/dev/null || true
    local i=0
    while kill -0 "$pid" 2>/dev/null && [ $i -lt 60 ]; do sleep 1; i=$((i + 1)); done
    kill -0 "$pid" 2>/dev/null && { echo "graceful poweroff failed, killing qemu"; kill "$pid"; }
    rm -f "$PIDFILE"
}

cmd_destroy() {
    cmd_down
    rm -rf "$STATE"
    echo "VM state deleted ($STATE). Cached base image kept in $CACHE."
}

cmd_status() {
    if vm_pid >/dev/null; then
        echo "running: pid $(vm_pid), ssh -p $(cat "$PORTFILE") ubuntu@127.0.0.1"
    else
        echo "not running"
    fi
}

case "${1:-up}" in
    up)       cmd_up ;;
    ssh)      shift; [ "${1:-}" = "--" ] && shift; vm_ssh -t "$@" ;;
    push)     cmd_push ;;
    test)     cmd_test ;;
    console)  tail -f "$CONSOLE" ;;
    status)   cmd_status ;;
    down)     cmd_down ;;
    destroy)  cmd_destroy ;;
    *)        sed -n '2,17p' "$0"; exit 1 ;;
esac
