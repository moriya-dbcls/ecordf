# EcoRDF — 設計思想と技術仕様

## なぜ Virtuoso / Qlever より低コストか

| システム | 弱点 | 根本原因 |
|---------|------|---------|
| Virtuoso | 複雑なSPARQLが遅い | 行指向B木 + ハッシュ結合の逐次処理 |
| Qlever   | メモリ消費が激しい | 起動時に全インデックスをRAMに展開 |
| **EcoRDF** | — | memmap2（OSページング）+ Leapfrog Triejoin |

---

## 核心技術 1 — memmap2 による OS管理ページング

```
Qlever方式:
  起動 → 全データをRAM展開（1億トリプル ≈ 12GB）→ クエリ実行

EcoRDF方式:
  起動 → インデックスファイルをmmap → クエリ実行
  OSカーネルがページキャッシュを管理
  → アクセスしたページだけRAMに載る
  → 典型的なSPARQLは全ページの2〜5%しか触れない
  → 実効RAM ≈ ワーキングセット（クエリ依存）
```

```rust
// index.rs — 安全: ファイルを変更しない (read-only mount)
let mmap = unsafe { Mmap::map(&file)? };
// 仮想アドレス空間にマップするだけ。実RAMページは
// 初回アクセス時にOSがロード、メモリプレッシャーで自動evict。
```

---

## 核心技術 2 — Leapfrog Triejoin

Virtuoso のハッシュ結合は 2パターンずつ逐次処理し、中間結果を物理化します。

```
?protein up:organism <taxon:9606>    → 20,000件をハッシュテーブルに積む
?protein up:classifiedWith ?go_term  → 200,000件をプローブ
?protein disease:related ?disease    → 50,000件をプローブ
合計: 中間270,000行のメモリ
```

Leapfrog Triejoin は **全パターンのイテレータを同時に動かし**、共有変数に全一致する値だけを列挙します。

```
iter1 (ヒトタンパク質): [P00533, P01375, P04637, ...]
iter2 (GO注釈あり):    [P00533, P00734, P04637, ...]
iter3 (疾患関連あり):  [P00533, P01116, P04637, ...]

アルゴリズム:
  max = max(全イテレータの現在値)
  全イテレータを max にシーク
  全一致 → 結果に追加、全員 advance
  不一致 → 新しい max を更新、繰り返し
```

**計算量: O(output × k × log n)** — 中間結果を一切メモリに展開しない

実装は `src/sparql/executor.rs` の `leapfrog_join` と `execute_leapfrog_join`。

---

## インデックス構成

```
SPO索引 (spo.bin):  主語でソート → 特定エンティティの全述語・目的語を高速取得
POS索引 (pos.bin):  述語でソート → 生命科学クエリの大半（型・関係の絞り込み）
OSP索引 (osp.bin):  目的語でソート → 値や概念からの逆引き
GSPO索引 (gspo.bin): グラフ+SPO → Named Graphs / N-Quads 対応（存在する場合のみ）
```

各エントリは `[s: u32, p: u32, o: u32]` の 12 バイト（GSPO は先頭に `g: u32` を加えた 16 バイト）。  
全インデックスは `TripleIndex::open` がメモリマップで開き、クエリ時は `scan()` / `scan_graph()` で範囲走査します。

**ディスク使用量の概算:**

| トリプル数 | SPO+POS+OSP | GSPO付き | 辞書込み目安 |
|-----------|-------------|---------|------------|
| 1,000万   | 360 MB      | 520 MB  | ～450 MB   |
| 1億       | 3.6 GB      | 5.2 GB  | ～4.5 GB   |
| 10億      | 36 GB       | 52 GB   | ～45 GB    |

---

## 辞書 (Dictionary)

全URI・リテラルを `u32` IDに変換。  
生命科学の主要名前空間（UniProt, PDB, OBO, XSD, RDF/S, OWL … 19プレフィックス）をプレフィックステーブルで圧縮し、辞書サイズを約40%削減します。

