# EcoRDF — 設計思想と技術仕様

## 設計の動機

EcoRDF が重視した3点：

- **複雑な JOIN の性能**: Virtuoso のようなハッシュ結合の逐次処理に対して、Leapfrog Triejoin（最悪ケース最適）を採用。
- **ビルド時のメモリ効率**: 辞書・インデックスを2パス外部ソートで構築し、ピーク RAM をデータセットサイズに依存しない定数に抑える。UniProt 全体（~13.4B 固有 IRI/リテラル）のように u32 上限（4.3B）を超えるデータセットにも対応するため、`TermId` は u64 を採用。
- **並列ロード**: rayon によるファイル並列処理で、複数ファイルの Phase 1/2 を CPU コア数に応じてスケールアップ。

---

## 核心技術 1 — memmap2 による OS管理ページング

インデックスファイルは `memmap2::Mmap` で仮想アドレス空間にマップされます。実 RAM ページは初回アクセス時にのみ OS がロードし、メモリプレッシャー時は自動的に evict されます。

```
起動 → インデックスファイルをmmap → クエリ実行
OSカーネルがページキャッシュを管理
→ アクセスしたページだけRAMに載る
→ 実効RAM ≈ ワーキングセット（クエリ依存）
```

```rust
// index.rs — 安全: ファイルを変更しない (read-only mount)
let mmap = unsafe { Mmap::map(&file)? };
// 仮想アドレス空間にマップするだけ。実RAMページは
// 初回アクセス時にOSがロード、メモリプレッシャーで自動evict。
```

---

## 核心技術 2 — 2パス外部ソート辞書ビルダー

`dict_builder.rs` が実装するメモリ効率的な辞書構築。インメモリ `Dictionary` は全ユニーク文字列を同時保持するため、UniProt 規模で 10 GB 超のメモリを消費する問題があった。

### Phase 1 — DictBuilder（文字列収集）

入力ファイルをストリームしながら文字列だけを収集。メモリが `dict_chunk_mb` MB を超えたらソート・dedup してディスクへチャンクを書き出す。全ファイル走査後に k-way マージで `dict_sorted.bin` を生成。

```
ピーク RAM = チャンクバッファ 1 つ分（≤ dict_chunk_mb MB）
```

**EMFILE 対策 + 並列化 — 階層マージ (`MAX_FAN_IN = 64`):**  
チャンク数が 64 を超える場合は、64 個ずつバッチに分けて中間チャンクを生成し、最終マージを行う。macOS のデフォルト fd 上限 (256) でも安全。

同一レベル内のバッチは互いに独立しているため、**Rayon で並列処理**する（`into_par_iter().zip()`）。32 コア環境では Level 0 の数千バッチが ~32× 速くなる。最終マージ（中間ファイル数十本）は逐次で行う。

```
Level 0 batches:  [B0] [B1] [B2] … [B4700]   ← 32スレッドで並列
Level 1 batches:  [B0] [B1] … [B73]           ← 並列
Level 2 final:    直接 dict_sorted.bin へ      ← 逐次
```

### Phase 2 — トリプルロード（2戦略）

`term_count` によって自動的に戦略を切り替えます。

#### 戦略 A: mmap バイナリサーチ（`term_count ≤ 1B`）

`dict_sorted.bin` を `memmap2::Mmap` でマップし、オフセットテーブルを使った O(log N) バイナリサーチで ID を解決。頻出タームはホットキャッシュ (≤ 4M エントリ / 約 400 MB) に保持して再探索を回避。

```
ピーク RAM = ホットキャッシュ（≤ ~400 MB）+ OS ページキャッシュ（mmap）
```

辞書が RAM に収まる小〜中規模データセットに適した高速なパス。

#### 戦略 B: Streaming Phase 2（`term_count > 1B`、UniProt 規模）

**問題**: UniProt では `dict_sorted.bin` が約 640 GB（13.4B 固有タームで RAM 64 GB を大幅超過）。バイナリサーチが毎回ランダムページフォルトを起こし、CPU 使用率 5% のまま3日以上稼働する。

**解決策**: ランダム I/O をゼロにする3ステップのバッチ処理。ファイルを `batch_size` 本ずつまとめて処理する：

```
for each batch:
  ┌─ Phase 2a (並列): ファイルごとに unique 文字列を収集・ソート
  │   → per_file_sorted: Vec<Vec<String>>（各ファイルの辞書順文字列リスト）
  │
  ├─ Join   (逐次): dict_sorted.bin を1回 sequential スキャン
  │   → k-way merge of per_file_sorted vs. dict stream
  │   → LocalMap（string→u64）を各ファイル用に構築
  │   複数ファイルへの同一文字列の割当ても1パスで完結
  │
  └─ Phase 2b (並列): LocalMap を使って O(1) ハッシュルックアップでトリプルロード
```

