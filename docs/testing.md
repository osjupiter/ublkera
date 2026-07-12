# テスト戦略

守るべき不変条件はほぼ1つに集約される:

> カーソル (generation, era) のチェックポイント以降に書かれたブロックは、
> `dump --since` の結果に**必ず**含まれる(偽陰性ゼロ)。余分に dirty と
> 報告するのは許容(次のコピーが増えるだけ)。クラッシュ・メタデータ喪失
> 時は「正しい復元」か「全面 dirty / カーソル拒否 → フルコピー」のどちらか
> に必ず倒れる。

これをレイヤごとに別の手法で検証する。カーネル(ublk_drv)がデータパスに
挟まる以上、全系を単一スレッドで決定的にシミュレートする TigerBeetle 流の
DST は適用できないため、「決定的に列挙できる部分は列挙し、統合部分は実カー
ネルで揺らす」構成にしている。

## 1. ユニットテスト(`cargo test`、決定的)

`src/era.rs` のテスト群。境界(端数チャンク)、save/load の往復、並行
checkpoint との競合(`no_write_lost_across_concurrent_checkpoints`)に加え、
クラッシュ点の全列挙がある:

- `crash_at_every_byte_of_a_save_degrades_safely` — save は tmp 書き込み +
  rename なので、クラッシュが残せる状態は「旧ファイル + 切断された tmp
  (全プレフィックス)」だけ。その**全バイト位置**で復旧を実行し、必ず
  unclean 検知 → 全面 dirty に倒れることを確認する。VM の kill -9 では
  数マイクロ秒の窓を確率的にしか踏めないが、ここでは決定的に全列挙する。
- `truncated_metadata_never_loads` — 全長未満の全切断長でロードが必ず失敗
  する(短いチャンク配列で成功しない)。
- `corrupted_metadata_never_loads` — 外部要因のビット化け対策。メタデータ
  ファイル全体に CRC32 が掛かっており、全バイト位置の反転と末尾ゴミの付加
  それぞれでロードが必ず失敗することを確認する。破損したまま黙って使う
  (era 配列の化けによる過少報告)ことはなく、`add` は明示的なエラーになる。
  対処は「ファイルを消して新しい履歴を開始」(消費者は generation 不一致で
  フルコピーに倒れる)。

## 2. カオスハーネス(`src/bin/chaos.rs`)

シード付き PRNG から操作列を生成し、実 ublk デバイス(キュー数もシードから
1/2/4 を選ぶ)に対して書き込みバースト / checkpoint / graceful 再アタッチ /
**kill -9** / **IO 実行中の kill -9** / メタデータ削除をランダムに実行し
ながら、増分バックアップの消費者をシミュレートする。バックアップのたびに
2つを検査:

1. パススルー整合性 — デバイスを読み戻すと書いた通りである
2. CBT 契約 — 「dump の範囲だけコピーし、カーソルが拒否されたらフル
   コピー」という消費者のイメージが、実デバイスと完全一致する

「IO 実行中の kill -9」はデータパスの torn write を対象にした op で、
書き込みスレッド4本が流している最中にデーモンを殺す。契約は
「**完了が返った書き込みはバイト単位で必ず生存**」「宙に浮いた書き込みは
旧・新・途中切れのどれでもよい(通常のディスクの電源断と同じ)が、復旧後は
全面 dirty になるので次のバックアップは必ず正しい」の2点で、両方を検査する。

失敗時は同じシードで再現できる(IO は実カーネル経由なのでタイミングまでは
決定的でないが、操作列は決定的)。検証器自体の検出力はミューテーションで
確認済み(mark_write が 4K 書き込みを落とすよう故意に壊すと op 25 で検出)。

```
./scripts/chaos-vm.sh                        # ホストカーネル + initramfs で5エピソード
SEED=42 EPISODES=20 OPS=300 ./scripts/chaos-vm.sh
# noble VM 内なら: sudo ublkera-chaos --dir /root/chaos --seed 1 --ops 200
```

## 3. ブロック層ストレス(`scripts/blkstress.sh`)