```
dict.bin フォーマット:
  [magic: "ECOD0001"][prefix_count: u32]
  (各プレフィックス: [len: u16][bytes])
  [term_count: u32]
  (各タームID: [prefix_id: u16][local_len: u32][local_bytes])
```

**スレッド安全な interior mutability:**  
クエリ実行時に `STR(IRI)`・`CONCAT`・`UCASE` などで生成されるリテラルを辞書に追加できるよう、`encode` は `&self` で呼び出せます。内部は `RwLock<Vec<Box<str>>>` と `RwLock<FxHashMap<String, u32>>` で実装し、`axum` のマルチスレッド環境でも安全に動作します。

```rust
// 読み取り: read lock のみ（複数スレッドが並行して実行可）
pub fn decode(&self, id: u32) -> String { ... }
pub fn lookup(&self, s: &str) -> Option<u32> { ... }

// 挿入: write lock（既存IDなら read lock だけで返す）
pub fn encode(&self, s: &str) -> u32 { ... }
```

クエリ時の追加エントリはメモリ上にのみ存在し、`dict.bin` には保存されません。

---

## SPARQL 1.1 対応状況

### 対応済み

| 機能 | 実装箇所 |
|------|---------|
| SELECT / ASK | `executor.rs` |
| BGP (基本グラフパターン) | Leapfrog Triejoin + hash join |
| OPTIONAL | left outer join |
| UNION | 列スキーマを合わせてマージ |
| FILTER (REGEX, STR, LANG, DATATYPE, 比較, 論理演算) | `eval_bool` / `eval_string` |
| BIND | `ExecutionPlan::Extend` |
| VALUES | インライン値表 |
| GROUP BY / HAVING / 集計 (COUNT, SUM, MIN, MAX, AVG) | `apply_group_by` |
| ORDER BY / LIMIT / OFFSET | `execute_select` |
| DISTINCT | 重複除去 |
| プレフィックス宣言 (PREFIX) | パーサー |
| 算術演算 (+, -, *, /) | `eval_term` |
| 文字列関数 (UCASE, LCASE, CONCAT, CONTAINS, STRSTARTS, STRENDS) | `eval_term` |
| 型検査 (isIRI, isLiteral, isBlank, BOUND) | `eval_bool` |
| **Property Paths** (* + ? / \| ^ ) | BFS転移閉包 + 再帰評価 |
| **GRAPH clause / Named Graphs** | GSPO索引 + `execute_named_graph` |
| **STR(IRI) のリテラル型化** | `encode` on `&self` で辞書に登録 |

### 未対応 / 制限事項

| 機能 | 状況 |
|------|------|
| CONSTRUCT | 未実装（`QueryError::Unsupported`） |
| SPARQL UPDATE (INSERT/DELETE) | 未実装 |
| SERVICE (フェデレーション) | 未実装 |
| サブクエリ (SELECT in WHERE) | パーサー対応済み、実行は outer plan として処理 |
| Leapfrog の多変数完全実装 | 共有変数が2つ以上のとき hash join にフォールバック |

---

## データ入力フォーマット

| 拡張子 | フォーマット | 動作 |
|-------|------------|------|
| `.nt` / `.ntriples` | N-Triples | SPO/POS/OSPインデックスに格納 |
| `.nq` / `.nquads` | N-Quads | SPO/POS/OSP（ユニオングラフ）+ GSPO（名前付きグラフ） |
| `.gz` | gzip済みN-Triples | `--features gzip` でビルド時のみ対応 |

---

## ファイル構成

