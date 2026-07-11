# メモリ安全性とメモリ使用量

## 方針

ublkera は Rust で書かれており、`unsafe` は **カーネルとの境界に必要な
3 箇所だけ**に限定している。トラッキング本体(era マップ)、デバイス
レジストリ、制御プロトコルは 100% safe Rust で、コンパイラの所有権・
借用検査とatomic型により、データ競合・解放後使用・二重解放が型レベルで
排除されている。

## unsafe 箇所の棚卸し

すべて [target.rs](../src/target.rs) にあり、それ以外のモジュール
(era.rs / manager.rs / ctl.rs / main.rs)に `unsafe` はない。

### 1. ブロックデバイスサイズ取得の ioctl(`backing_size`)

`BLKGETSIZE64` / `BLKSSZGET` / `BLKPBSZGET` を `nix::ioctl_read!` 生成の
ラッパで呼ぶ。書き込み先はすべてローカル変数(`&mut u64` 等)への有効な
ポインタで、呼び出し完了後にポインタは保持されない。定型で健全。

### 2. バッキング fd への `fcntl(F_SETFL, O_DIRECT)`(`init_tgt`)

メモリを渡さない fd 操作。失敗してもバッファド IO にフォールバックする
だけで未定義動作はない。

### 3. io_uring SQE への生ポインタ(`make_sqe` / `handle_io_cmd`)

READ/WRITE の SQE にはバッファの生ポインタを渡す必要がある(カーネル
インターフェイスの制約。FLUSH / DISCARD はバッファを参照しない)。健全性は
次の所有構造で担保される:

- バッファ(`libublk::helpers::IoBuf`)は **キュー起動時に
  `dev.alloc_queue_io_bufs()` で depth 個まとめて確保**され、
  `Rc<Vec<IoBuf>>` として queue スレッドの `queue_fn` が**キューの生存期間
  ずっと**所有する(タグ = 添字)。ヒープ上の固定アドレスで、キューが
  落ちるまで drop されない
- IO パスは **1 スレッドのステートマシン**(`wait_and_handle_io` が CQE
  ごとに `handle_io_cmd` を呼ぶ)。あるタグの対象 IO を発行してから
  (BACKEND フェーズ)、その完了 CQE で再び `handle_io_cmd` が呼ばれるまでの
  間、**同じスレッドがそのバッファに触れるコードパスは存在しない**。
  カーネルがポインタを参照するのはまさにこの区間だけなので、使用中
  バッファへの同時アクセス・解放は起きない
- 長さは常に `IoBuf` の確保サイズ(`max_io_buf_bytes`)以下:ublk が
  渡してくる READ/WRITE は `max_sectors`(= `max_io_buf_bytes >> 9`)で
  制限されている(DISCARD はバッファを使わず、レンジ長のみを扱う)

バッファはタグ間で共有されず(1 タグ = 1 バッファ)、エイリアシングも
発生しない。

## データ競合フリーの根拠

- era マップは `Arc<EraState>` で共有され、可変状態は `AtomicU32` のみ。
  `&mut` での共有は存在せず、`unsafe impl Send/Sync` のような検査の
  回避も行っていない
- atomic の ordering は保守的に選んでいる(刻印: Acquire load +
  AcqRel `fetch_max`、checkpoint: AcqRel `fetch_add`)。競合時の意味論は
  [concurrency.md](concurrency.md) を参照
- デバイスレジストリは `Mutex<HashMap>`。ロックの取得規律(保持したまま
  join しない)も同ドキュメント参照

## 境界値の扱い

- **範囲外アクセス防止**: `mark_write` はチャンク添字を配列長で
  クランプし、そもそも ublk がデバイスサイズを超える IO を渡さない。
  era 配列アクセスは slice 経由で常に境界検査される
- **末尾の端数チャンク**: `ranges_since` が `len` をデバイスサイズで
  クランプするため、デバイス外を指すレンジは返らない
- **ゼロ長書き込み**: `mark_write` は `len == 0` を即 return
  (`offset + len - 1` のアンダーフローなし)
- **era カウンタの上限**: era は u32。約 42 億回の checkpoint で枯渇する
  (1 分に 1 回でも 8000 年)。オーバーフロー時の wrap は `fetch_max` の
  意味論を壊すため、実質的な運用上限として明記しておく

## メモリ使用量の見積もり

### era マップ

チャンクあたり 4 バイト(`AtomicU32`)。

```
必要メモリ ≒ デバイスサイズ / granularity × 4 バイト
```

| デバイスサイズ | granularity | era マップ |
|---|---|---|
| 100 GiB | 64 KiB | 6.25 MiB |
| 1 TiB | 64 KiB | 64 MiB |
| 1 TiB | 1 MiB | 4 MiB |
| 10 TiB | 1 MiB | 40 MiB |

大容量デバイスでは granularity を上げるとメモリと差分サイズ
(過大近似の粗さ)のトレードオフになる。メタデータファイルのサイズも
ほぼ同じ(ヘッダ 52 バイト + 4 バイト/チャンク)。

### IO バッファ

デバイスごとに:

```
queues × depth × buf_size   (既定: 1 × 64 × 512KiB = 32 MiB)
```

タグごとに `max_io_buf_bytes` を丸ごと確保するため、`-d`(depth)と
`-b`(buf_size)がそのまま常駐メモリに効く。多数のデバイスを
アタッチする場合はここが支配項になる(例: 既定値で 10 デバイス =
320 MiB)。小さくするなら `-b 256K` や `-d 32` を検討する。

### その他

- スレッド: デバイスごとに supervisor 1 本 + queue スレッド `queues` 本
  (それぞれ既定スタック)
- 制御プレーン(レジストリ、JSON 処理)は無視できる大きさ
