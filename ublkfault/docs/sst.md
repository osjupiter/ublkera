# ublk Sandwich Semi-deterministic Simulation Testing (SST)

ublkベースのブロックデバイス実装(target ublksrv)に対する、実カーネル・実FSを含んだ準決定的なシミュレーションテスト方針。

## 命名について

TigerBeetleやFDBの純粋なDST(Deterministic Simulation Testing)と異なり、実カーネル・実blk-mq・実FSを介するため完全な決定性は得られない。この性質を明示するため、以下の候補で呼び分ける。

- **SST (Semi-deterministic Simulation Testing)** — 素直な命名
- **RST (Reproducible Simulation Testing)** — "reproducible"を強調(完全再現ではなくbugの再現性を狙う意味で)
- **Sandwich Testing** — 構成を強調するニックネーム

以下では **SST** を用いる。

## 目的

- ublkでブロックデバイスを実装する際の、契約遵守と障害耐性を体系的に検証する
- 実カーネル・実FS・実アプリを走らせながら、契約が保証しない挙動(reorder, torn, lost, latency, error)を意図的に注入し、bugを叩き出す
- 見つかったbugをseedで再現し、回帰テストとして蓄積する

## 全体構成

```
[workload driver: fio / FS + app / custom]
        ↓
[fault ublk 上位側 (upper)]     ← 契約が保証しない範囲で意地悪な入力を投げる
        ↓
[target ublksrv (SUT)]           ← 試験対象。上下契約を仲介する
        ↓
[fault ublk 下位側 (lower)]     ← 内蔵in-memoryモデルが契約に基づく応答を返す
```

- 3つのublkプロセスは独立
- 上位/下位のfault ublkは**同一実装**で、設定によりロールを切替
- 下位fault ublkは外部backingを持たず、プロセス内メモリで committed / volatile cache / in-flight を明示的にモデル化する
- 外部backing(brd, tmpfs, 実デバイス)は本テストの対象外。大容量やLBA算術に関わる境界値テストは別系統で扱う

## 設計原則

### 1. 契約表を仕様書兼fault仕様書として使う

ブロック層が上位に対して結ぶ契約を明示的に列挙し、各項目について「保証する / 保証しない」を明記する。

- **保証しない項目** → 対応する障害モデルをfault ublkに実装
- **保証する項目** → シミュレータのinvariant(violation検出対象)

これにより契約表が fault injection仕様書 兼 invariant検証仕様書 として機能する。

### 2. 対称なfault ublk

上位側と下位側は同じバイナリで、ロール指定と設定のみが異なる。障害モデルは共通の語彙で表現する。

- latency, error, torn, reorder, lost, corruption, flush-lies, discard-content
- 上位側は「契約範囲内で意地悪な入力」、下位側は「契約が保証しない範囲での応答異常」として使う
- 上位ublkの独自価値は fio 単独では届かない **bio粒度のreorder/delay/split** と、実ワークロードのストリームへの **任意のFLUSH/DISCARD注入** を実現する点にある(後者を使う場合、FS管理領域への注入はFS invariantと両立しないため、op注入モードではFS invariantを切ってrawワークロードで使う)

### 3. 段階的決定性

完全な決定性は諦め、以下の層で決定性の粒度を分ける。

- **障害注入の意思決定は決定的** — seededな PRNG で全ての注入判断を再現可能に
- **障害注入のタイミングは半決定的** — 実カーネルのスケジューリングに依存
- **上位workloadの挙動は非決定的** — 実FSやappの内部挙動は制御外

「同じseedで同じbugが出る保証」は諦め、「同じseedを何度か回せば高確率で再現する」を目標とする。運用は再現性より **長時間soak**(seed範囲を回し続け、failing tupleを保存する)に軸足を置く。

### 4. 障害注入は宣言した契約の範囲外だけで行う

fault ublk自身も上位に対して契約を宣言する(logical_block_size, atomic_write_unit_*, discard semantics 等)。宣言した範囲内では**意地悪をしてはならない**(それをするとテストとして不当)。