**メモリ**: `batch_size × strings_per_file × 80 bytes × 2 ≤ ram_budget`  
`ram_budget = dict_chunk_mb × parallel_threads`（Phase 1 と同じ予算を流用）

**I/O**: 1バッチあたり dict_sorted.bin を1回フルスキャン（sequential I/O）。  
UniProt (1200 MB/thread × 32 threads = 40 GB budget, batch ≈ 132ファイル, スキャン回数 ≈ 54回):  
`54 × 640 GB / 2 GB/s ≈ 5時間`（Join のみ）→ 合計 **12〜20時間**（現状3日以上から大幅改善）。

```
DictScanner: dict_sorted.bin の文字列セクションを先頭から順次読む
 →「辞書は大きいが I/O は sequential」を活かして page fault ゼロ
```

### QueryDict — 実行時の辞書インターフェイス

`store.rs` の `dict` フィールドは `QueryDict` enum になっており、`ReadonlyDict`（2パス）か `Dictionary`（レガシー1パス）かを透過的に切り替える。

### dict_sorted.bin フォーマット

```text
[magic: b"ESRT0001"  (8 bytes)]
[count: u64           (8 bytes)]   ← ユニーク項数
[offsets_start: u64   (8 bytes)]   ← オフセットセクションの開始バイト位置
── 文字列セクション (byte 24 から) ──────────────────────────
for each term (辞書順):
  [len: u32][bytes: len × u8]
── オフセットセクション (offsets_start から) ─────────────────
for each term i in 0..count:
  [u64]  term i の (len, bytes) の絶対バイト位置
```

ID は辞書順インデックスで割り当て。Phase 2 で構築されるトリプルインデックスが同じ ID を使うため、全文字列を HashMap に展開せず一貫性が保たれる。

### レジューム機能 — `--resume-phase2`

長大データセットのビルドが中断した場合：

- **ケース A**: `_ecordf_tmp/dict_sorted.bin` が存在 → Phase 1 を完全スキップして Phase 2 へ
- **ケース B**: チャンクファイル (`_ecordf_tmp/p1_*`) が残存 → マージだけやり直してから Phase 2 へ

```bash
ecordf build --dir ./store --resume-phase2 --from-file inputs.txt
```

### ビルドメモリ概算（2パス時）

| フェーズ | ピーク RAM |
|---------|-----------|
| Phase 1（文字列収集）| `dict_chunk_mb × parallel_threads`（デフォルト 200 MB × スレッド数）|
| マージ | I/O バッファのみ（数十 MB 以下）|
| Phase 2 — 戦略 A（mmap バイナリサーチ）| `chunk_size × 24B × 3`（SPO+POS+OSP バッファ; Phase 1 解放後）|
| Phase 2 — 戦略 B（Streaming / term_count > 1B）| `batch_size × strings_per_file × 80B × 2` ≤ `dict_chunk_mb × threads`（Phase 1 と同予算）|
| クエリ実行時の辞書（ReadonlyDict）| ホットキャッシュ ≤ ~400 MB + mmap |

**ハードウェアプロファイル（推奨値）:**

|  RAM  | スレッド数 | `dict_chunk_mb` | `chunk_size`    |
|-------|-----------|-----------------|-----------------|
|  8 GB | 任意       | 200             | 5_000_000       |
| 16 GB | 任意       | 500             | 10_000_000      |
| 32 GB | 32         | 750             | 20_000_000      |
| 64 GB | 32         | **1200**        | **50_000_000**  |
|128 GB | 32         | 1500            | 100_000_000     |

---

## 核心技術 3 — Leapfrog Triejoin

Virtuoso のハッシュ結合は 2パターンずつ逐次処理し、中間結果を物理化します。EcoRDF の Leapfrog Triejoin は全パターンのイテレータを同時に動かします。

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

SPO/POS/OSP の3インデックスを採用。各インデックスは**列指向フォーマット**（3カラム分離ファイル）に加え、スパースな in-memory SkipIndex と、POS 専用の PredicateIndex を持ちます。

```
SPO索引:  主語でソート → 特定エンティティの全述語・目的語を高速取得
  spo.c0  (Subject列:   u64 × N)
  spo.c1  (Predicate列: u64 × N)
  spo.c2  (Object列:    u64 × N)
  spo.skip (SkipIndex: 8B × ⌈N/512⌉)

POS索引:  述語でソート → 生命科学クエリの大半（型・関係の絞り込み）
  pos.c0, pos.c1, pos.c2, pos.skip
  pos.pidx (PredicateIndex: 24B × 述語数)

OSP索引:  目的語でソート → 値や概念からの逆引き
  osp.c0, osp.c1, osp.c2, osp.skip

GSPO索引 (gspo.bin): グラフ+SPO → Named Graphs / N-Quads 対応（存在する場合のみ）
```

各カラムは `u64` 配列を memmap2 でマップし、クエリ時は `scan()` / `scan_graph()` で範囲走査します（GSPO は先頭に `g: u64` を加えた 4カラム）。

