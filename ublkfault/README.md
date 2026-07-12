# ublkfault

テスト用の障害注入 ublk ブロックデバイス(ublkera リポジトリの workspace
メンバー。現状ほぼ ublkera 専用の道具なので同居させている)。外部 backing を
持たず、**committed / volatile cache / in-flight** をプロセス内メモリで
明示的にモデル化し(ALICE / CrashMonkey と同じ operational model)、契約が
保証しない挙動を seed 駆動で注入する。ublk ベースのブロックデバイス実装
(SUT)を上下から挟んで試験する
[SST(準決定的シミュレーションテスト)](docs/sst.md) の下位レイヤ実装。

既存ツール(dm-flakey / dm-error / dm-delay)に対する主な優位:

- **EIO と D ステートの網羅的注入** — 「このリクエストだけ完了を返さない」
  を op 種別・確率・任意タイミングで制御できる。宙吊りにした IO は ctl で
  後から成功/EIO どちらでも解放できるので、「backing が刺さったまま SUT の
  制御プレーンは生きているか」を安定して試験できる(プロセスを殺さないので
  テアダウン競合がない)
- **電源断セマンティクス** — crash コマンドで volatile cache / in-flight の
  部分集合だけを committed に反映し、残りを破棄する。FLUSH 耐久性
  (「fsync が ack した書き込みは電源断後も必ず残る」)を検証できる
- **flush lies** — FLUSH を ack だけして実行しない、壊れたディスクの模倣

## 使い方

```sh
cargo build --release   # リポジトリルートで(workspace として一緒にビルドされる)

# 64MiB のインメモリデバイスを作る(フォアグラウンド、ready で JSON を出力)
sudo ../target/release/ublkfault serve --size 64M --seed 1 --socket /tmp/fault.sock

# 別端末から制御
ublkfault status --socket /tmp/fault.sock
ublkfault set    --socket /tmp/fault.sock --error-pm 500          # write の 50% を EIO
ublkfault set    --socket /tmp/fault.sock --hang-pm 100           # 10% を宙吊り(D ステート)
ublkfault thaw   --socket /tmp/fault.sock --result ok             # 宙吊りを成功で解放
ublkfault thaw   --socket /tmp/fault.sock --result eio            # または EIO で解放
ublkfault set    --socket /tmp/fault.sock --flush-lie-pm 1000     # FLUSH を全部嘘にする
ublkfault crash  --socket /tmp/fault.sock --mode drop             # 電源断: 揮発分を全ロスト
ublkfault crash  --socket /tmp/fault.sock --mode seeded           # 電源断: seed で部分反映
```

確率は千分率(pm)。注入の意思決定はすべて `--seed` の PRNG から決まる
(タイミングは実カーネル依存 — 準決定的。運用は再現より長時間 soak に寄せる)。

## セマンティクス

- 通常運転では契約完全準拠: 完了を返した write は read で必ず見える
  (committed + volatile のマージ)。torn / lost / reorder が観測されるのは
  **crash 経由のみ**(電源断で volatile の部分集合だけが残るのは契約の範囲内)
- volatile cache は容量(`--cache`)を超えると古い順に自動 writeback される
  (実ディスクのキャッシュ同様、「起動以来の全部が消える」ことはない)
- FLUSH は volatile 全量を committed へ(flush-lie 時は ack のみ)
- DISCARD / WRITE_ZEROES はゼロ書きとして volatile に入る(= crash で
  消えることもある)。WRITE_ZEROES を公告するのは、パススルー型 SUT が
  DISCARD を fallocate(PUNCH_HOLE) で転送するとカーネルが WRITE_ZEROES に
  変換するため(非対応だと EOPNOTSUPP)
- エラーにした write も **部分的に volatile に入りうる**(失敗した書き込みが
  媒体を変えている可能性の模倣)

## テスト

```sh
cargo test             # モデル単体(決定的): マージ・FLUSH 耐久・crash 部分反映・torn
./scripts/e2e-vm.sh    # root 不要: initramfs VM でカーネル越しに FLUSH 耐久性 /
                       # crash 揮発消失 / flush-lies / EIO / D ステート解放を検証

# サンドイッチ(workload → SUT → ublkfault)は SUT 側が所有する:
#   リポジトリルートの scripts/sandwich-vm.sh
```

サンドイッチ構成は「SUT が FLUSH を下に転送するか」「下位の EIO が上に
伝播するか」「下位が宙吊りでも SUT の制御プレーンが生きるか」を検査できる。
ublkera との初回サンドイッチで早速2つのバグを検出した:
`UBLK_ATTR_VOLATILE_CACHE` 非公告で **FLUSH が一切配送されていなかった**
(fio --verify では原理的に見えない: FLUSH の意味は「電源断をまたいで
残るか」なので、揮発性をモデル化した下位を挟まないと観測できない)、
および**ブロックデバイス backing での DISCARD が常に EOPNOTSUPP** だった。

## 宣言的シナリオ

契約テストは宣言的なシナリオファイルで記述できる
(`ublkfault scenario --dev <bdev> --socket <sock> file.scen ...`)。
シナリオが表す契約は SUT の要件なので、**ファイルは SUT のリポジトリが
所有する**(例: ublkera/scenarios/)。ここには verb 一巡の
[scenarios/selftest.scen](scenarios/selftest.scen) だけを置く。

```
# fsync が ack した書き込みは電源断後も必ず残る
write  off=0 len=1M pattern=durable fsync
crash  mode=drop
expect off=0 len=1M pattern=durable
```

パターン内容は名前から決定的に導出されるので、期待値は名前だけで書ける
(`pattern=zero` は全ゼロ)。verb は `write`(`fsync` フラグ可)/
`write-fails` / `expect` / `flush` / `crash` / `set` / `thaw` / `sleep` と、
エスケープハッチの `run` / `fail`(シェルコマンド、終了コードを検査)。
タイミングに依存するもの(D ステートの実測など)はシェルスクリプト側に
残す方針。詳細は [src/scenario.rs](src/scenario.rs) 冒頭のコメント。

## 段階(docs/sst.md の実装ロードマップ)

- [x] 1. in-memory モデル + パススルー(verify モード = 全 fault オフ)
- [x] 2. error(EIO)+ hang(D ステート)注入、ctl による解放
- [x] 3. torn / lost / reorder(crash 経由)+ flush lies
- [x] 4. サンドイッチ配置(SUT 側が所有: リポジトリルートの scripts/sandwich-vm.sh)
- [ ] 5. latency(タイマー駆動の自動解放)、per-LBA ターゲティング
- [ ] 6. 上位ロール(SUT に被せて reorder / FLUSH / DISCARD 注入。workload は
      実 FS ではなく IO ジェネレータを想定)
- [ ] 7. UBLK_F_USER_RECOVERY 連携
- [ ] 8. known_seeds/ の回帰運用