- 例: atomic_write_unit_max = 0 を宣言 → 任意サイズのwriteをtornさせてよい
- 例: atomic_write_unit_max = 4KB を宣言 → 4KB以内alignedのwriteはtornさせてはならない

これは契約と障害注入の一貫性を保つための原則。

### 5. 実カーネルのノイズを許容する

3層の間には本物のblk-mqが挟まり、plugging・I/O scheduler・request mergingによる"実物のreorder"が起きる。これは決定性を下げるが、純粋in-processシミュレータでは得られない現実性の源泉。tradeoffを認めた上で活用する。

### 6. seed駆動 + 化石ファイル

- 通常は seed範囲を回すランダムテストとして運用
- CIで発見された failing seed は `known_seeds/` に記録して回帰テストとして永続化
- 特定シナリオが必要になったら明示的なパラメータ指定で再現できるCLIを用意

### 7. 下位fault ublkは契約モデルそのもの

下位fault ublkは外部backingに委譲せず、プロセス内で以下を明示的に持つ。

- **committed** — 確実に永続化された状態(LBA → bytes)
- **volatile cache** — FLUSHで確定を待つpending write群
- **in-flight** — ackを返すか保留中のwrite

この構造の上で:

- FLUSH受領時、seedに従い volatile cache の部分集合を committed に反映
- クラッシュシミュレーション時、volatile cache と in-flight から部分集合を選択して committed に反映、残りは破棄
- torn write は in-flight 登録時にbytesを部分書きに置き換えて表現
- read は committed と volatile cache をマージした値を返す

これは実ストレージ動作の operational model そのもので、ALICE / Ferrite の crash-consistency model と同じ枠組み。**backingを持たないのではなく、契約モデルがbackingを兼ねる**設計。

外部backing(brd, tmpfs等)を使う案もあるが、その場合カーネルblock layerが下位に挟まって決定性が下がるうえ、"意地悪な下位ublk"と"素直な実デバイス"のモデルが二重化する。SSTの目的には内蔵モデルが適する。

## クラッシュ面の区別

「クラッシュ」は2つの別物を指すので混同しない。

- **(a) SUTプロセスのクラッシュ** — SUT自身のメタデータ復旧・unclean検知の経路。下位モデルでは扱えない。SUTのプロセスをkillするか、SUT側のシミュレーション(クラッシュ点列挙のユニットテスト等)で扱う
- **(b) ストレージの電源断** — 下位fault ublkのcrashコマンドが扱う範囲。volatile cache / in-flight の部分集合だけがcommittedに残る。**プロセスは誰も死なない**ので、テアダウン競合のない安定したテストになる

また、注入の「方針」はseedで決まっても、crash時にvolatile cacheに何が入っているかはカーネルタイミング依存であり、「同じseed = 同じ状態」ではない(半決定性はこの層にも効く)。

## 障害モデル(fault ublkが注入するもの)

上位契約が保証しないカテゴリを網羅する。

**タイミング系**
- latency injection(一様・spike・特定LBA)
- queue depth上限までの詰め込み
- completion順序の意図的入れ替え

**エラー系**
- EIO / ENOSPC / EAGAIN 等の返却
- partial completion(bioの一部だけ成功)

**永続化系**
- torn write(logical block境界での分割書き)
- lost write(ack返却、backingに反映せず)
- flush lies(FLUSHをack返却のみで実行しない)
- FUA無視(FUAを通常writeとして扱う)
- reorder(pendingを並べ替えて永続化)

**内容系**
- bit-level corruption
- 隣接LBA巻き添え書き換え
- DISCARD後の内容選択(zero / old / random)

**構成系**
- クラッシュシミュレーション(ublksrv停止 → UBLK_F_USER_RECOVERY で再開)
- backingスナップショット + 部分反映によるcrash state生成

## Invariant(検証項目)

契約が保証すると宣言した項目に対応する。