### SkipIndex — スパース1次キャッシュ

`SKIP_STRIDE = 512` おきに c0 の値を in-memory 配列（`anchors`）に保持します。

```
anchors[i] = c0[i × 512]  (各8バイト)
```

バイナリサーチ時：
1. `anchors` に対して上位バイナリサーチ → O(log(N/512)) の in-memory 比較
2. 絞り込み後の範囲は **最大 512 エントリ = 4 KB = 1 OS ページ**
3. `prefetch_c0` で MADV_WILLNEED ヒントを出し、実際の c0 ページフォルトは最小1回

| 規模 | c0 ファイルサイズ | anchors サイズ |
|------|-----------------|--------------|
| 1,000万トリプル | ~80 MB | ~156 KB |
| 11.8M (JPostDB) | ~94 MB | ~184 KB |
| 3,000万トリプル | ~240 MB | ~469 KB |

SkipIndex は `.skip` ファイルに永続化し、次回起動は `read_to_end` で即座にロードします（最初のビルド時のみ c0 の全スキャンが走りますが、ログにメッセージが出ます）。

### PredicateIndex (.pidx) — POS 述語→範囲マップ

POS インデックスの各述語について `[lo, hi)` のエントリ範囲を in-memory HashMap に保持します。

```
ファイルフォーマット: magic(8B) + pred_count(8B) + entries[(pred:u64, lo:u64, hi:u64) × pred_count]
エントリサイズ: 24 バイト × 述語数
例: JPostDB 数百述語 → ~数十 KB
```

POS スキャン時に述語が定数であれば、PredicateIndex を参照して **c0 全体を走査せずに** 正確な `[lo, hi)` 範囲を取得できます（SkipIndex の upper-bound ではなく exact range）。

**ディスク使用量の概算:**

| トリプル数 | SPO+POS+OSP（c0〜c2×3）| GSPO付き | 辞書込み目安 |
|-----------|----------------------|---------|------------|
| 1,000万   | 720 MB               | 1.0 GB  | ～800 MB   |
| 1億       | 7.2 GB               | 10.4 GB | ～9 GB     |
| 10億      | 72 GB                | 104 GB  | ～90 GB    |

---

## 辞書 (Dictionary)

全URI・リテラルを `u64` IDに変換。u64 採用により UniProt 全体（~13.4B 固有タームで u32 上限 4.3B を超過）にも対応。  
生命科学の主要名前空間（UniProt, PDB, OBO, XSD, RDF/S, OWL … 19プレフィックス）をプレフィックステーブルで圧縮し、辞書サイズを約40%削減します。

```
dict.bin フォーマット（レガシー互換、u32 上限あり）:
  [magic: "ECOD0001"][prefix_count: u32]
  (各プレフィックス: [len: u16][bytes])
  [term_count: u32]                    ← u32::MAX を超える辞書では書き出しをスキップ
  (各タームID: [prefix_id: u16][local_len: u32][local_bytes])
```

タームが 4.3B を超える場合、`dict.bin` の書き出しは非致命的スキップ（`Note: skipping legacy dict.bin` を出力）。クエリ時は `dict_sorted.bin` を使用するため動作に影響しません。

**スレッド安全な interior mutability:**  
クエリ実行時に `STR(IRI)`・`CONCAT`・`UCASE` などで生成されるリテラルを辞書に追加できるよう、`encode` は `&self` で呼び出せます。内部は `RwLock<Vec<Box<str>>>` と `RwLock<FxHashMap<String, u64>>` で実装し、`axum` のマルチスレッド環境でも安全に動作します。

```rust
// 読み取り: read lock のみ（複数スレッドが並行して実行可）
pub fn decode(&self, id: u64) -> String { ... }
pub fn lookup(&self, s: &str) -> Option<u64> { ... }

// 挿入: write lock（既存IDなら read lock だけで返す）
pub fn encode(&self, s: &str) -> u64 { ... }
```

クエリ時の追加エントリはメモリ上にのみ存在し、`dict.bin` には保存されません。

---

## 起動時キャッシュ — PredCache と PathCache

HDD ランダム I/O のコストが高い環境では、頻用述語の全ペアをあらかじめ RAM に読み込んでおくことでクエリ時の POS スキャンを完全に回避できます。EcoRDF は2種類の起動時キャッシュを提供します。

### PredCache — 単述語ペアキャッシュ (`predcache.rs`)

指定した RAM 予算の範囲で、POS インデックスから各述語の `(subject, object)` ペア全件を `Vec<(TermId, TermId)>`（ソート済み）として読み込みます。

**ロード戦略**: 述語を「ペア数 × 16 バイト」の大きい順に並べ、per-predicate cap を超えない範囲で予算を消費します。

