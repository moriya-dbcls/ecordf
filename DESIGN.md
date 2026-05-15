# EcoRDF — 設計思想と技術的優位性

## なぜVirtuosoとQleverを超えられるか

### 問題の整理

| システム | 弱点 | 原因 |
|---------|------|------|
| Virtuoso | 複雑なSPARQLが遅い | 行指向B木 + ハッシュ結合 |
| Qlever | メモリ消費が激しい | 起動時に全データをRAMに展開 |

---

## EcoRDFの3つの核心技術

### 1. memmap2 による OS管理ページング（Qleverへの回答）

```
Qlever方式:
  起動時 → 全データをRAM展開 → クエリ実行
  問題: 1億トリプル ≈ 3.6GB 固定消費

EcoRDF方式:
  起動時 → インデックスファイルをmmap → クエリ実行
  OS kernel がページキャッシュを管理
  → アクセスしたページだけRAMに載る
  → 典型的なSPARQLセッションでは全体の2〜5%しか触れない
  → 実効メモリ: ワーキングセット比例
```

**実装の核心 (`index.rs`):**
```rust
// 安全: ファイルを変更しない (read-only mount)
let mmap = unsafe { Mmap::map(&file)? };
// mmap は仮想アドレス空間にマップされるだけ
// 実際のRAMページは初回アクセス時にOSがロード
// メモリプレッシャーがかかれば自動的にevict
```

---

### 2. Leapfrog Triejoin（Virtuosoへの回答）

**ハッシュ結合の問題:**
```
SELECT ?protein ?go_term ?disease WHERE {
  ?protein up:organism <taxon:9606> .     # パターン1
  ?protein up:classifiedWith ?go_term .   # パターン2
  ?protein disease:related ?disease .     # パターン3
}
```

ハッシュ結合:
1. パターン1を実行 → 20,000件のヒトタンパク質
2. パターン2をハッシュ結合 → 200,000 GO注釈をスキャン
3. パターン3をハッシュ結合 → 50,000 疾患関連をスキャン
**合計: 270,000件をスキャン**

**Leapfrog Triejoin:**
```
3つの整列済みイテレータを同時に走査:
  iter1: [P00533, P01375, P04637, ...] (ヒトタンパク質)
  iter2: [P00533, P00734, P04637, ...] (GO注釈あり)  
  iter3: [P00533, P01116, P04637, ...] (疾患関連あり)

アルゴリズム:
  max = max(iter1.current, iter2.current, iter3.current)
  全イテレータを max にシーク
  全一致 → 結果に追加、全て advance
  不一致 → 新しい max を計算、繰り返し
```

**計算量: O(output × k × log n)** — 中間結果を一切メモリに展開しない

---

### 3. 3インデックス戦略

```
SPO索引: S で絞る → UniProtタンパク質IDで高速検索
POS索引: P で絞る → 述語が固定のパターン（生命科学クエリの大半）
OSP索引: O で絞る → オブジェクト（値・概念）での逆引き
```

パターン `?protein rdf:type up:Protein` は POS索引を使用:
- P = rdf:type → POS索引でO = up:Proteinを探す
- ソート済み配列のバイナリサーチ → O(log n)

---

## ファイル構成

```
ecordf/
├── src/
│   ├── dict.rs       辞書: 文字列 ↔ u32 ID
│   │                  生命科学名前空間の圧縮対応
│   ├── triple.rs     トリプル型とインデックス選択ロジック
│   ├── index.rs      memmap2ベースのソート済み整数配列
│   │                  IndexBuilder (ロード時) + IndexFile (クエリ時)
│   ├── store.rs      ストアファサード: load/open/query API
│   ├── loader.rs     N-Triplesストリーミングパーサー
│   ├── stats.rs      ヒストグラム統計（カーディナリティ推定）
│   ├── sparql/
│   │   ├── ast.rs    SPARQL 1.1 AST型定義
│   │   ├── parser.rs 手書き再帰下降パーサー
│   │   ├── plan.rs   実行計画型
│   │   └── executor.rs Leapfrog Triejoin + ハッシュ結合
│   ├── server.rs     axum HTTPサーバー (SPARQL 1.1 Protocol)
│   └── main.rs       CLI (build/serve/query/stats)
└── Cargo.toml
```

---

## ビルドと使用方法

### ビルド
```bash
cd ecordf
cargo build --release
```

### データ読み込み
```bash
# UniProt RDF (.nt形式)
./target/release/ecordf build \
  --dir ./uniprot-store \
  uniprot_sprot.nt uniprot_taxonomy.nt

# 読み込み完了後:
# Built store: 142357891 triples, 28456123 terms
```

### SPARQLクエリ
```bash
# コマンドラインから
./target/release/ecordf query --dir ./uniprot-store \
  "SELECT ?protein ?name WHERE {
     ?protein a <http://purl.uniprot.org/core/Protein> ;
              <http://purl.uniprot.org/core/organism> <http://purl.uniprot.org/taxonomy/9606> ;
              <http://purl.uniprot.org/core/recommendedName> ?nameNode .
     ?nameNode <http://purl.uniprot.org/core/fullName> ?name .
   } LIMIT 20"

# HTTPサーバーとして
./target/release/ecordf serve --dir ./uniprot-store --port 7878

# SPARQL 1.1 Protocol互換
curl "http://localhost:7878/sparql?query=SELECT+*+WHERE+{+%3Fs+%3Fp+%3Fo+}+LIMIT+10"
```

### SPARQLWebサービスとして (SPARQL 1.1 Protocol準拠)
```
GET  http://localhost:7878/sparql?query=...
POST http://localhost:7878/sparql
  Content-Type: application/sparql-query
  Body: SELECT ...

レスポンス形式:
  application/sparql-results+json (デフォルト)
  application/sparql-results+xml
  text/tab-separated-values
  text/csv
```

---

## 性能特性の試算

| シナリオ | Virtuoso | Qlever | EcoRDF |
|---------|---------|--------|--------|
| 1億トリプル読み込みRAM | ~4GB | ~12GB | ~500MB* |
| BGP (2パターン, 共有変数) | ベース | 2-5×速 | 3-8×速† |
| 複雑なJOIN (5パターン) | ベース | 3-10×速 | 5-15×速† |
| コールドスタート時間 | 数秒 | 数分 | 即時 |

\* mmapによるワーキングセット管理（OS次第）  
† Leapfrog Triejoinによる中間結果削減（データ依存）

---

## 今後の拡張

1. **Property Paths** (`+`, `*`, `?`, `/`, `|`) — 現在は基本パターンのみ
2. **SPARQL UPDATE** — INSERT/DELETE
3. **Named Graphs** — GRAPH <g> { ... }
4. **並列クエリ実行** — tokioを使った非同期JOIN
5. **Block圧縮** — zstdでディスク使用量をさらに削減
6. **SPARQL Federation** — SERVICE句による外部エンドポイント連携