データパスの検証は自作せず、定番の道具に任せる
(`./scripts/vm-ubuntu.sh stress` で noble VM 内で実行):

1. **fio --verify=crc32c** — 自己検証ペイロードで書いて読み戻す。
   シーケンシャル 1M(リクエスト分割を通過)と random 4k qd32
   (タグ/スロットの再利用)の2本。
2. **ext4 + fsck** — mkfs → 検証付き負荷 → fsck -fn。FLUSH の順序性の
   ような raw IO では踏みにくい経路はファイルシステムが一番よく踏む。
3. **dm-flakey を backing に挟む** — down 窓で書き込みを EIO にしながら
   連打し、**失敗した書き込みのブロックも dump に必ず現れる**ことを確認
   (失敗した書き込みでも媒体は部分的に変わりうるため、成功時のみ刻印
   すると偽陰性になる)。

さらに網羅的な適合性テストが欲しい場合はカーネル本家の
[blktests](https://github.com/osandov/blktests) を `TEST_DEVS=/dev/ublkbN`
で当てられる(セットアップが重いため常設はしていない)。

## 3.5 サンドイッチ(ublkfault)

**ストレージの電源断セマンティクス**(完了したが FLUSH 前の書き込みが
消える・順序が入れ替わる)は、デーモンの kill ではページキャッシュが
消えないため上の手法では踏めない。これはサブプロジェクト
[ublkfault/](../ublkfault/)(committed / volatile cache を明示的に持つ
インメモリ障害注入 ublk デバイス。workspace メンバーとして同時にビルド
される)で backing を差し替えるサンドイッチ構成で検証する:

```
dd(+fsync) → /dev/ublkbS(ublkera)→ /dev/ublkbF(ublkfault)
```

`./scripts/sandwich-vm.sh` が initramfs VM で検査する。プロセスを一切殺さない(crash は下位モデルへの
1コマンド)ので、kill 系より安定して回せる。

基本ケースは [scenarios/](../scenarios/) の**宣言的なシナリオファイル**で
記述してある(形式は ublkfault README 参照)。契約はパススルーする側=
ublkera の要件なので、ファイルはこのリポジトリが所有する:

- flush-durability / power-loss / flush-lies — fsync が ack した書き込みは
  下位の電源断後も SUT 越しに必ず残り、未 fsync は消えてよい
- eio — 下位のエラーは上に伝播する
- discard-passthrough — DISCARD の素通しとゼロ読み
- cbt-guards — 古い era / 別 generation のカーソル拒否

動的な値やタイミングが要るケースはシェル側。ビットマップ(era マップ)が
「変わったかもしれない範囲」を落とさないことを、失敗系それぞれで確認する:

- 電源断で backing から消えた完了済み書き込み → dump に必ず現れる
- EIO になった書き込み → dump に必ず現れる(該当チャンクちょうど)
- 宙吊り(D ステート)→ thaw で成功完了した書き込み → dump に現れる。
  宙吊り中も SUT の制御プレーン(status / dump)は生きている
- 宙吊り → thaw で EIO 完了した書き込み → それでも dump に現れる

このサンドイッチは実際にバグを2つ検出している:

1. **FLUSH が一切配送されていなかった** — `UBLK_ATTR_VOLATILE_CACHE` を
   公告しないとカーネルは write-through 扱いにして FLUSH を送らない。
   fsync は ack だけ返り backing に届かない(fio --verify では原理的に
   見えない)。
2. **ブロックデバイス backing で DISCARD が常に失敗** — fallocate の
   PUNCH_HOLE はブロックデバイスでは WRITE_ZEROES に変換され、非対応の
   backing では EOPNOTSUPP になる。現在は backing が実行できる場合のみ
   DISCARD を公告する(ファイル=常時、ブロック=write-zeroes 対応時)。

## 4. 既存の e2e / 手動テスト

- `./scripts/e2e-vm.sh` — 機能の e2e スイート(初期からあるもの。
  多デバイス、-f 指定、DISCARD、generation、クラッシュ復旧の各シナリオ)
- [manual-test.md](manual-test.md) — ハッシュ三者一致の手動検証手順