```
ecordf/
├── src/
│   ├── config.rs      設定: Config / QueryConfig / ServerConfig
│   │                    ecordf.toml を serde+toml でデシリアライズ
│   │                    ファイル探索順: --config > <store-dir>/ecordf.toml > デフォルト値
│   ├── dict.rs        辞書: 文字列 ↔ u32 ID
│   │                    RwLock による interior mutability
│   │                    19プレフィックスの名前空間圧縮
│   ├── triple.rs      TripleId / Triple / Quad 型、UNBOUND定数
│   ├── index.rs       memmap2ベースのソート済み整数配列
│   │                    IndexBuilder / IndexFile (SPO・POS・OSP)
│   │                    GspoBuilder / GspoIndexFile (Named Graphs)
│   │                    AllBuilders (ビルドフェーズの統合API)
│   ├── store.rs       ストアファサード: load / open / open_with_config / query / stats
│   │                    Config を保持し Executor に QueryConfig を渡す
│   ├── loader.rs      N-Triples / N-Quads ストリーミングパーサー
│   ├── sparql/
│   │   ├── ast.rs     SPARQL 1.1 AST型定義
│   │   │                PropertyPath / GraphPattern を含む完全定義
│   │   ├── parser.rs  手書き再帰下降パーサー
│   │   │                Property Path (*/+/?/|/^//) 対応
│   │   ├── plan.rs    実行計画型 (ExecutionPlan enum)
│   │   └── executor.rs Leapfrog Triejoin + hash join + left join
│   │                    QueryConfig (max_intermediate_rows / bind_join_threshold)
│   │                    Property Path BFS (ZeroOrMore / OneOrMore)
│   │                    Named Graph スキャン (execute_named_graph)
│   │                    FILTER / BIND / STR の正確な評価
│   ├── server.rs      axum HTTPサーバー (SPARQL 1.1 Protocol)
│   │                    AppState (Arc<Store> + Option<Semaphore>)
│   │                    spawn_blocking でクエリをブロッキングプールに委譲
│   │                    Semaphore による同時クエリ数制限
│   │                    JSON / XML / TSV / CSV レスポンス
│   │                    CORS オプション対応
│   └── main.rs        CLI: build / serve / query / stats
│                        serve: --host / --port / --cors / --config
│                        query: --config
├── ecordf.toml        設定ファイルのサンプル（全パラメータ・説明付き）
├── DESIGN.md          本ドキュメント
└── Cargo.toml
```

---

## ビルドと使用方法

### ビルド

```bash
cd ecordf
cargo build --release

# gzip対応を含める場合
cargo build --release --features gzip
```

### データ読み込み

```bash
# N-Triples
./target/release/ecordf build \
  --dir ./uniprot-store \
  uniprot_sprot.nt

# N-Quads（named graph付き）
./target/release/ecordf build \
  --dir ./togo-store \
  togoid.nq

# 読み込み完了後:
# Built store: 142357891 triples, 28456123 terms, 5 named graphs
```

### コマンドラインクエリ

```bash
# 表形式出力（デフォルト）
./target/release/ecordf query --dir ./uniprot-store \
  "SELECT ?protein ?name WHERE {
     ?protein a <http://purl.uniprot.org/core/Protein> ;
              <http://purl.uniprot.org/core/recommendedName> ?node .
     ?node <http://purl.uniprot.org/core/fullName> ?name .
   } LIMIT 20"

# Property Path
./target/release/ecordf query --dir ./go-store \
  "SELECT ?child WHERE {
     ?child <http://www.w3.org/2000/01/rdf-schema#subClassOf>* \
            <http://purl.obolibrary.org/obo/GO_0005575> .
   }"

# STR / BIND / FILTER
./target/release/ecordf query --dir ./uniprot-store \
  "SELECT ?up ?str WHERE {
     ?up a <http://purl.uniprot.org/core/Protein> .
     BIND(STR(?up) AS ?str)
     FILTER(REGEX(?str, \"UP000005640\"))
   } LIMIT 10"
```

### 設定ファイル

```bash
# store-dir に ecordf.toml を置くと自動読み込み
cp ecordf.toml ./uniprot-store/ecordf.toml
$EDITOR ./uniprot-store/ecordf.toml

# 明示的に指定する場合
./target/release/ecordf serve --dir ./uniprot-store --config /etc/ecordf.toml
```

主な設定項目（デフォルト値）:

| キー | デフォルト | 説明 |
|------|-----------|------|
| `query.max_intermediate_rows` | 50,000,000 | 中間結果の行数上限（OOM防止） |
| `query.bind_join_threshold` | 10,000 | bind_join / hash_join の切り替え閾値 |
| `server.host` | `127.0.0.1` | バインドアドレス |
| `server.port` | `7878` | TCPポート |
| `server.cors_origins` | `""` | CORS設定（`"*"` or カンマ区切りオリジン） |
| `server.max_concurrent_queries` | `0` | 同時クエリ数上限（0=無制限） |

