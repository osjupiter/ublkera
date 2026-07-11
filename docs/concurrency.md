# 並行処理と競合安全性

ublkera が「どの競合をどう防いでいるか」「何を保証し、何を保証しないか」の仕様。

## 登場するスレッドと共有状態

```
ctl スレッド(1本)     : 制御ソケットの accept ループ。リクエストを1件ずつ直列処理
supervisor スレッド     : デバイスごとに1本。UblkCtrl と run_target を所有
queue スレッド          : デバイス×キューごとに libublk が生成。IO 処理の実体
シグナルスレッド        : SIGTERM/SIGINT で全デバイス切り離し

共有状態:
  EraState(era マップ) : Arc 共有。current_era: AtomicU32 / eras: Vec<AtomicU32>
  DeviceManager レジストリ: Mutex<HashMap<dev_id, Managed>>
  メタデータファイル      : tmp 書き込み + rename
```

## 1. era マップ(データプレーン)

チャンクの era 刻印はロックフリーで、以下の 2 操作だけで構成される:

- データを変える IO(WRITE / DISCARD)の完了時([target.rs](../src/target.rs)
  `handle_io_cmd`): `current_era` を **Acquire で load** → 該当チャンク範囲へ
  **AcqRel の `fetch_max`**([era.rs](../src/era.rs) `mark_write`)
- checkpoint([era.rs](../src/era.rs) `checkpoint`):
  `current_era` を **AcqRel の `fetch_add(1)`**

これにより:

- **複数 queue スレッドが同一チャンクへ同時に刻印しても壊れない**:
  `AtomicU32` なので torn write はなく、`fetch_max` なので「古い era で
  新しい era を上書きする」lost update も起きない
- **checkpoint 同士の競合**: `fetch_add` はアトミックなので、同時に 2 つ
  checkpoint が走っても必ず別々の `closed_era` を受け取る
- **競合時は必ず「新しい era」側に倒れる**: checkpoint と書き込みが競合した
  場合、チャンクは新しい era で刻印されうる。新しい era は「次回の差分に
  含まれる」方向なので、**差分の過大報告はあっても過小報告にはならない**
  (下記 3 の但し書きを除く)

## 2. 刻印と IO 完了応答の順序保証

刻印は「バッキングデバイスでのデータ変更(書き込み/discard)が完了した後、
**ublk へ完了を応答する前**」に行われる(`handle_io_cmd` の BACKEND フェーズ:
対象 IO の CQE 受領 → `mark_write` → `complete_io_cmd_unified`)。つまり:

> **保証**: 上位層(ファイルシステムやアプリ)に完了が見えた書き込み/discard
> は、その時点で必ず era マップに記録済みである。

したがって「fsfreeze などで IO を静止 → checkpoint」という手順を踏めば、
静止時点までの全書き込みが closed_era 以下の era で確実に記録されており、
`dump --since <前回のclosed_era>` に漏れなく現れる。

部分書き込み(short write)の場合は実際に書けたバイト数(`res`)分のみ
刻印される — 書けていない範囲を dirty 扱いしない、という意味でこれも
過小報告にはならない。DISCARD は成功時(`res == 0`)に**要求レンジ全体**を
刻印する(discard はデータをゼロ化する = 変更なので、dm-era 同様
「changed」として記録する)。

## 3. 静止させない checkpoint と in-flight 書き込み(消失なし)

IO を流したまま checkpoint した場合、**その瞬間に in-flight だった
(=完了応答前の)書き込み**が closed_era / 新 era のどちらに入るかは
タイミング次第で不定である。ただし **どちらに入っても消えることはない**:

`mark_write`([era.rs](../src/era.rs))は「`current_era` を読む → チャンクへ
`fetch_max` → もう一度 `current_era` を読む」を行い、**刻印中に checkpoint が
era を進めていたら、新しい era で刻印し直す**(リトライ)。これにより:

> **保証**: `mark_write` が戻った時点で、そのチャンクの era は「戻るまでに
> 完了したどの checkpoint の新 era 以上」である。

したがって、書き込みが checkpoint と競合しても **closed_era 側に取り残されて
今回の dump にも次回にも現れない、という消失は起きない**。競合した in-flight
書き込みは最悪でも **次の era に押し出されて次回差分に現れる**(過大報告)。
これは dm-era と同じ「過小報告しない」契約を、IO を止めずに満たしている。

