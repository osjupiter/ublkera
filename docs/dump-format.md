# dump のバイトレンジ形式

`ublkera dump -n <ID> [--since <ERA>]`(ソケット直叩きなら
`{"cmd":"dump","dev_id":N,"since":M}`)の出力仕様。

## 出力例

```json
{
  "ok": true,
  "dev_id": 0,
  "generation": "9f3b2a1c8d4e6f01",
  "current_era": 3,
  "since": 1,
  "granularity": 65536,
  "dirty_bytes": 196608,
  "ranges": [
    { "offset": 0,        "len": 65536  },
    { "offset": 10485760, "len": 131072 }
  ]
}
```

## フィールド

| フィールド | 型 | 意味 |
|---|---|---|
| `ok` | bool | 成否。`false` のときは代わりに `error`(文字列)が入る |
| `dev_id` | u32 | 対象デバイス。ブロックデバイスは `/dev/ublkb<dev_id>` |
| `generation` | hex文字列 | トラッキング履歴のランダム ID。era は履歴が作り直されると 1 から再開するため、カーソルは (generation, era) のペアで保存し、次回 `dump --generation <hex>` で渡すと履歴違いが空差分ではなくエラーになる |
| `current_era` | u32 | dump 時点で進行中の era |
| `since` | u32 | リクエストで指定したフィルタ値(そのまま返す) |
| `granularity` | u64 | トラッキングのチャンクサイズ(バイト)。attach 時の `-g` の値 |
| `dirty_bytes` | u64 | 全 `ranges` の `len` の合計 |
| `ranges` | array | 変更バイトレンジの配列(下記) |

## ranges の各要素

- `offset`: デバイス先頭からのバイトオフセット。IO は 1:1 パススルーなので
  **バッキングファイル/デバイスの同じオフセット**をそのまま指す
- `len`: バイト長

## ranges が満たす性質

1. **意味**: 「最後にデータが変更された(書き込み or discard)era が `since`
   より大きい(era > since)チャンク」の集合。`--since 0`(既定)は「一度でも
   変更された全チャンク」(未変更は era 0)
2. **チャンク粒度の過大近似**: 1 バイトの書き込み(や範囲の一部への discard)
   でもそのチャンク全体(`granularity` バイト)が dirty になる。**過小報告は起きない**
   (変更されたのに ranges に出ない、は仕様上ない。競合時の厳密な条件は
   [concurrency.md](concurrency.md) を参照)
3. **整列済み・重複なし**: `offset` 昇順で、レンジ同士は重ならない
4. **最大結合済み**: 隣接する dirty チャンクは 1 レンジにマージされる。
   したがって `offset` と `len` は `granularity` の倍数 — ただし
   **デバイス末尾の端数チャンクだけは例外**で、`len` がデバイスサイズで
   クランプされる(例: 64MiB+4KiB のデバイスでは最後のレンジが 4KiB になり得る)
5. マージされたレンジ内の各チャンクの era は同一とは限らない(すべて
   `> since` であることだけが保証)

## 典型的な使い方(増分バックアップ)

```text
1. フルバックアップを取得
2. checkpoint 実行 → closed_era = N を控える
3. (時間経過、書き込み発生)
4. 静止点が必要なら fsfreeze → checkpoint → unfreeze。closed_era = M
5. dump --since N
6. ranges の各 (offset, len) を /dev/ublkbX から読み、
   バックアップ先の同じ offset に書く
7. 次回は --since M で繰り返す
```

`--since` に渡すのは**基準にしたい checkpoint の `closed_era`**。
その checkpoint 以降の変更が返る。

## era 番号について

- era は 1 始まりの u32。checkpoint のたびに +1(`closed_era` = 直前まで
  進行していた era、`current_era` = 新しく開いた era = closed_era + 1)
- チャンクに記録されるのは「書き込み/discard 完了時点の current_era」
- era 0 は「未書き込み」の意味で予約
