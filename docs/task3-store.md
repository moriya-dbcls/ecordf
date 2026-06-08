# タスク3: ロード・ストア・設定

## 担当ファイル

- `src/loader.rs` — N-Triples/N-Quads ストリーミングパーサー、Phase1/2 並列ロード
- `src/store.rs` — ストアファサード（load / open / query）
- `src/config.rs` — ecordf.toml のデシリアライズ

## このタスクの責務

RDF ファイルの読み込み、インデックスの構築、ストアの開閉、クエリの呼び出し窓口。
全機能を束ねる最上位の API 層。

## Store 構造体

```rust
pub struct Store {
    pub dict:            QueryDict,
    pub index:           Arc<TripleIndex>,
    pub dir:             PathBuf,
    pub config:          Config,
    pub stats:           StoreStatistics,
    pub pred_cache:      PredCache,
    pub path_cache:      PathCache,
    pub type_cache:      TypeCache,
}
```

### 主要メソッド

```rust
// ビルド
Store::load(dir, files)                          // シンプルな1パス（小規模）
Store::load_with_graphs(dir, inputs)             // 2パス外部ソート（推奨）
Store::load_with_graphs_resume_phase2(dir, inputs) // Phase2 から再開

// 開く
Store::open(dir)                                  // シンプル
Store::open_with_config(dir, config_path)        // ログ付き（serve 用）

// キャッシュビルド（open 後に呼ぶ）
store.build_pred_cache_sync(budget_mb, per_pred_cap_mb, priority_iris)
store.build_type_cache(budget_mb)
store.build_path_cache(rdf_config_specs, budget_mb)
store.warmup_background(warmup_mb)

// クエリ
store.query(sparql)                               // キャンセルなし
store.query_with_cancel(sparql, cancel_flag)      // タイムアウト対応
store.query_to_table(sparql)                      // 文字列テーブルとして返す
```

## ビルドシーケンス（2パス）

```
1. tmp_dir 作成 (_ecordf_tmp/)
2. Phase 1: collect_strings_parallel → dict チャンク → merge → dict_sorted.bin
3. Phase 2: 辞書サイズで戦略選択
   A (≤1B terms): load_triples_parallel (mmap binary search)
   B (>1B terms): load_triples_streaming (sequential dict scan)
4. AllBuilders::build_from_parallel_chunks → インデックスファイル書き出し
5. dict_sorted.bin をストアルートにコピー
6. dict.bin 書き出し（term_count > 4.3B なら skip）
7. tmp_dir 削除
8. config.build.auto_compress_cols が true なら compress_columns 実行
```

## InputSpec

```rust
pub struct InputSpec {
    pub path:  PathBuf,
    pub graph: Option<String>,  // Named Graph IRI（N-Triples のみ）
}
impl InputSpec {
    pub fn plain(path: PathBuf) -> Self
    pub fn with_graph(path: PathBuf, graph: String) -> Self
}
```

## Config 主要フィールド

```rust
pub struct BuildConfig {
    pub chunk_size:         usize,   // 外部ソートチャンク（0=旧1パス）
    pub dict_chunk_mb:      usize,   // Phase1 バッファ（MB）
    pub parallel_threads:   usize,   // 0=全コア
    pub auto_compress_cols: bool,    // ビルド後に compress-cols 自動実行
}
pub struct ServerConfig {
    pub host:                     String,
    pub port:                     u16,
    pub warmup_mb:                u64,
    pub pred_cache_mb:            u64,
    pub pred_cache_per_pred_cap_mb: u64,
    pub type_cache_mb:            u64,
    pub query_timeout_secs:       u64,
    pub scan_dontneed_mb:         u64,
    pub max_concurrent_queries:   usize,
}
pub struct ModelConfig {
    pub rdf_configs:  Vec<String>,
    pub path_cache_mb: u64,
}
```

設定ファイル探索順: `--config` 引数 → `<store_dir>/ecordf.toml` → デフォルト値。

## 対応フォーマット

| 拡張子 | 説明 |
|-------|-----|
| `.nt` / `.ntriples` | N-Triples |
| `.nq` / `.nquads` | N-Quads |
| `.nt.gz` / `.nq.gz` | gzip 圧縮版（feature = "gzip" が必要） |

## 注意事項

- `Store::open_with_config` は起動ログ（dict→index→stats の各ステップを `eprintln!`）を出力する。サーバー起動時はこちらを使う。
- `build_pred_cache_sync` は同期的に（ブロックして）キャッシュをビルドする。最初のクエリ前に必ずキャッシュが完成している必要がある場合はこちら。
- `auto_compress_cols` は `load_two_pass_internal` の末尾で実行される。既存ストアへの適用は `ecordf compress-cols --force` を使う。