- REQ_PREFLUSH受領後、以前ackしたwriteが全てbackingに反映されている
- REQ_FUAでackしたwriteが即座にbackingに反映されている
- 宣言したatomic write unit範囲内のwriteはtornしていない
- クラッシュリカバリ後、FSがmount可能かつfsck cleanである
- 上位アプリの永続化契約(DB WAL、KVSジャーナル等)が破られていない

各invariantはworkload driver側または独立チェッカで検証する。

## workload driver層

用途別に3系統を想定。

1. **契約直接叩き** — fioやカスタムbio generatorで、FS抜きで上位契約の網羅的テスト
2. **実FS経由** — ext4/xfs/btrfsをmountしたうえで fio / dbench / fsstress
3. **アプリ経由** — SQLite / RocksDB / PostgreSQL 等を走らせてクラッシュ後の整合性検証

段階的にレイヤーを増やして検証範囲を広げる。

## seedモデル

3つのプロセスそれぞれにseedを持たせる。全体の試験ケースは以下のtupleで表現する。

```
(seed_upper, seed_target, seed_lower, workload_spec, git_commit)
```

- CIでは全要素をランダム化して大量に回す
- failing caseはこのtupleごと保存
- replayは同じtupleを再投入

完全な再現は保証しないが、複数回のreplayでbugが十分な確率で現れることを目標とする。

## 実装の進め方(段階)

1. **passthrough ublk** — 素通し。ublkプロトコルとio_uring配管の確立
2. **latency + error注入** — 最小の障害モデル追加
3. **torn / reorder / lost** — 永続化系の障害モデル追加
4. **クラッシュ + recovery** — UBLK_F_USER_RECOVERY連携
5. **サンドイッチ配置** — target ublksrvを上下で挟むCI環境
6. **invariant checker** — workloadごとのチェッカ整備
7. **seed管理と回帰** — known_seedsの運用

各段階で見つかったbugをknown_seedsに固定しつつ次段階へ進む。

## この方式の位置づけ

既存手法との対比。

| 方式 | 実カーネル | 決定性 | bio粒度制御 | 忠実度 |
|------|-----------|--------|-------------|--------|
| TigerBeetle / FDB DST | ✗ | 完全 | N/A | 契約層に限定 |
| CrashMonkey + dm-log-writes | ✓ | replayのみ | ✓ | 高 |
| dm-flakey / dm-error | ✓ | なし | 限定的 | 中 |
| **ublk sandwich SST** | ✓ | 半決定的 | ✓ | 高 |

ublk sandwich SSTは「実カーネル・実FSを含んだ現実性」と「userspaceで自由に書ける柔軟性」の両立を狙う。完全な決定性を諦める代わりに、契約層より上のロジックにも障害を届かせられる。

## 想定される限界

- 完全な決定性は得られない。同じseedでも再現しないbugがある
- 3層のublkプロセスによる性能オーバーヘッド(内蔵in-memoryモデル前提でも実I/Oより遅い場合あり)
- ublkプロトコル自体のバグはこの構成では検出できない(別途fuzzing等が必要)
- 実カーネルのスケジューリング起因のraceは間接的にしか叩けない
- CPU memory model起因のバグはこの層では検出できない(Loom等の別ツールと併用)

## スコープ外

以下はSSTの目的とは別の関心事として、別系統のテストで扱う。

- **大容量ブロックデバイス特有の境界値バグ** — LBA算術のオーバーフロー、扱えるセクタ数の上限、非2冪サイズでの計算誤り等。brdや実デバイスを使った境界値テストが適する
- **性能特性の検証** — レイテンシ分布、スループット、CPU効率等は実デバイス上での測定が必要
- **ublkプロトコル自身の実装バグ** — syzkallerや専用fuzzerの守備範囲

## 補完すべき他の検証

SSTだけでは不十分な範囲を補うため以下も併用する。

- 並行データ構造の検証: Loom / miri / TSan
- ublkプロトコル層のfuzzing: syzkaller / 独自fuzzer
- 実機性能・耐久テスト: 実NVMe上の長時間run
- 形式的仕様: 契約表のFerrite風モデル化(可能な範囲で)