```
budget = pred_cache_mb × 1 MiB
per-pred cap = pred_cache_per_pred_cap_mb (0 のとき pred_cache_mb / 2)

例: JPostDB, pred_cache_mb=2048, per_pred_cap_mb=200
  → faldo:begin    (11.8M × 16B = 188MB) ✓ キャッシュ
  → faldo:position (11.8M × 16B = 188MB) ✓ キャッシュ
  → jpo:someHugePred (479MB) → cap 超過のためスキップ
  → その他小述語   … 予算の残りで順次キャッシュ
```

**クエリ時の利用**:
- `executor.rs` が `PredCache::get(pred)` でヒットを確認。
- ヒットした場合: POS スキャン（HDD）を行わず、RAM 上のソート済み Vec に対して **線形マージ**（multi-hop の中間バッファとのマージ）または **バイナリサーチ**（単一プローブ）で答えを得る。
- ミスの場合: 通常の POS スキャン（mmap + ページフォルト）にフォールバック。

| フェーズ | HDD スキャン（キャッシュなし） | RAM マージ（キャッシュあり） |
|---------|------------------------------|--------------------------|
| faldo:position 1 step cold | ~13 s | ~0.4 s |
| faldo:position 1 step warm | ~6 s  | ~0.4 s |

設定: `server.pred_cache_mb` / `server.pred_cache_per_pred_cap_mb`（ecordf.toml）または `--pred-cache-mb` / `--pred-cache-per-pred-cap-mb`（CLI）。ビルドは起動時に**同期的**に行い、最初のクエリが必ずキャッシュ済み状態で処理されます。

### PathCache — 多ホップパス事前実体化 (`path_cache.rs`)

rdf-config の `model.yaml` から抽出した**複合プロパティパス**（ブランクノードを経由する述語チェーン）を、起動時に `Vec<(TermId, TermId)>`（ソート済み）として実体化します。

```
例: faldo パス [faldo:begin, faldo:position] を実体化
  手順:
  1. POS で faldo:begin 全件 → (s, m) ペア 11.8M 件
  2. POS で faldo:position 全件 → (m, o) ペア 11.8M 件
  3. 結合 (s, o) を Vec に格納・ソート
  消費メモリ ≈ 11.8M × 16B = 188MB

SPARQL クエリ時:
  ?protein faldo:begin/faldo:position ?pos
  → PathCache::get([begin_id, position_id]) でヒット
  → HDD スキャン一切なし。bind_join で各 ?protein に対して binary search → O(log M)
```

**rdf-config 統合** (`rdf_config.rs`): `prefix.yaml` + `model.yaml` を読み込み、ブランクノードを経由するパスを抽出。ローカルパスまたは GitHub ツリー URL を指定可能。

設定: `model.rdf_configs` / `model.path_cache_mb`（ecordf.toml）または `--rdf-config` / `--path-cache-mb`（CLI）。

### 3層のコスト選択ロジック

`executor.rs` の JOIN 選択は、キャッシュ状態を踏まえて3層に分岐します：

```
path_cached = PathCache にパス全体がある？
all_cached  = path_cached || (全ステップが PredCache にある？)

seek_ns = all_cached ? 2,000 ns (RAM binary search)
                     : 150,000,000 ns (HDD SPO ランダムシーク)

bind_join_cost = N_groups × path_steps × seek_ns
hash_join_cost = first_pred_range × 200 ns  (HDD seq read 120 MB/s)

use_hash = (scan_cost < bind_cost) && !path_cached
```

さらに `use_hash` かつ右辺が 2-hop 以上の Sequence パスの場合、**フィルタリング hash join** を適用します（次節）。

### フィルタリング hash join (`eval_sequence_with_subject_filter`)

通常の hash join は Sequence パスのステップ 0 を全件スキャン（例: faldo:begin 11.8M 件）し、ステップ 1 の HashMap が 11.8M エントリになります。左辺の JOIN 変数の主語集合が既知の場合、ステップ 0 の直後にフィルタリングして中間結果を大幅に削減できます。

```
通常の hash_join（Sequence [faldo:begin, faldo:position]）:
  step 0: POS(faldo:begin)  → 11.8M (s, m) ペア
  step 1: batch_scan        → 11.8M エントリの HashMap を構築 → ~18s

フィルタリング hash_join（左辺の主語集合が既知: N=508 タンパク質）:
  step 0: POS(faldo:begin)  → 11.8M (s, m) ペア
  retain: subject_filter で s ∉ {508 IDs} を除去 → 508 (s, m) ペア
  step 1: batch_scan        → 508 エントリの HashMap を構築 → ~5s

削減比: 11.8M → 508 = 約 23,000 倍
```

