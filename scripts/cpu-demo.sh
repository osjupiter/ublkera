#!/usr/bin/env bash
# マルチコア実験: ublkera のキュー(-q)と CPU 負荷の関係を fio + pidstat で観察する。
# バッキングは tmpfs (RAM) にしてディスク律速を排除。ゲスト内で root 実行。
set -euo pipefail

FIO_COMMON="--rw=randread --bs=4k --iodepth=32 --direct=1 --time_based --runtime=12 --group_reporting"

jget() { python3 -c "import json,sys; print(json.load(sys.stdin)['$1'])"; }

pgrep -f 'ublkera daemon' >/dev/null || { ublkera daemon; sleep 1; }
DPID=$(pgrep -f 'ublkera daemon')
echo "daemon pid=$DPID / vCPU=$(nproc)"

echo "backing を tmpfs に用意 (256MiB x2, ランダムデータ)"
dd if=/dev/urandom of=/dev/shm/a.img bs=1M count=256 status=none
dd if=/dev/urandom of=/dev/shm/b.img bs=1M count=256 status=none

show_threads() {  # 8秒間のスレッド別 CPU%(1%超だけ表示)。CPU列 = 実行コア番号
    pidstat -t -p "$DPID" 8 1 | awk '/Command/ && !seen++ {print} $9+0 > 1 {print}'
}

fio_load() { # fio_load <dev> <numjobs> <name>
    fio --name="$3" --filename="$1" --numjobs="$2" $FIO_COMMON 2>&1 \
        | grep -E '^\s*read: IOPS' | sed "s/^ */  $3 /"
}

echo
echo "===== A: 1デバイス -q 1 (デフォルト) — キュー1本 = 1スレッド ====="
ID=$(ublkera add -f /dev/shm/a.img --buffered | jget dev_id)
sleep 0.5
( sleep 2; show_threads ) &
fio_load /dev/ublkb$ID 3 "[fio numjobs=3]"
wait
ublkera del -n $ID >/dev/null

echo
echo "===== B: 同じデバイスを -q 4 — キュー4本 = 4スレッド ====="
ID=$(ublkera add -f /dev/shm/a.img --buffered -q 4 | jget dev_id)
sleep 0.5
( sleep 2; show_threads ) &
fio_load /dev/ublkb$ID 4 "[fio numjobs=4]"
wait
ublkera del -n $ID >/dev/null

echo
echo "===== C: 2デバイス (各 -q 1) を同時に叩く — デバイス間はもともと並列 ====="
IDA=$(ublkera add -f /dev/shm/a.img --buffered | jget dev_id)
IDB=$(ublkera add -f /dev/shm/b.img --buffered | jget dev_id)
sleep 0.5
( sleep 2; show_threads ) &
fio_load /dev/ublkb$IDA 2 "[fio devA]" &
fio_load /dev/ublkb$IDB 2 "[fio devB]" &
wait
ublkera del -n $IDA >/dev/null
ublkera del -n $IDB >/dev/null

rm -f /dev/shm/a.img /dev/shm/b.img
echo
echo EXPERIMENT-DONE
