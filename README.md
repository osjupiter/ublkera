# ublkera

dm-era ライクな差分トラッキング(CBT)付き ublk ブロックデバイス。

1つの常駐デーモンが複数のトラッキング対象デバイスを管理します。バッキング
ファイル/ブロックデバイスへ IO をパススルーしつつ、書き込まれたチャンクを
「era(世代)」単位で記録。checkpoint で era を進め、`dump --since <era>` で
「その checkpoint 以降に変更されたバイトレンジ」を JSON で取り出せます。
CBT 対象はデーモン実行中に自由に増減できます。

## アーキテクチャ

```
ublkera daemon (常駐、制御ソケット /run/ublkera/daemon.sock)
 ├─ デバイス0: supervisor thread ── /dev/ublkb0 ⇄ a.img   (eraマップ + meta)
 ├─ デバイス1: supervisor thread ── /dev/ublkb1 ⇄ b.img   (eraマップ + meta)
 └─ ... add/del で実行中に増減
```

- デバイス1個 = 専用スーパーバイザスレッド。libublk の制御/キュー io_uring は
  すべて thread-local なので、1プロセス内で複数デバイスが完全に独立して動く
- 各デバイスはチャンク(既定 64KiB、`--granularity`)ごとに「最後に書き込まれた
  era 番号」を持つ(dm-era と同じモデル)
- era の刻印はバッキングデバイスでの書き込み完了時に `fetch_max` で行うため、
  checkpoint と競合しても「取りこぼさない」方向に安全
- 外部ツールで ublk デバイスが消された場合もスーパーバイザが検知してメタデータを
  保存し、レジストリから自動回収される

IO パスは libublk の同期(io_uring ステートマシン)実装で READ/WRITE/FLUSH/DISCARD
をサポート。FLUSH は fsync、DISCARD はバッキングへの hole-punch/discard に変換
(成功時は era マップ上「変更あり」として記録)。WRITE_ZEROES は未対応(EINVAL)。

## ビルド

```sh
cargo build --release
```

要件: Linux 6.x + `ublk_drv` モジュール、root。

## 使い方

```sh
sudo modprobe ublk_drv

# デーモン起動(既定でバックグラウンド化。--foreground でフォアグラウンド)
sudo ./target/release/ublkera daemon

# CBT対象を追加(実行中いつでも)→ /dev/ublkbN が生える
sudo ./target/release/ublkera add -f /path/to/a.img --meta /var/lib/ublkera/a.meta
# => {"ok":true,"dev_id":0,"bdev":"/dev/ublkb0",...}
sudo ./target/release/ublkera add -f /dev/sdX --meta /var/lib/ublkera/sdX.meta

# 一覧・状態(-n <dev_id> の代わりに -f <バッキングパス> でも指定できる)
sudo ./target/release/ublkera list
sudo ./target/release/ublkera status -n 0
sudo ./target/release/ublkera status -f /path/to/a.img

# フルバックアップ直後に checkpoint(閉じた era 番号を控える)
sudo fsfreeze -f /mnt                      # 整合性が必要なら凍結してから
sudo ./target/release/ublkera checkpoint -n 0        # 1台だけ
sudo ./target/release/ublkera checkpoint --all       # 全対象一括
sudo fsfreeze -u /mnt
# => {"ok":true,"closed_era":1,"current_era":2,"meta_saved":true}

# era 1 の checkpoint 以降に変わった範囲だけ取り出す
sudo ./target/release/ublkera dump -n 0 --since 1
# => {"ok":true,"ranges":[{"offset":33554432,"len":65536}],"dirty_bytes":65536,...}
# ranges の offset/len で /dev/ublkb0 から読めば増分バックアップになる

# 対象から外す(メタデータ保存して /dev/ublkbN を削除。他デバイスは無影響)
sudo ./target/release/ublkera del -n 0

# 全デバイス切り離し + デーモン停止(SIGTERM/SIGINT でも同じ後始末をする)
sudo ./target/release/ublkera shutdown
```

制御は UNIX ソケット経由の1行 JSON プロトコルなので、CLI を使わず直接叩けます:

```sh
echo '{"cmd":"add","backing":"/path/to/c.img","granularity":65536}' \
  | socat - UNIX-CONNECT:/run/ublkera/daemon.sock
echo '{"cmd":"dump","dev_id":0,"since":1}' \
  | socat - UNIX-CONNECT:/run/ublkera/daemon.sock
```

コマンド: `add` `del` `list` `status` `checkpoint` `checkpoint_all` `dump` `shutdown`。
デバイス指定は `"dev_id": <n>` の代わりに `"backing": "<パス>"` でも可。

## 性能とチューニング

スレッドモデル: デバイス1個 = supervisor スレッド1本、その下に
**HWキュー1本につき専用の IO スレッド + io_uring** が立つ。デバイス間で
共有する実行資源はなく、era の刻印もチャンクごとのアトミック演算だけなので、
複数デバイスは何もしなくても別コアに散る。