実装: `eval_sequence_with_subject_filter(steps, s, o, subject_filter: &HashSet<TermId>)`。  
`FILTER_SUBJECT_CAP = 100_000`：主語集合がこれを超える場合はフィルタリングのオーバーヘッドが無視できないため通常の hash join にフォールバックします。

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
| GROUP BY / HAVING / 集計 (COUNT, SUM, MIN, MAX, AVG, GROUP_CONCAT, SAMPLE) | `apply_group_by` |
| **COUNT without GROUP BY** | 全行を暗黙的1グループとして扱う（SPARQL 1.1 §11） |
| ORDER BY / LIMIT / OFFSET | 数値・文字列の型対応比較 |
| DISTINCT | 重複除去 |
| プレフィックス宣言 (PREFIX) | パーサー（**空プレフィックス `PREFIX : <iri>` も対応**） |
| 算術演算 (+, -, *, /) | `eval_term` |
| 文字列関数 (UCASE, LCASE, CONCAT, CONTAINS, STRSTARTS, STRENDS, **REPLACE**) | `eval_term` |
| 型検査 (isIRI, isLiteral, isBlank, BOUND) | `eval_bool` |
| **Property Paths** (* + ? / \| ^ ) | BFS転移閉包 + 再帰評価 |
| **GRAPH clause / Named Graphs** | GSPO索引 + `execute_named_graph` |
| **STR(IRI) のリテラル型化** | `encode` on `&self` で辞書に登録 |
| **ブランクノード存在変数** (`_:b`, `[]`, `[pred obj]`) | パース時に `Term::Variable("_:b")` へ変換 |
| **サブクエリの WHERE 省略** | `SELECT ?x { ... }` と `SELECT ?x WHERE { ... }` を同等に扱う |

### 未対応 / 制限事項

| 機能 | 状況 |
|------|------|
| CONSTRUCT | 未実装（`QueryError::Unsupported`） |
| SPARQL UPDATE (INSERT/DELETE) | 未実装 |
| SERVICE (フェデレーション) | 未実装 |
| Leapfrog の多変数完全実装 | 共有変数が2つ以上のとき hash join にフォールバック |

---

## データ入力フォーマット

| 拡張子 | フォーマット | 動作 |
|-------|------------|------|
| `.nt` / `.ntriples` | N-Triples | SPO/POS/OSPインデックスに格納 |
| `.nq` / `.nquads` | N-Quads | SPO/POS/OSP（ユニオングラフ）+ GSPO（名前付きグラフ） |
| `.nt.gz` / `.ntriples.gz` | gzip済みN-Triples | デフォルトで対応（`default = ["gzip"]`） |
| `.nq.gz` / `.nquads.gz` | gzip済みN-Quads | デフォルトで対応（`default = ["gzip"]`） |
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

両パスとも追加メモリは `HashMap<TermId, PredicateStats>`（述語数 × 32 バイト）のみ。

**ファイルフォーマット (`stats.bin`):**

```
offset  0: magic          [u8; 8]  = "ECOSTAT2"
offset  8: total_triples  u64
offset 16: n_predicates   u64
offset 24: (32バイト × n_predicates):
             pred_id       u64      ← v1 の u32 から変更
             triple_count  u64
             subject_count u64
             object_count  u64
```

