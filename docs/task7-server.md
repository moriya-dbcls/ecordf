# タスク7: HTTP サーバー・CLI・rdf-config

## 担当ファイル

- `src/server.rs` — axum HTTP サーバー（SPARQL 1.1 Protocol）
- `src/main.rs` — CLI サブコマンド
- `src/rdf_config.rs` — rdf-config GitHub/ローカル読み込み

## このタスクの責務

HTTP エンドポイントの提供、コマンドラインインターフェース、rdf-config からのパス抽出。
他タスクへの依存は全て一方向（下位タスクを呼ぶだけ）。

## HTTP サーバー（server.rs）

### エンドポイント

```
GET  /sparql?query=<encoded_SPARQL>
POST /sparql
  Content-Type: application/sparql-query
  Body: SELECT ...
```

### レスポンス形式（Accept ヘッダで選択）

| Accept | 形式 |
|--------|------|
| `application/sparql-results+json` | JSON（デフォルト） |
| `application/sparql-results+xml` | XML |
| `text/tab-separated-values` | TSV |
| `text/csv` | CSV |

### AppState

```rust
pub struct AppState {
    pub store: Arc<Store>,
    pub semaphore: Option<Arc<Semaphore>>,  // max_concurrent_queries
}
```

クエリは `spawn_blocking` でブロッキングスレッドプールに委譲（非同期ワーカーをブロックしない）。
`max_concurrent_queries > 0` の場合は Semaphore で同時実行数を制限。

### クエリタイムアウト

`config.server.query_timeout_secs > 0` のとき、tokio の `timeout` タスクが
`cancel_flag: Arc<AtomicBool>` をセットして executor を中断させる。

---

## CLI（main.rs）

### サブコマンド

| コマンド | 説明 |
|---------|-----|
| `build` | ストアをビルド |
| `serve` | HTTP サーバー起動 |
| `query` | コマンドラインクエリ |
| `stats` | 述語統計を表示 |
| `compress-cols` | Delta 圧縮を実行 |
| `build-pred-parts` | 述語パーティションファイルを生成 |

### build コマンドの主要フラグ

```
--dir          ストアディレクトリ
--from-file    入力ファイルリスト（- で stdin）
--resume-phase2  Phase1 をスキップして再開
--auto-compress-cols  ビルド後に compress-cols を自動実行
```

### serve コマンドの主要フラグ

```
--host, --port
--cors
--config
--warmup-mb
--pred-cache-mb, --pred-cache-per-pred-cap-mb
--rdf-config, --path-cache-mb
```

### serve の起動シーケンス

```
1. Store::open_with_config（ログ付きオープン）
2. build_pred_cache_sync（同期ビルド）
3. build_type_cache
4. rdf-config ロード → compound paths 抽出
5. build_path_cache_from_compounds
6. warmup_background（バックグラウンド）
7. Server::run
```

---

## rdf_config.rs

### 目的

rdf-config の `model.yaml` + `prefix.yaml` からブランクノードを経由する複合パスを抽出し、
PathCache のビルド材料として提供する。

### 対応入力形式

```rust
// GitHub URL
"https://github.com/dbcls/rdf-config/tree/master/config/jpostdb"
→ raw.githubusercontent.com から prefix.yaml と model.yaml を取得

// ローカルパス
"/path/to/rdf-config/jpostdb"
→ prefix.yaml と model.yaml をローカルから読む
```

### CompoundPath

```rust
pub type CompoundPath = Vec<String>;  // IRI 文字列のシーケンス
// 例: ["<http://biohackathon.org/resource/faldo#begin>",
//       "<http://biohackathon.org/resource/faldo#position>"]
```

### 注意事項

- IRI は `<http://...>` 形式（angle bracket あり）で返す。
  `PathCache` / `PredCache` の priority IRI 解決時は `trim_matches(|c| c == '<' || c == '>')` でストリップ。
- rdf-config の変数名（`?protein` 等）はIRI でないためスキップされる（`not_found` としてログに出る）。
  これは正常動作で、エラーではない。

## CORS 設定

```toml
cors_origins = ""    # CORS 無効（デフォルト）
cors_origins = "*"   # 全オリジン許可
cors_origins = "https://app.example.com,https://other.example.com"  # ホワイトリスト
```

## 注意事項

- `spawn_blocking` のスレッドプールは tokio のデフォルト 512 スレッド。
  `max_concurrent_queries = 2 × CPU コア数` を設定すると安定する。
- `build_pred_cache_sync` は同期実行なので起動が遅くなる場合は `pred_cache_mb` を下げるか
  バックグラウンドビルド（`build_pred_cache`）に切り替える。
  ただし `build_background` はキャッシュが完成する前にクエリが来ると遅い。