### HTTPサーバー

```bash
# ローカル起動（設定はecordf.tomlまたはデフォルト値）
./target/release/ecordf serve --dir ./uniprot-store

# CLIフラグはconfigファイルより優先
./target/release/ecordf serve --dir ./uniprot-store --host 0.0.0.0 --port 8080

# CORS許可（全オリジン）
./target/release/ecordf serve --dir ./uniprot-store --cors '*'

# CORS許可（特定オリジン）
./target/release/ecordf serve --dir ./uniprot-store \
  --cors 'https://app.example.com,https://sparql.example.com'
```

### SPARQL 1.1 Protocol

```
GET  http://localhost:7878/sparql?query=SELECT+...
POST http://localhost:7878/sparql
  Content-Type: application/sparql-query
  Body: SELECT ...

レスポンス形式 (Accept ヘッダで指定):
  application/sparql-results+json  (デフォルト)
  application/sparql-results+xml
  text/tab-separated-values
  text/csv
```

---

## 性能特性

| シナリオ | Virtuoso | Qlever | EcoRDF |
|---------|---------|--------|--------|
| 1億トリプル・起動時RAM | ~4 GB | ~12 GB | ~500 MB* |
| BGP 2パターン・共有変数 | ベース | 2〜5× | 3〜8×† |
| BGP 5パターン・複雑JOIN | ベース | 3〜10× | 5〜15×† |
| コールドスタート時間 | 数秒 | 数分 | 即時 |
| 並列クエリ処理 | 対応 | 対応 | **対応**（tokio blocking pool） |
| Property Path (* 転移閉包) | 対応 | 対応 | BFS（対応済） |
| Named Graphs (GRAPH句) | 対応 | 対応 | GSPO索引（対応済） |

\* mmapによるワーキングセット管理（OS・クエリ依存）  
† Leapfrog Triejoinによる中間結果削減（データ・クエリ依存）

### 並列クエリ処理

1クエリの内部処理はシングルスレッドですが、**複数クエリは並列に処理されます**。

```
tokio worker threads (num_cpus):  HTTPコネクション管理・I/O
tokio blocking pool (最大512):    クエリ実行 (spawn_blocking)
Semaphore (max_concurrent_queries): アプリ層の同時数キャップ
```

各クエリは `spawn_blocking` でブロッキング専用スレッドプールに委譲されるため、非同期ワーカースレッドをブロックしません。`max_concurrent_queries = 0`（デフォルト）の場合はtokioの上限（512）まで並列実行できます。重いクエリが多い場合は `max_concurrent_queries = 2 × CPUコア数` を推奨します。

### スケール時の制約

- **ビルドフェーズ**: ソートはメモリ上で行うため、ビルド時のピークRAMはトリプル数に比例します（外部ソート未実装）。
- **1クエリの内部処理**: シングルスレッド。広域スキャンが絡む単一クエリではQleverの内部並列実行に劣ります。
- **Leapfrog**: 共有変数が2つ以上のパターンはハッシュジョインにフォールバックします（完全な多変数Leapfrogは今後の課題）。

---

## 今後の拡張候補

1. **SPARQL UPDATE** — INSERT DATA / DELETE DATA / MODIFY
2. **外部ソート対応** — ビルド時のメモリ削減（10億トリプル以上のビルド）
3. **クエリ内部の並列化** — rayon による JOIN内部のスレッド並列化（クエリ間並列は実装済み）
4. **Leapfrog 多変数完全実装** — 共有変数が2つ以上の場合もLeapfrogで処理
5. **SPARQL Federation** — SERVICE句による外部エンドポイント連携
6. **Block圧縮** — zstdでディスク使用量をさらに削減
7. **CONSTRUCT** — RDFグラフを返すクエリ形式
8. **クエリタイムアウト** — 長時間クエリを自動キャンセルする機能