旧フォーマット（`ECOSTAT1`、`pred_id` が u32）は読み込み失敗時に自動再ビルドされます。

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
│   ├── lib.rs         クレートエントリポイント
│   │                    モジュール宣言 / 公開 API (Config, InputSpec, Store, StoreStatistics)
│   ├── config.rs      設定: Config / BuildConfig / QueryConfig / ServerConfig / ModelConfig
│   │                    ecordf.toml を serde+toml でデシリアライズ
│   │                    ファイル探索順: --config > <store-dir>/ecordf.toml > デフォルト値
│   │                    BuildConfig:  chunk_size / dict_chunk_mb / parallel_threads
│   │                    ServerConfig: host / port / cors_origins / max_concurrent_queries
│   │                                  warmup_mb / pred_cache_mb / pred_cache_per_pred_cap_mb
│   │                    ModelConfig:  rdf_configs (Vec<String>) / path_cache_mb
│   ├── dict.rs        辞書 (レガシー・1パス用): 文字列 ↔ u64 ID
│   │                    RwLock による interior mutability
│   │                    19プレフィックスの名前空間圧縮
│   │                    クエリ実行時の ephemeral エントリ追加 (STR, CONCAT 等)
│   ├── dict_builder.rs 辞書ビルダー (2パス外部ソート用)
│   │                    DictBuilder    — Phase 1: チャンクバッファ→ディスク→k-wayマージ
│   │                    ReadonlyDict   — Phase 2A: mmap+バイナリサーチ+ホットキャッシュ
│   │                    DictScanner    — Phase 2B Join: dict_sorted.bin を逐次スキャン
│   │                                    8 MiB read buffer / sequential I/O / page fault ゼロ
│   │                    QueryDict      — 実行時 enum (ReadonlyDict | Dictionary)
│   │                    merge_string_chunks — 階層マージ (MAX_FAN_IN=64, EMFILE対策)
│   │                                         各レベルのバッチを Rayon で並列実行
│   │                    dict_sorted.bin フォーマット: ESRT0001 magic
│   ├── triple.rs      TermId = u64 / Triple (24B) / Quad (32B)、UNBOUND = u64::MAX
│   ├── index.rs       memmap2ベースの列指向ソート済み整数配列
│   │                    ─ 列指向フォーマット (c0/c1/c2): 各列が独立した mmap ファイル
│   │                    ─ SkipIndex: SKIP_STRIDE=512 のスパース anchor 配列
│   │                        `.skip` ファイルに永続化; 初回ビルドは c0 全スキャン
│   │                        バイナリサーチ後の絞り込み範囲 ≤ 512 エントリ (1 OS ページ)
│   │                    ─ PredicateIndex: POS 専用の pred→(lo,hi) HashMap
│   │                        `.pidx` ファイルに永続化; POS スキャン時に exact range を提供
│   │                    IndexBuilder / IndexFile (SPO・POS・OSP)
│   │                    GspoBuilder / GspoIndexFile (Named Graphs)
│   │                    AllBuilders (ビルドフェーズの統合API; 並列対応)
│   │                    spo_scan_all / pos_scan_all (統計構築用全スキャン)
│   ├── stats.rs       述語統計: StoreStatistics / PredicateStats
│   │                    build_from_index (2パスO(N)スキャン)
│   │                    save / load / load_or_build
│   │                    estimate (SP/PO/P/SPO ファンアウト推定)
│   ├── predcache.rs   述語キャッシュ: PredCache / PredPairs
│   │                    build_sync  — 起動時同期ビルド (largest-first, per-pred cap)
│   │                    build_background — バックグラウンドビルド (非推奨)
│   │                    get(pred) → Option<PredPairs> — クエリ時プローブ
│   │                    bytes_used() — 使用メモリ量
│   ├── path_cache.rs  多ホップパスキャッシュ: PathCache / PathPairs
│   │                    build(compound_paths, dict, index, budget) — 起動時実体化
│   │                    get(pred_ids: &[TermId]) → Option<PathPairs>
│   │                    bytes_used() / len()
│   ├── rdf_config.rs  rdf-config 統合: CompoundPath 抽出
│   │                    load_compound_paths(specs) — ローカルパスまたは GitHub URL を受け付ける
│   │                    prefix.yaml + model.yaml を解析してブランクノード経由パスを返す
│   ├── store.rs       ストアファサード: load / open / open_with_config / query
│   │                    dict フィールドが QueryDict (ReadonlyDict or Dictionary)
│   │                    load_with_graphs — Named Graph 対応の2パスロード
│   │                    load_with_graphs_resume_phase2 — 中断再開
│   │                    build_pred_cache_sync  — 同期述語キャッシュビルド
│   │                    build_path_cache       — rdf-config パスキャッシュビルド
│   │                    warmup_background      — OS ページキャッシュウォームアップ
│   │                    open_with_config: 起動ログ付き (dict→index→stats の各ステップを eprintln!)
│   ├── loader.rs      N-Triples / N-Quads ストリーミングパーサー (.nt/.nq/.gz デフォルト対応)
│   │                    InputSpec — ファイルパス + オプショナル Named Graph IRI
│   │                    collect_strings_from_inputs — Phase 1 (シングルスレッド)
│   │                    collect_strings_parallel     — Phase 1 (rayon 並列)
│   │                    load_triples_with_readonly_dict — Phase 2A (シングル)
│   │                    load_triples_parallel           — Phase 2A (rayon 並列, mmap)
│   │                    load_triples_streaming          — Phase 2B (Streaming, term_count>1B)
│   │                      collect_strings_for_file_sorted — Phase 2a: ファイル別sorted文字列収集
│   │                      join_batch_with_dict            — Join: dict逐次スキャン→LocalMap構築
│   │                      load_triple_from_one_input_local — Phase 2b: LocalMap O(1)ルックアップ
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
│   │                    3層コスト選択: path_cached / all_cached / use_hash
│   │                    eval_sequence_with_subject_filter — フィルタリング hash join
│   │                      (FILTER_SUBJECT_CAP=100,000; step 0 後に subject 集合で絞り込み)
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
│                        build: --resume-phase2 (中断再開)
│                        serve: --host / --port / --cors / --config / --warmup-mb
│                               --pred-cache-mb / --pred-cache-per-pred-cap-mb
│                               --rdf-config / --path-cache-mb
│                        query: --config / "-" (stdin読み込み)
├── ecordf.toml        設定ファイルのサンプル（全パラメータ・説明付き）
├── DESIGN.md          本ドキュメント
└── Cargo.toml
```

---

## ビルドと使用方法

### ビルド

```bash
cd ecordf
cargo build --release          # gzip対応込み（デフォルト）

# gzip対応を除外する場合
cargo build --release --no-default-features
```

### データ読み込み

```bash
# 直接ファイルを指定（少数ファイルの場合）—ユニオングラフのみ
./target/release/ecordf build \
  --dir ./uniprot-store \
  uniprot_sprot.nt uniprot_trembl.nt

