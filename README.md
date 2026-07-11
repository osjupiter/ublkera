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

# 一覧・状態
sudo ./target/release/ublkera list
sudo ./target/release/ublkera status -n 0

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

コマンド: `add` `del` `list` `status` `checkpoint` `checkpoint_all` `dump` `shutdown`

## メタデータ永続化

`add --meta <path>` を指定すると、era マップを checkpoint 時・デタッチ時・
デーモン停止時にファイルへアトミックに保存し、次回 add 時に自動ロードします
(granularity とデバイスサイズが一致しない場合はエラー)。

注意: 保存は checkpoint / 正常なデタッチ時のみなので、デーモンや電源の
クラッシュ時は最後の checkpoint 以降の書き込み記録が失われます。クラッシュ後は
安全側に倒してフルバックアップを取り直してください(dm-era も同様の運用が前提)。

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

## Go による参考実装

[go/](go/) に、同じコンセプトを **Go + cgo + libublksrv(C ライブラリ)** で
実装した場合の参考例がある(1プロセス1デバイスの簡潔版、メタデータ形式は
Rust 版と互換)。`go/build.sh` でビルドでき、ビルド済みなら
`scripts/e2e-vm.sh` の VM テストに自動で含まれる。詳細は
[go/README.md](go/README.md)。

## ドキュメント

- [docs/dump-format.md](docs/dump-format.md) — dump が返すバイトレンジの形式と性質、増分バックアップでの使い方
- [docs/concurrency.md](docs/concurrency.md) — 競合安全性: 何をどう防ぎ、何を保証し、どこに限界があるか
- [docs/memory.md](docs/memory.md) — メモリ安全性(unsafe の棚卸し)とメモリ使用量の見積もり

## dm-era との違い

- 1デーモンで複数デバイスを管理し、実行中に対象を増減できる(dm-era はデバイス毎に dmsetup)
- メタデータは常時オンディスクではなくメモリ上(checkpoint 時に保存)。
  dm-era のようなクラッシュセーフ era メタデータではない
- checkpoint 中も IO をブロックしない。整合点が必要なら fsfreeze してから checkpoint
- WRITE_ZEROES 未対応(DISCARD は対応。バッキングの discard/hole-punch へパススルー)