1デバイスの IO 処理はキュー数分のスレッドに限られる(既定 `-q 1` なら
最大でも約1コア)。これは ublk の限界ではなく設定で、`add` 時にデバイス
単位で変えられる:

- `-q, --queues` — キュー数 = IO スレッド数(既定 1)。1デバイスの IOPS が
  CPU で頭打ちになるならコア数を上限に増やす。blk-mq が発行元 CPU ごとに
  キューを割り当てるため、複数コアから叩く負荷なら自然に分散する
- `-d, --depth` — キュー深さ(既定 64、キューあたりの in-flight 数)
- `-b, --buf-size` — 1リクエストの最大サイズ(既定 512K)。ublk はリクエスト
  ごとにこのバッファを常駐で事前確保するため、メモリ消費は
  デバイスあたり `queues × depth × buf_size`(既定 32MiB)。大きくした場合、
  実効リクエストサイズはブロック層のソフト上限
  `/sys/block/ublkbN/queue/max_sectors_kb`(既定 1280KiB)との min になる
  点に注意(HW 上限 `max_hw_sectors_kb` までは sysfs で引き上げ可能)

IO は 1:1 パススルー(ublk リクエスト1個 = バッキングへの io_uring 1発)で、
ublkera が IO を granularity 単位に分割・結合することはない。

### メモリ消費の見積もり

デバイスあたりの常駐メモリは2つの項の和:

```
IO バッファ: queues × depth × buf_size          (既定 1 × 64 × 512KiB = 32 MiB)
era マップ : デバイスサイズ / granularity × 4B  (メタデータファイルもほぼ同サイズ)
```

| デバイスサイズ | granularity | era マップ | IO バッファ(既定) | 合計目安 |
|---|---|---|---|---|
| 100 GiB | 64 KiB | 6.25 MiB | 32 MiB | ≈38 MiB |
| 1 TiB | 64 KiB | 64 MiB | 32 MiB | ≈96 MiB |
| 1 TiB | 1 MiB | 4 MiB | 32 MiB | ≈36 MiB |
| 10 TiB | 1 MiB | 40 MiB | 32 MiB | ≈72 MiB |

- IO バッファは `-q`/`-d`/`-b` にそのまま比例する(`-q 4` なら 128 MiB)。
  多数のデバイスを抱えるデーモンではここが支配項。ただしページが載るのは
  実際に IO で使われてからで、アイドルなら RSS には現れない
  (実測: 1 TiB を attach 直後の RSS 増分 ≈65 MiB = era マップ分のみ)
- era マップは granularity を上げれば線形に減るが、差分の粒度(過大近似の
  粗さ)とのトレードオフ
- 詳細(スレッドスタック等も含む)は [docs/memory.md](docs/memory.md)

参考実測(4 vCPU の QEMU ゲスト、バッキング brd(RAM ディスク、O_DIRECT)、
fio 4k randread。再現は VM 内で [scripts/cpu-demo.sh](scripts/cpu-demo.sh) を root 実行):

| 構成 | デーモン合計CPU | IOPS |
|---|---|---|
| 1デバイス `-q 1`(既定) | ≈63%(キュースレッド1本) | 67.5k |
| 1デバイス `-q 4` | ≈214%(4コアに分散) | 188k |
| 2デバイス(各 `-q 1`) | ≈121%(別コアに分離) | 53k + 53k |

バッキングが O_DIRECT 非対応(例: tmpfs 上のファイル)だと io_uring が
バッファード IO を io-wq カーネルワーカーに punt するため、iou-wrk-* スレッドが
現れて遅くもなる。RAM でベンチするなら tmpfs ではなく brd を使うこと。

## メタデータは揮発性(消えても安全)

設計方針: era マップは常にオンメモリで、永続化は「クリーンな再起動をまたいで
差分の連続性を保つための任意機能」にすぎない。マップが失われて壊れるのは
効率(次回がフルコピーになる)だけで、正しさは壊れない。ジャーナルや同期
書き込みのような複雑性を IO パスに持ち込まないための取り決め。

- **`--meta` なし(純オンメモリ)**: デタッチやデーモン再起動で履歴は消え、
  era は 1 から始まり直す。古いカーソルでの `dump --since N` は
  「since N is not in this device's era history」エラーになるので、履歴の
  消失に気づかず空差分を信じる事故は起きない(エラー = フルバックアップの合図)。
- **`--meta <path>` あり**: checkpoint 時・デタッチ時にアトミックに自動保存し、
  次回 add 時に自動ロード(granularity とデバイスサイズの不一致はエラー)。
  attach 中のファイルには「未クローズ」マークが付いており、クラッシュ
  (kill -9、電源断)後の add はこれを検知して**全チャンクを変更扱い**にする
  (応答に `"recovered_unclean": true`)。次の `dump` が全域を返すため、
  バックアップは自動的にフルコピーへ縮退する。