補足(依然として推奨される運用):

- **アプリ整合(application-consistent)なバックアップが必要なら、やはり
  checkpoint 前に静止させる**(fsfreeze、VM なら guest agent の fs-freeze)。
  上記の保証は「変更の取りこぼしがない」ことであって、「in-flight 書き込みが
  今回と次回のどちらの差分に入るか」までは決めない。ファイルシステム的に
  一貫した瞬間を切り出したい場合は静止が要る(dm-era でも同じ)
- 静止なし運用は「クラッシュコンシステント相当・取りこぼしなし」と理解する。
  dm-era は checkpoint 中に IO をブロックしてこれを実現するが、ublkera は
  IO を止めずに `mark_write` のリトライで同等の非消失性を得ている

## 4. dump と並行書き込み

`ranges_since` は era 配列をアトミックに 1 要素ずつ読むだけで、配列全体の
スナップショットは取らない。dump 実行中に書き込まれたチャンクは「今回の
結果に入る/入らない」どちらもありうるが、刻印自体は失われないので、
入らなかった場合も同じ `--since` での次回 dump には必ず現れる。
checkpoint 後にその `closed_era` で dump する通常の使い方では、dump 中の
新規書き込みは era > closed_era で刻印されるため結果は安定する。

## 5. 制御プレーン(レジストリ)の直列化

- **ctl ループは 1 スレッドでリクエストを 1 件ずつ処理する**。これが
  add/del/checkpoint/dump 相互の最上位の直列化点であり、たとえば
  「同じバッキングを同時に 2 回 add して二重アタッチになる」TOCTOU は
  プロトコルレベルで起きない(重複チェック自体もレジストリのロック内)
- レジストリは単一の `Mutex<HashMap>`。**ロックを保持したまま生きている
  スレッドを join しない**規律を守っている:
  - `del`: エントリをマップから **取り除いてからロックを解放し**、その後に
    ublk デバイス削除 → supervisor join。supervisor 側はレジストリに
    触らないのでデッドロックしない
  - `reap`: `is_finished()` が真(=既に終了済み)のスレッドしか join しない
- デバイスが**外部ツールで削除された**場合(daemon を経由しない
  `ublk del` 等)は、supervisor がキュー停止を検知してメタデータを保存して
  終了し、次のコマンド処理時の `reap` でレジストリから回収される

## 6. メタデータファイルの保存

- 保存は「一時ファイルに全書き込み + `fsync` + `rename`」のアトミック
  置換。**読める状態の中途半端なファイルは残らない**
- 保存の発火点は checkpoint(ctl スレッド)・デタッチ/シャットダウン時
  (supervisor スレッド)。通常運用では ctl ループの直列性と
  「del は supervisor join 後に返る」順序により同一ファイルへの同時保存は
  起きない。**唯一の例外**は「checkpoint 実行中に外部ツールでデバイスを
  削除した」場合で、ctl スレッドと supervisor の保存が競合しうる
  (最後の rename が勝つ。ファイル自体は壊れない)
- 保存中の並行書き込みで刻印された era は、そのファイルに入るとは
  限らない(メモリ上には残る)。**daemon クラッシュ後にファイルから復元
  した場合、最後の checkpoint 前後の記録が欠けている可能性がある**ため、
  クラッシュ後はフルバックアップを取り直すこと(README の注意と同じ)

## 保証まとめ

| シナリオ | 保証 |
|---|---|
| 複数キュー/複数スレッドの同時書き込み・discard | 刻印は欠落・破損しない(atomic + fetch_max) |
| 完了応答済みの書き込み・discard | 必ず記録済み(刻印 → 応答の順序) |
| 静止(fsfreeze)後の checkpoint | 静止時点までの変更が漏れなく差分に出る |
| 静止なしの checkpoint | 取りこぼしなし。競合した in-flight 書き込みは今回/次回いずれかの差分に必ず出る(過大報告側、§3) |
| 同時 add / 同時 checkpoint / add と del の交錯 | ctl ループ直列化 + 単一 Mutex で安全 |
| daemon クラッシュ | 最後の checkpoint 以降の記録は失われうる → 要フル取り直し |
