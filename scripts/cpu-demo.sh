#!/usr/bin/env bash
# マルチコア実験: ublkera のキュー(-q)ごとの IO スレッドが CPU にどう乗るかを
# fio + pidstat で観察する。バッキングは vm-ubuntu.sh が付ける virtio データ
# ディスク(/dev/disk/by-id/virtio-ublkera-data)。2デバイス実験用に GPT で
# 2分割して使う(初回のみ作成)。ゲスト内で root 実行。
set -euo pipefail

FIO_COMMON="--rw=randread --bs=4k --iodepth=32 --size=1G --direct=1 --time_based --runtime=12 --group_reporting"

jget() { python3 -c "import json,sys; print(json.load(sys.stdin)['$1'])"; }

DISK=$(readlink -f /dev/disk/by-id/virtio-ublkera-data)
if [ ! -b "${DISK}1" ]; then
    printf 'label: gpt\n,4GiB\n,\n' | sfdisk -q "$DISK"
    partprobe "$DISK"; sleep 1
fi
PA=${DISK}1
PB=${DISK}2

pgrep -f '[u]blkera daemon' >/dev/null || { ublkera daemon; sleep 1; }
DPID=$(pgrep -f '[u]blkera daemon' | paste -sd,)   # 複数残っていても pidstat に全部渡す
echo "daemon pid=$DPID / vCPU=$(nproc) / backing=$PA,$PB"

echo "backing 先頭 1GiB をランダムデータでプレフィル"
dd if=/dev/urandom of=$PA bs=1M count=1024 oflag=direct status=none
dd if=/dev/urandom of=$PB bs=1M count=1024 oflag=direct status=none

show_threads() {  # 8秒間のスレッド別 CPU%(1%超だけ表示)。CPU列 = 実行コア番号
    pidstat -t -p "$DPID" 8 1 | awk '/Command/ && !seen++ {print} $9+0 > 1 {print}'
}

fio_load() { # fio_load <dev> <numjobs> <name>
    fio --name="$3" --filename="$1" --numjobs="$2" $FIO_COMMON 2>&1 \
        | grep -E '^\s*read: IOPS' | sed "s/^ */  $3 /"
}

echo
echo "===== A: 1デバイス -q 1 (デフォルト) — キュー1本 = 1スレッド ====="
ID=$(ublkera add -f $PA | jget dev_id)
sleep 0.5
( sleep 2; show_threads ) &
fio_load /dev/ublkb$ID 4 "[fio numjobs=4]"
wait
ublkera del -n $ID >/dev/null

echo
echo "===== B: 同じデバイスを -q 4 — キュー4本 = 4スレッド ====="
ID=$(ublkera add -f $PA -q 4 | jget dev_id)
sleep 0.5
( sleep 2; show_threads ) &
fio_load /dev/ublkb$ID 4 "[fio numjobs=4]"
wait
ublkera del -n $ID >/dev/null

echo
echo "===== C: 2デバイス (各 -q 1) を同時に叩く — デバイス間はもともと並列 ====="
IDA=$(ublkera add -f $PA | jget dev_id)
IDB=$(ublkera add -f $PB | jget dev_id)
sleep 0.5
( sleep 2; show_threads ) &
fio_load /dev/ublkb$IDA 2 "[fio devA]" &
fio_load /dev/ublkb$IDB 2 "[fio devB]" &
wait
ublkera del -n $IDA >/dev/null
ublkera del -n $IDB >/dev/null

echo
echo EXPERIMENT-DONE
