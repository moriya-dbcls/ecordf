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
| `.nt.gz` / `.ntriples.gz` | gzip済みN-Triples | `--features gzip` でビルド時のみ対応 |
| `.nq.gz` / `.nquads.gz` | gzip済みN-Quads | `--features gzip` でビルド時のみ対応 |
| `.gz`（単体） | gzip済みN-Triples | 後方互換。ダブル拡張子推奨 |

---

## オプティマイザ — カーディナリティ推定による JOIN 順序決定

### 設計概要

BGP（基本グラフパターン）内の複数のトリプルパターンを結合する順番は、クエリ性能に最も大きく影響します。EcoRDF は **2段階のカーディナリティ推定**を組み合わせて、クロスプロダクトを避けながら小さな中間結果が得られる順序を貪欲法で選びます。

### 段階 1 — インデックスプローブ（常に利用）

パターン中の定数 IRI / リテラルを辞書でエンコードし、ソート済みインデックスに対してバイナリサーチで一致範囲を数えます。

```
?child up:proteome <chr:1>
  → TriplePattern { s: UNBOUND, p: up:proteome_id, o: chr1_id }
  → POS索引の (up:proteome, chr1) 範囲をバイナリサーチ
  → 推定値: ~2,000 件（ヒトプロテオーム内のタンパク質数）

?parent rdfs:subClassOf <GO:0005575>
  → TriplePattern { s: UNBOUND, p: subClassOf_id, o: GO_id }
  → 推定値: ~3,000 件（細胞成分 GO の子孫数）
```

2,000 < 3,000 なので `up:proteome <chr:1>` を先に評価 → クロスプロダクト回避。

**コスト: O(log N) / パターン。余分なメモリゼロ。**

### 段階 2 — 述語統計ファイル（stats.bin）

述語が定数で S/O が変数のパターン（`?child up:classifiedWith ?go`）はインデックスプローブが述語の全トリプル数を返してしまいます。`stats.bin` には各述語について以下を保持します。

| フィールド | 意味 |
|----------|------|
| `triple_count` | 述語の総トリプル数 |
| `subject_count` | 述語に現れる主語の異なり数 |
| `object_count` | 述語に現れる目的語の異なり数 |

これにより SP / PO パターンの平均ファンアウトを推定できます。

```
?child up:classifiedWith ?go   (どちらも変数)
  → stats.estimate(None, Some(up:classifiedWith), None)
  → triple_count = 3,200,000
  ↓
?child rdfs:label ?label        (どちらも変数)
  → stats.estimate(None, Some(rdfs:label), None)
  → triple_count = 500,000
```

`rdfs:label` の方がトリプル数が少ない → `rdfs:label` を先に評価してラベルで絞り込む。

### stats.bin の構築

`stats.bin` は初回の `Store::load` / `Store::open` 時に **2パスのO(N)スキャン**で構築し、以降は再利用します。

```
Pass 1 — POS索引を順走査 (P, O, S 順):
  Pが変わったとき → 新述語。P内でOが変わったとき → object_count++。triple_count++。

Pass 2 — SPO索引を順走査 (S, P, O 順):
  (S, P) のペアが変わったとき → subject_count++ (述語Pの)。
```

両パスとも追加メモリは `HashMap<TermId, PredicateStats>`（述語数 × 28 バイト）のみ。

**ファイルフォーマット (`stats.bin`):**

```
offset  0: magic          [u8; 8]  = "ECOSTAT1"
offset  8: total_triples  u64
offset 16: n_predicates   u64
offset 24: (28バイト × n_predicates):
             pred_id       u32
             triple_count  u64
             subject_count u64
             object_count  u64
```

### 既バインド変数の割引（Tier 1 のみ）

Tier 1（インデックスプローブ）を使う場合、すでに外側のパターンでバインドされた変数はその位置の選択性を大幅に上げますが、推定値にはその情報が含まれません。そこで **バインド済み変数の位置 1つにつき推定値を 1/100 に割り引き**ます。