どちらのモードでも「クラッシュ後は人間がフルを取り直す」という運用ルールは
不要 — ツール側が次の差分としてフルコピーを要求してくる。

**カーソルの安全な持ち方**: era は履歴が作り直されると 1 から始まり直す小さな
自然数なので、バックアップ側のカーソルは **(generation, era) のペア**で保存する。
generation はトラッキング履歴ごとのランダムな 64bit(hex)で、add/status/dump の
応答に含まれる。`dump --since N --generation <hex>` と渡せば、履歴が入れ替わって
いた場合に空差分ではなくエラーが返る(per-chunk 側は u32 のままなのでメモリは
増えない)。`--since` が現 era 以上の場合も同様にエラー。

## テスト

```sh
cargo test                 # era マップのユニットテスト
./scripts/e2e-vm.sh        # ★root不要: QEMU VM内で実デバイス E2E 一式を実行
sudo ./scripts/e2e.sh      # ホスト上で直接実行する場合(root必要)
```

`e2e-vm.sh` はホストの root を使いません。ホストカーネル + busybox +
ublkera バイナリ + 展開した ublk_drv.ko だけの極小 initramfs を組み立てて
QEMU(KVM が使えれば KVM、なければ TCG)でブートし、ゲスト内の root で
[vm-guest-init.sh](scripts/vm-guest-init.sh) がE2E一式(デーモン起動、
複数デバイスの動的 attach/detach、独立トラッキング、checkpoint --all、
メタデータ復元、データ整合性)を実行、判定はシリアルコンソールの
PASS/FAIL マーカーで回収します。OSイメージのダウンロードは不要で、
実行時間は数秒〜十数秒です。

必要なもの: `qemu-system-x86_64`、静的リンクの busybox
(`apt install busybox-static`)、読み取り可能な `/boot/vmlinuz-$(uname -r)`。
KVM を使う場合は `kvm` グループ所属(なくても TCG で動作)。

### フルイメージの Ubuntu VM で対話的に検証する

busybox initramfs ではなく本物の Ubuntu(公式 cloud image)を起動して、
中にログインして手で ublkera を触りたい場合:

```sh
./scripts/vm-ubuntu.sh up        # イメージDL→起動→ublk_drv導入→バイナリ転送(初回は数分)
./scripts/vm-ubuntu.sh ssh       # ゲストにログイン(ubuntu/ubuntu、鍵は自動生成)
./scripts/vm-ubuntu.sh test      # ゲスト内で scripts/e2e.sh を実行
./scripts/vm-ubuntu.sh push      # ホストで再ビルドしたバイナリを再転送
./scripts/vm-ubuntu.sh down      # 停止(ディスクは保持、次回 up は速い)
./scripts/vm-ubuntu.sh destroy   # VM削除(cloud image のキャッシュは保持)
```

ホストの root は不要(user-mode ネットワーク + ssh ポートフォワード)。
ゲストは既定で Ubuntu 24.04(`UBUNTU_SERIES=noble`)。ホストでビルドした
バイナリをそのまま `~/ublkera/target/release/ublkera` と
`/usr/local/bin/ublkera` に配置します。cloud image のカーネルには
`ublk_drv` が含まれないため、初回ブート時に cloud-init が
`linux-modules-extra-$(uname -r)` を自動インストールします。
VM の状態は `.vm/`(git 管理外)、ベースイメージは `~/.cache/ublkera/`。

## Go による参考実装

[go/](go/) に、同じコンセプトを **Go + cgo + libublksrv(C ライブラリ)** で
実装した場合の参考例がある(1プロセス1デバイスの簡潔版、メタデータ形式は
Rust 版と互換)。`go/build.sh` でビルドでき、ビルド済みなら
`scripts/e2e-vm.sh` の VM テストに自動で含まれる。詳細は
[go/README.md](go/README.md)。

## ドキュメント

- [docs/manual-test.md](docs/manual-test.md) — 手動検証手順: フルリードのハッシュ差分と era 差分の突き合わせ
- [docs/dump-format.md](docs/dump-format.md) — dump が返すバイトレンジの形式と性質、増分バックアップでの使い方
- [docs/concurrency.md](docs/concurrency.md) — 競合安全性: 何をどう防ぎ、何を保証し、どこに限界があるか
- [docs/memory.md](docs/memory.md) — メモリ安全性(unsafe の棚卸し)とメモリ使用量の見積もり

## dm-era との違い

- 1デーモンで複数デバイスを管理し、実行中に対象を増減できる(dm-era はデバイス毎に dmsetup)
- メタデータは常時オンディスクではなくメモリ上(checkpoint 時に保存)。
  dm-era のようなクラッシュセーフ era メタデータではない
- checkpoint 中も IO をブロックしない。整合点が必要なら fsfreeze してから checkpoint
- WRITE_ZEROES 未対応(DISCARD は対応。バッキングの discard/hole-punch へパススルー)