# N-Quads（named graph付き）
./target/release/ecordf build \
  --dir ./togo-store \
  togoid.nq

# --from-file: ファイルリストをテキストファイルで指定
# グラフ名なし（ユニオングラフのみ）
cat > inputs.txt << 'EOF'
# UniProt release 2024_04
/data/uniprot/uniprot_sprot.nt.gz
/data/uniprot/uniprot_trembl.nt.gz
/data/uniparc/uniparc.nt.gz
EOF
./target/release/ecordf build --dir ./uniprot-store --from-file inputs.txt

# N-Quads ファイルを用意せず、N-Triplesファイルにグラフ名を紐付け
# 2列目にグラフIRI（< > あり・なし両方OK）
cat > graphs.txt << 'EOF'
# ファイルパス  グラフIRI
/data/uniprot_sprot.nt.gz    <http://sparql.uniprot.org/uniprot>
/data/go.nt.gz               <http://sparql.uniprot.org/go>
/data/taxonomy.nt.gz         http://sparql.uniprot.org/taxonomy
/data/shared.nt              # グラフ名なし → ユニオングラフのみ
EOF
./target/release/ecordf build --dir ./store --from-file graphs.txt

# find などのパイプ（グラフ名なしの場合）
find /data -name '*.nt.gz' | \
  ./target/release/ecordf build --dir ./store --from-file -

# 複数リストファイルと直接指定の混在も可
./target/release/ecordf build --dir ./store \
  --from-file core.txt --from-file optional.txt \
  extra.nt

# 読み込み完了後:
# Built store: 142357891 triples, 28456123 terms, 5 named graphs

# ビルドが中断した場合は --resume-phase2 で再開（dict_sorted.bin が存在すれば Phase 1 スキップ）
./target/release/ecordf build --dir ./store --resume-phase2 --from-file inputs.txt
```

ロードされたグラフは `GRAPH` 句でアクセスできます：

```sparql
# 特定グラフ内のみ
SELECT ?s ?p ?o WHERE {
  GRAPH <http://sparql.uniprot.org/uniprot> { ?s ?p ?o }
} LIMIT 10

