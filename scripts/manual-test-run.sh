#!/usr/bin/env bash
# docs/manual-test.md の手順をそのまま流す検証スクリプト(ゲスト内で root 実行)。
# 各ステップで実行するコマンドを「$ ...」として表示してから実行する。
set -euo pipefail

step() { echo; echo "===== $* ====="; }
run()  { printf '$ %s\n' "$*"; "$@"; }

modprobe ublk_drv
pgrep -f 'ublkera daemon' >/dev/null || run ublkera daemon

step "step 1-2: 空デバイス作成 + ublk 公開"
run truncate -s 64M /root/cbt-test.img
echo '$ ublkera add -f /root/cbt-test.img -g 64K --meta /root/cbt-test.meta'
ID=$(ublkera add -f /root/cbt-test.img -g 64K --meta /root/cbt-test.meta \
     | tee /dev/stderr \
     | python3 -c 'import json,sys; print(json.load(sys.stdin)["dev_id"])')
DEV=/dev/ublkb$ID
CHUNK=65536
NCHUNKS=$(( 64 * 1024 * 1024 / CHUNK ))
echo "-> dev_id=$ID DEV=$DEV チャンク=64KiB x $NCHUNKS"

step "step 3: 全体をランダムで埋める"
run dd if=/dev/urandom of=$DEV bs=1M count=64 oflag=direct status=progress
echo

hash_chunks() {   # フルリード: drop_caches してから 64KiB ごとに sha256
    echo 3 > /proc/sys/vm/drop_caches
    python3 - "$DEV" <<'EOF'
import hashlib, sys
CHUNK = 65536
with open(sys.argv[1], "rb") as f:
    i = 0
    while (b := f.read(CHUNK)):
        print(i, hashlib.sha256(b).hexdigest())
        i += 1
EOF
}

step "step 4: フルリードでチャンクごとのハッシュ (before)"
echo '$ echo 3 > /proc/sys/vm/drop_caches   # ページキャッシュを捨ててから読む'
echo '$ hash_chunks > /root/hash.before     # 64KiB ごとに sha256、「<番号> <ハッシュ>」形式'
hash_chunks > /root/hash.before
echo "-> チャンク数 = $(wc -l < /root/hash.before)、先頭3チャンク:"
run head -3 /root/hash.before

step "step 5: checkpoint (初期 era を閉じる)"
run ublkera checkpoint -n $ID

step "step 6: 20 チャンクにだけランダムライト"
echo "\$ shuf -i 0-$(( NCHUNKS - 1 )) -n 20 | sort -n > /root/written-chunks.txt"
shuf -i 0-$(( NCHUNKS - 1 )) -n 20 | sort -n > /root/written-chunks.txt
echo "-> 対象チャンク = $(paste -sd, /root/written-chunks.txt)"
# granularity 64KiB = 4KiB×16: チャンク i の先頭 = i*16、+RANDOM%16 でチャンク内のランダム位置。
# set -x (PS4='$ ') が展開済みの dd を1行ずつ表示する。
for i in $(< /root/written-chunks.txt); do
    ( PS4='$ '; set -x; dd if=/dev/urandom of=$DEV bs=4K count=1 seek=$(( i*16 + RANDOM%16 )) oflag=direct conv=notrunc status=none )
done

step "step 7: checkpoint (差分を凍結)"
run ublkera checkpoint -n $ID

step "step 8-9: 再ハッシュして変更チャンクを求める"
echo '$ hash_chunks > /root/hash.after'
hash_chunks > /root/hash.after
echo "\$ paste /root/hash.before /root/hash.after | awk '\$2 != \$4 {print \$1}' > /root/diff-hash.txt"
paste /root/hash.before /root/hash.after \
  | awk '$2 != $4 { printf "hash : chunk %4d   %.12s… -> %.12s…\n", $1, $2, $4 > "/dev/stderr"
                    print $1 }' > /root/diff-hash.txt
echo "-> ハッシュが変わったチャンク数 = $(wc -l < /root/diff-hash.txt)"

step "step 10: era 差分をチャンクに展開して突き合わせ"
echo "\$ ublkera dump -n $ID --since 1   # レンジ(バイト単位)をチャンク番号に展開して /root/diff-era.txt へ"
ublkera dump -n $ID --since 1 | tee /root/dump.json | python3 -c '
import json, sys
CHUNK = 65536
for r in json.load(sys.stdin)["ranges"]:
    off, ln = r["offset"], r["len"]
    lo, hi = off // CHUNK, (off + ln) // CHUNK
    label = f"chunk {lo}" if hi - lo == 1 else f"chunk {lo}..{hi - 1}"
    print(f"era  : range offset={off:<9} len={ln:<7} -> {label}", file=sys.stderr)
    for i in range(lo, hi):
        print(i)
' > /root/diff-era.txt
echo "-> era 差分のチャンク数 = $(wc -l < /root/diff-era.txt)"

run diff /root/diff-hash.txt /root/diff-era.txt
run diff /root/written-chunks.txt /root/diff-era.txt
echo "MATCH: hash diff == era diff == written chunks"

step "cleanup"
run ublkera del -n $ID
run rm -f /root/cbt-test.img /root/cbt-test.meta /root/hash.before /root/hash.after \
      /root/diff-hash.txt /root/diff-era.txt /root/written-chunks.txt /root/dump.json
echo DONE
