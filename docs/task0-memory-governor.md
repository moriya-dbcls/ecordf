# メモリガバナ（クエリ受け入れ判定）設計コントラクト — タスク0

OOM 対策の根本実装。2026-06-08、jpost 運用中に OS の OOM killer に 4 回連続で
kill された（anon-rss 約 63.5GB / 物理 62GB 超過）。原因は **同時実行クエリの
中間結果 materialize のヒープ膨張**。`max_intermediate_rows=100M`(≈4GB/クエリ) ×
`max_concurrent_queries=12` ≈ 48GB ＋ キャッシュで物理超過。

## 方針

「同時実行数」という独立した制約は廃止する。**1クエリのメモリ上限**と
**全体メモリ上限**の2つだけを持ち、実効同時実行数は
`floor(全体メモリ上限 / 1クエリ上限)` として自然に決まる。

新クエリは全体プールから「1クエリ上限」分のメモリを **予約** してから実行する。
プールが足りなければ **拒否せず await で待機**し、先行クエリが終了して返却された
ら起動する。各クエリは自分の上限を満額予約済みなので、実行中に追加のメモリ待ちは
発生せず **デッドロックしない**。

## 設定（config.rs / ecordf.toml）

```toml
[query]
# 1クエリが中間結果に使ってよいメモリ上限(MiB)。executor がバイト見積りで enforce。
# 0 = 旧来どおり max_intermediate_rows のみで判定（バイト上限なし）。後方互換。
max_intermediate_mb = 4096

[server]
# サーバ全体のクエリ用メモリプール(MiB)。新クエリはここから予約して実行。
# 0 = プールゲートなし（旧来どおり無制限）。後方互換。
total_query_mem_mb = 32768
```

- `max_concurrent_queries` は **非推奨**。後方互換のため残すが、`total_query_mem_mb > 0`
  のときはメモリプールが主ゲート。`max_concurrent_queries > 0` の場合のみ追加の数ゲート
  として併用可（既定 0 = 無効）。jpost 運用では 0 にする。

## 共有インターフェース（タスク間の契約。勝手に変えない）

1クエリの予約量 `reserve_mb` の決定（server 側で計算）:
```
reserve_mb = if config.query.max_intermediate_mb > 0 {
                 config.query.max_intermediate_mb
             } else {
                 // バイト上限未設定時は行数上限から概算（1行≈40B）
                 max(1, config.query.max_intermediate_rows * 40 / (1024*1024))
             }
```

executor 側の1クエリ上限 enforcement（QueryConfig にヘルパ追加）:
```rust
impl QueryConfig {
    /// 指定アリティ(列数)の中間結果が保持してよい最大行数。
    /// 絶対行数上限 max_intermediate_rows と、メモリ上限 max_intermediate_mb の
    /// 両方を尊重し、厳しい方を返す。
    pub fn row_cap(&self, arity: usize) -> usize {
        let arity = arity.max(1) as u64;
        let by_rows = self.max_intermediate_rows;
        if self.max_intermediate_mb == 0 {
            return by_rows.max(1);
        }
        // 1 TermId=8B + Vec/dedup オーバヘッド込みで保守的に 24B/列 と見積る
        const BYTES_PER_TERM: u64 = 24;
        let budget = self.max_intermediate_mb as u64 * 1024 * 1024;
        let by_mem = (budget / (arity * BYTES_PER_TERM)) as usize;
        by_mem.min(by_rows).max(1)
    }
}
```

## タスク分担（担当ファイルが重複しないので並列可。ただし task3 を先に）

- **task3** (`src/config.rs`): `QueryConfig.max_intermediate_mb: usize`（既定 0）、
  `ServerConfig.total_query_mem_mb: u64`（既定 0）を追加。`row_cap()` ヘルパ実装。
  `max_concurrent_queries` の doc を「非推奨・total_query_mem_mb 優先」に更新。
  doc コメントの例 toml も更新。`cargo build` が通ること。
- **task5** (`src/sparql/executor.rs`): 中間結果の上限チェック箇所
  （`>= self.config.max_intermediate_rows` 約15箇所、`truncate(...max_intermediate_rows)`
  含む）を、その ResultSet のアリティを使った `self.config.row_cap(arity)` に置換。
  アリティは ResultSet の列数（vars.len() / 行の長さ）。cross-product 早期 abort
  （executor.rs:455 付近）の比較も row_cap ベースに合わせる。
- **task7** (`src/server.rs`, `src/main.rs`): `AppState` にメモリプール用
  `Arc<Semaphore>`（permit 数 = `total_query_mem_mb`）と `reserve_mb` を保持。
  `run_query` で `acquire_many_owned(reserve_mb)` を **await**（=待機）してから実行、
  permit はクエリ終了までガード保持。`serve()` 起動時に
  `reserve_mb > total_query_mem_mb`（>0時）なら警告し reserve_mb を total に clamp
  （でないと永久待機）。待機が発生したらログ（`tracing::info!` で待ち時間）。
  `max_concurrent_queries` の既存セマフォは `>0` のときのみ併用維持。

## 依存と順序

task3 が config フィールドを追加 → ビルド確認 → task5/task7 を並列。
task5 は `config.query.max_intermediate_mb` / `row_cap()`、task7 は
`config.query.max_intermediate_mb` / `config.server.total_query_mem_mb` を参照する。

## やってはいけないこと（全タスク共通・プロンプトにも明記）

- `git push`、サーバの起動/停止/再起動はしない（タスク0が行う）。
- 担当外ファイルを編集しない。
- `TermId` / `Triple` / 公開 API シグネチャを変えない。
- 承認が要る事態は `report/need_approval.md` に書いて中断。
