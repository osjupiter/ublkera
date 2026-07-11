# 手動検証手順: フルリードのハッシュ差分と era 差分の突き合わせ

CBT(era トラッキング)が正しいことを、外部の独立した方法で確認する手順。
「実際に内容が変わったチャンク(フルリードのハッシュ比較)」と「ublkera が
差分として報告するチャンク(`dump --since`)」を突き合わせ、一致すれば
トラッキングは取りこぼしも過剰もない。

流れ:

1. 空のバッキングファイルを作る
2. ublk デバイスとして公開する
3. 全体をランダムデータで埋める
4. フルリードして各チャンク(= granularity、既定 64KiB)の SHA-256 を記録
5. checkpoint で era を進める(初期 era を閉じる)
6. 一部のチャンクにだけランダムライト
7. もう一度 checkpoint で era を進める(差分を凍結)
8. フルリードして再度チャンクごとのハッシュを記録
9. ハッシュが変わったチャンク一覧 = 実際に書き込まれたチャンク
10. `dump --since <初期era>` のレンジをチャンク番号に展開した一覧と diff → 一致で合格

root で実行する。実機でもよいが、[scripts/vm-ubuntu.sh](../scripts/vm-ubuntu.sh)
の Ubuntu VM 内が安全(`./scripts/vm-ubuntu.sh up && ./scripts/vm-ubuntu.sh ssh`)。

以下の全手順を(実行コマンドの表示付きで)そのまま流すスクリプトが
[scripts/manual-test-run.sh](../scripts/manual-test-run.sh)。VM 内なら:

```sh
./scripts/vm-ubuntu.sh ssh -- sudo bash ublkera/scripts/manual-test-run.sh
```

## 前提と注意

- **ハッシュのブロック長はデバイスの granularity と必ず一致させる**こと
  (この手順では両方 64KiB)。`dump` はチャンク単位でしか報告しないため、
  ブロック長がずれると突き合わせできない。
- 厳密には era 差分はハッシュ差分の**上位集合**になり得る: 同じ内容を
  上書きしても era 上は「書かれた」と記録される(ハッシュは変わらない)。
  この手順では書き込みデータが `/dev/urandom` なので、偶然一致する確率は
  無視でき、両者は一致するはず。
- 書き込みは `oflag=direct`、ハッシュ前に `drop_caches` している。ページ
  キャッシュ経由の読みと direct 書きが混ざると古いキャッシュを読んで
  偽の差分が出るため。

## 手順

以降すべて root(`sudo -i`)。

### 1〜2. 空デバイスを作って ublk で公開

```sh
modprobe ublk_drv
ublkera daemon        # 起動済みならそのまま

truncate -s 64M /root/cbt-test.img       # スパースな空ファイル(読み出しは全ゼロ)
ID=$(ublkera add -f /root/cbt-test.img -g 64K --meta /root/cbt-test.meta \
     | python3 -c 'import json,sys; print(json.load(sys.stdin)["dev_id"])')
DEV=/dev/ublkb$ID
CHUNK=65536
NCHUNKS=$(( 64 * 1024 * 1024 / CHUNK ))  # = 1024
ublkera status -n $ID                     # current_era: 1 (初期 era)
```

### 3. 全体をランダムデータで埋める

```sh
echo "fill: $DEV 全体 = ${NCHUNKS} チャンク × 64KiB をランダムで埋める"
dd if=/dev/urandom of=$DEV bs=1M count=64 oflag=direct status=progress
```

### 4. フルリードでチャンクごとのハッシュを記録

```sh
hash_chunks() {   # 「<チャンク番号> <sha256>」を1行ずつ出力
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

hash_chunks > /root/hash.before
wc -l < /root/hash.before                 # => 1024
head -3 /root/hash.before                 # 中身の例: 「0 3fc9b689459d...」
```

### 5. checkpoint で初期 era を閉じる

```sh
ublkera checkpoint -n $ID                 # => closed_era: 1, current_era: 2
```

以降の書き込みは era 2 として記録される。`--since 1` が「ここから後の差分」。

### 6. 一部のチャンクにだけランダムライト

20 チャンクを無作為に選び、各チャンク内のランダムな 4KiB 位置に
ランダムデータを書く(チャンクの一部しか書かなくても、そのチャンク全体が
「変更あり」と記録されることも同時に検証できる):

```sh
shuf -i 0-$(( NCHUNKS - 1 )) -n 20 | sort -n > /root/written-chunks.txt
for i in $(< /root/written-chunks.txt); do
    ( PS4='$ '; set -x; dd if=/dev/urandom of=$DEV bs=4K count=1 seek=$(( i*16 + RANDOM%16 )) oflag=direct conv=notrunc status=none )
done
```

`seek` は `bs=4K` 単位。granularity 64KiB = 4KiB×16 なので、チャンク `i` の
先頭が `i*16`、`+ RANDOM%16` でチャンク内のランダムな 4KiB 位置になる
(バイト位置 = `seek × 4096`)。`set -x` が実行される dd を展開済みの
`seek=` 実値付きで 1 行ずつ表示するので、その行をそのままコピーして
単発で叩いても同じ書き込みになる。

### 7. era を進めて差分を凍結

```sh
ublkera checkpoint -n $ID                 # => closed_era: 2, current_era: 3
```

### 8〜9. 再ハッシュし、変わったチャンクを求める

ハッシュが変わったチャンクを「変更前 → 変更後」付きで表示しつつ、
チャンク番号だけを `/root/diff-hash.txt` に落とす:

```sh
hash_chunks > /root/hash.after
paste /root/hash.before /root/hash.after \
  | awk '$2 != $4 { printf "hash : chunk %4d   %.12s… -> %.12s…\n", $1, $2, $4 > "/dev/stderr"
                    print $1 }' > /root/diff-hash.txt
echo "hash : 変更チャンク数 = $(wc -l < /root/diff-hash.txt)"
```

### 10. era 差分をチャンク番号に展開して突き合わせ

dump の生レンジ(バイト単位)とチャンク番号への対応をログに出しながら展開する:

```sh
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
echo "era  : 変更チャンク数 = $(wc -l < /root/diff-era.txt)"

diff /root/diff-hash.txt /root/diff-era.txt \
  && diff /root/written-chunks.txt /root/diff-era.txt \
  && echo "MATCH: hash diff == era diff == written chunks"
```

`MATCH` が出れば合格:

- **ハッシュ差分 = era 差分** — 報告された差分が過不足なく実変更と一致
- **書いたチャンク一覧とも一致** — 書き込み記録の観点でも取りこぼしなし

## 後片付け

```sh
ublkera del -n $ID
rm -f /root/cbt-test.img /root/cbt-test.meta /root/hash.before /root/hash.after \
      /root/diff-hash.txt /root/diff-era.txt /root/written-chunks.txt /root/dump.json
```

## 発展

- `-g` を 4K〜1M に変えて繰り返す(`CHUNK` と `hash_chunks` 内の定数も合わせる)
- 書き込みをチャンク境界をまたぐサイズ(例 `bs=128K`)にして、複数チャンクが
  マークされることを確認する
- 手順 6 のあと `ublkera del` → `add --meta` で再アタッチしてから 7 以降を続け、
  メタデータ永続化を挟んでも結果が変わらないことを確認する