# 全グラフを横断
SELECT ?g ?s ?o WHERE {
  GRAPH ?g { ?s a <http://purl.uniprot.org/core/Protein> }
} LIMIT 10
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
| `build.chunk_size` | 5,000,000 | 外部ソートのトリプルチャンクサイズ（0=レガシー1パス） |
| `build.dict_chunk_mb` | 200 | Phase 1 文字列バッファの RAM 上限（MiB） |
| `build.parallel_threads` | 0 | 並列ロードスレッド数（0=全 CPU コア） |
| `query.max_intermediate_rows` | 50,000,000 | 中間結果の行数上限（OOM防止） |
| `query.bind_join_threshold` | 10,000 | bind_join / hash_join の切り替え閾値 |
| `server.host` | `127.0.0.1` | バインドアドレス |
| `server.port` | `7878` | TCPポート |
| `server.cors_origins` | `""` | CORS設定（`"*"` or カンマ区切りオリジン） |
| `server.max_concurrent_queries` | `0` | 同時クエリ数上限（0=無制限） |
| `server.warmup_mb` | `0` | 起動直後にバックグラウンドでページキャッシュへ読み込む MB 数（0=無効）。`--warmup-mb` で上書き可 |
| `server.pred_cache_mb` | `1024` | 述語キャッシュの RAM 予算（MiB）。0=無効。`--pred-cache-mb` で上書き可 |
| `server.pred_cache_per_pred_cap_mb` | `0` | 述語ごとの上限（MiB）。0=`pred_cache_mb/2`。巨大述語が予算を占拠するのを防ぐ。`--pred-cache-per-pred-cap-mb` で上書き可 |
| `model.rdf_configs` | `[]` | rdf-config ディレクトリまたは GitHub URL のリスト。PathCache の材料。`--rdf-config` で上書き可 |
| `model.path_cache_mb` | `0` | パスキャッシュの RAM 予算（MiB）。0=無効。`--path-cache-mb` で上書き可 |

### HTTPサーバー

```bash
# ローカル起動（設定はecordf.tomlまたはデフォルト値）
./target/release/ecordf serve --dir ./jpostdb-store

# CLIフラグはconfigファイルより優先
./target/release/ecordf serve --dir ./jpostdb-store --host 0.0.0.0 --port 8080

# CORS許可（全オリジン）
./target/release/ecordf serve --dir ./jpostdb-store --cors '*'

# コールドスタート対策: 起動直後に 8 GB 分のインデックスをページキャッシュへ読み込む
./target/release/ecordf serve --dir ./uniprot-store --warmup-mb 8192

# 述語キャッシュ: faldo:begin/position (各 188 MB) をキャッシュ、巨大述語は除外
# --pred-cache-per-pred-cap-mb 200 により 479 MB 超の述語をスキップ
./target/release/ecordf serve --dir ./jpostdb-store \
  --pred-cache-mb 2048 \
  --pred-cache-per-pred-cap-mb 200

# rdf-config パスキャッシュ: faldo パスを 512 MB 予算で事前実体化
./target/release/ecordf serve --dir ./jpostdb-store \
  --pred-cache-mb 2048 --pred-cache-per-pred-cap-mb 200 \
  --rdf-config https://github.com/dbcls/rdf-config/tree/master/config/jpostdb \
  --path-cache-mb 512

# 起動ログ例:
#   Opening dictionary...
#   Opening indexes...
#     indexes opened in 2.31s
#   Loading statistics...
#   Opened store: 11,823,456 triples, 3,201,847 terms
#   Building predicate cache (2048 MB, per-predicate cap = 200 MB)...
#   Predicate cache ready (1842 MB used).
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

> 他システムとの定量比較は実測ベンチマークによるべきであり、ここでは EcoRDF の設計上の特徴のみを示します。

| 特性 | EcoRDF の動作 |
|------|-------------|
| 起動時間 | mmap のため index オープン自体は即時。ただし `open_with_config` 内でディスクから `.skip`/`.pidx`/`stats.bin` を読み込むため、コールドキャッシュ時は数秒かかる（ログで各ステップを可視化）。`--warmup-mb` でバックグラウンドウォームアップを有効化するとコールドスタート後の初回クエリが速くなる |
| 起動時 RAM | PredCache: `pred_cache_mb` MiB（同期ビルド）。PathCache: `path_cache_mb` MiB（同期ビルド）。設定 0 の場合ゼロ |
| クエリ時 RAM | ワーキングセット依存（OS ページキャッシュで管理） |
| ビルド時ピーク RAM | 2パス外部ソートにより定数。Phase 1: `dict_chunk_mb × スレッド数` MB。Phase 2A: `chunk_size × 72B`。Phase 2B (Streaming): Phase 1 と同予算（`dict_chunk_mb × スレッド数`）|
| 並列クエリ処理 | 対応（tokio blocking pool） |
| Property Path (* 転移閉包) | BFS（対応済） |
| Named Graphs (GRAPH句) | GSPO索引（対応済） |

### 並列クエリ処理

1クエリの内部処理はシングルスレッドですが、**複数クエリは並列に処理されます**。

```
tokio worker threads (num_cpus):  HTTPコネクション管理・I/O
tokio blocking pool (最大512):    クエリ実行 (spawn_blocking)
Semaphore (max_concurrent_queries): アプリ層の同時数キャップ
```

各クエリは `spawn_blocking` でブロッキング専用スレッドプールに委譲されるため、非同期ワーカースレッドをブロックしません。`max_concurrent_queries = 0`（デフォルト）の場合はtokioの上限（512）まで並列実行できます。重いクエリが多い場合は `max_concurrent_queries = 2 × CPUコア数` を推奨します。

### スケール時の制約

- **1クエリの内部処理**: シングルスレッド。広域スキャンが絡む単一クエリでは内部並列実行を持つシステムに劣る場合があります。
- **Leapfrog**: 共有変数が2つ以上のパターンはハッシュジョインにフォールバックします（完全な多変数 Leapfrog は今後の課題）。
- **クエリ時辞書**: `ReadonlyDict`（2パス構築後）はホットキャッシュ (≤ 4M エントリ / ~400 MB) + mmap で動作します。レガシー1パス時は `dict.bin` を全件 HashMap に展開するためデータセット規模に比例します。

---

## 今後の拡張候補

1. **SPARQL UPDATE** — INSERT DATA / DELETE DATA / MODIFY
2. **クエリ内部の並列化** — rayon による JOIN内部のスレッド並列化（クエリ間並列は実装済み、ファイルロードの並列化は実装済み）
3. **Leapfrog 多変数完全実装** — 共有変数が2つ以上の場合もLeapfrogで処理
4. **SPARQL Federation** — SERVICE句による外部エンドポイント連携
5. **Block圧縮** — zstdでディスク使用量をさらに削減
6. **CONSTRUCT** — RDFグラフを返すクエリ形式
7. **クエリタイムアウト** — 長時間クエリを自動キャンセルする機能
8. **HyperLogLog** — subject_count / object_count の近似カウント（現在は2パス全スキャン）
9. **BOUND_VAR_FACTOR の自動調整** — 述語の distinctiveness から動的に推定倍率を計算
10. **ReadonlyDict のキャッシュ戦略改善** — ホットキャッシュを LRU 化して偏りのあるアクセスパターンに対応
11. **PathCache の自動 TermId 解決** — 現状は起動後に rdf_config を解析して IRI→TermId を引くが、辞書ミス時の fallback をより堅牢に
12. **フィルタリング hash join の対象拡大** — 現状は 2-hop Sequence のみ。3-hop 以上や Alternative パスにも適用を検討