Tier 2（述語統計）は SP / PO / SPO の match arm がすでにバインド位置を織り込んだ値を返すので割引は行いません。

### 例：問題クエリの JOIN 順序

```sparql
WHERE {
  VALUES ?proteome { <chr:1> <chr:2> ... }
  ?child up:classifiedWith ?parent ;
         up:proteome ?proteome .
  ?parent rdfs:subClassOf <GO:0005575> .
  ?child  up:mnemonic ?child_label .
  ?parent rdfs:label  ?parent_label .
}
```

| ステップ | 選択パターン | 推定値 | 根拠 |
|---------|-----------|-------|------|
| 1 | VALUES (hoisted) | — | 自己完結パターンは常に最初 |
| 2 | `?child up:proteome ?proteome` | ~2,000 | Tier 1: PO索引プローブ (chr:1 等が定数) |
| 3 | `?parent rdfs:subClassOf GO:0005575` | ~3,000 | Tier 1: PO索引プローブ |
| 4 | `?child up:classifiedWith ?parent` | ~30* | Tier 1: ?child, ?parent バインド済 → /100 × 2 |
| 5 | `?child up:mnemonic ?child_label` | 1* | Tier 2 or Tier 1: ?child バインド済 |
| 6 | `?parent rdfs:label ?parent_label` | 1* | Tier 2 or Tier 1: ?parent バインド済 |

\* バインド済み変数による割引後の推定値

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
│   │                    spo_scan_all / pos_scan_all (統計構築用全スキャン)
│   ├── stats.rs       述語統計: StoreStatistics / PredicateStats
│   │                    build_from_index (2パスO(N)スキャン)
│   │                    save / load / load_or_build
│   │                    estimate (SP/PO/P/SPO ファンアウト推定)
│   ├── store.rs       ストアファサード: load / open / open_with_config / query
│   │                    Config / StoreStatistics を保持
│   │                    Executor に QueryConfig + StoreStatistics を渡す
│   ├── loader.rs      N-Triples / N-Quads ストリーミングパーサー
│   ├── sparql/
│   │   ├── ast.rs     SPARQL 1.1 AST型定義
│   │   │                PropertyPath / GraphPattern を含む完全定義
│   │   ├── parser.rs  手書き再帰下降パーサー
│   │   │                Property Path (*/+/?/|/^//) 対応
│   │   ├── plan.rs    実行計画型 (ExecutionPlan enum)
│   │   └── executor.rs Leapfrog Triejoin + hash join + left join
│   │                    QueryConfig (max_intermediate_rows / bind_join_threshold)
│   │                    optimize_bgp / estimate_pattern_cardinality
│   │                    2段階カーディナリティ推定 (index probe + stats)
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
# 直接ファイルを指定（少数ファイルの場合）
./target/release/ecordf build \
  --dir ./uniprot-store \
  uniprot_sprot.nt uniprot_trembl.nt

# N-Quads（named graph付き）
./target/release/ecordf build \
  --dir ./togo-store \
  togoid.nq

# --from-file: ファイルリストをテキストファイルで指定
# （ファイル数が多くてコマンドラインに収まらない場合）
cat > inputs.txt << 'EOF'
# UniProt release 2024_04
/data/uniprot/uniprot_sprot.nt.gz
/data/uniprot/uniprot_trembl.nt.gz
/data/uniparc/uniparc.nt.gz
EOF
./target/release/ecordf build --dir ./uniprot-store --from-file inputs.txt

# --from-file -: find などのパイプから stdin 経由で渡す
find /data -name '*.nt.gz' | \
  ./target/release/ecordf build --dir ./store --from-file -

# 直接指定と --from-file の混在も可
./target/release/ecordf build --dir ./store \
  --from-file batch1.txt --from-file batch2.txt \
  extra.nt

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
9. **HyperLogLog** — subject_count / object_count の厳密カウント（現在は全スキャン）
10. **BOUND_VAR_FACTOR の自動調整** — 述語の distinctiveness から動的に推定倍率を計算
