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

### ブランクノード再番号付け — `reorder_bnodes`

ビルド完了後、`reorder_bnodes = true`（デフォルト）のとき、ブランクノードの TermId を型ごとに連番に割り振り直し、全6インデックスをまとめて書き換えます。

**目的:**
- **インデックス一貫性**: 全6インデックスが同一エポックの TermId を共有するため、OPS/PSO/SOP の検索結果と SPO/POS/OSP の結果が正確に突き合わせられる。一部インデックスのみ更新すると TermId エポックが混在し、大述語（OPS ルーティング閾値 = `total_triples / 1000` 超）で JOIN 結果が壊れる。
- **圧縮効率**: ブランクノードが連番になるため、delta 符号化の効率が向上（特に c2 列）。

**成果物:** `bnode_remap.bin`（旧 TermId → 新 TermId の写像、u64ペア配列）。

`auto_compress = true` と組み合わせると、build → reorder_bnodes → ECOCOL04 圧縮の全ステップが1プロセスで完結します。

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

SPO/POS/OSP の3インデックスを基本とし、オプションで PSO/SOP/OPS の追加インデックスを持ちます。各インデックスは**列指向フォーマット**（3カラム分離ファイル）に加え、スパースな in-memory SkipIndex と、POS 専用の PredicateIndex を持ちます。

```
SPO索引:  主語でソート → 特定エンティティの全述語・目的語を高速取得
  spo.c0.zst / spo.c1.zst / spo.c2.zst  (ECOCOL04: delta+Zstd)
  spo.c0.skip (SkipIndex: 8B × ⌈N/512⌉)

POS索引:  述語でソート → 生命科学クエリの大半（型・関係の絞り込み）
  pos.c0.zst, pos.c1.zst, pos.c2.zst, pos.skip
  pos.pidx (PredicateIndex: 24B × 述語数)

OSP索引:  目的語でソート → 値や概念からの逆引き
  osp.c0.zst, osp.c1.zst, osp.c2.zst, osp.skip

PSO/SOP/OPS索引: デフォルトでビルド済み（ファイル不在の場合のみスキップ）
  PSO: 主語・述語同時束縛を効率的に処理
  SOP: 主語・目的語同時束縛を効率的に処理
  OPS: 目的語・述語同時束縛に特化。大述語（triple_count > total/1000）は
       OPS にルーティングされるため、全6インデックスが同一 TermId エポックを
       持つことが必須（→ reorder_bnodes で保証）

GSPO索引 (gspo.bin): グラフ+SPO → Named Graphs / N-Quads 対応（存在する場合のみ）
```

各カラムは `u64` 配列を memmap2 でマップし、クエリ時は `scan()` / `scan_graph()` で範囲走査します（GSPO は先頭に `g: u64` を加えた 4カラム）。

インデックスファイルの優先順位（`IndexFile::open`）:
```
1. .c0.zst / .c1.zst / .c2.zst  (ECOCOL04: delta+Zstd)  ← 最優先
2. .c0.dz  / .c1.dz  / .c2.dz   (ECOCOL02/03: delta)
3. .c0     / .c1     / .c2       (ECOCOL01: raw)
```

### カラム圧縮フォーマット（col_delta.rs）

各カラムファイルは3世代のフォーマットを経て進化しています。

| フォーマット | ファイル拡張子 | magic | 説明 |
|------------|-------------|-------|------|
| ECOCOL01 | `.c0` / `.c1` / `.c2` | `ECOCOL01` | 生の u64 配列（memmap 直読） |
| ECOCOL02 | `.c0.dz` | `ECOCOL02` | デルタ符号化ブロック（256値/ブロック、固定長） |
| ECOCOL03 | `.c0.dz` | `ECOCOL03` | デルタ符号化ブロック（述語境界でブロック分割、可変長） |
| ECOCOL04 | `.c0.zst` | `ECOCOL04` | デルタ符号化後に Zstd ブロック圧縮（64ブロック=16384値/Zstdチャンク） |

**ECOCOL02 のデルタ符号化:**

各ブロック内の最大デルタ幅に応じて最小ビット幅を選択します。

```
ENC_ALL_SAME (0): 全値が同じ  → 1バイト/ブロック (256× 圧縮)
ENC_U8       (1): delta ≤ 255 → 1バイト/値
ENC_U16      (2): delta ≤ 65535 → 2バイト/値
ENC_U32      (3): delta ≤ 2^32-1 → 4バイト/値
ENC_U64      (4): それ以外 → 8バイト/値（生値格納）
```

**c2 列が圧縮されにくい理由とECOCOL04 の効果:**

c2 列（3番目のソートキー）はグループ境界でリセットが生じます。
例: SPO 順では同一 (s,p) グループ内は昇順ですが、次の (s,p) グループの先頭値は
前グループの最終値より小さいことがあります。`v - base` が折り返し演算になり
`delta > u32::MAX` → `ENC_U64`（8バイト/値）となります。

ECOCOL04 は delta 符号化後のバイト列を Zstd で再圧縮します。
TermId は最大 254M（< 2^28）なので上位4バイトが常にゼロ（またはほぼゼロ）です。
Zstd はこの0バイトパターンを効率良く検出し約2:1の圧縮を達成します。
さらに述語列（c0）など元から小さいデルタを持つ列では10-30倍の圧縮に達します。

**ECOCOL04 ファイルフォーマット:**

```text
offset  0: magic               [u8; 8] = b"ECOCOL04"
offset  8: count               u64     (総 u64 値数)
offset 16: block_count         u64     (デルタブロック数 = ceil(count / 256))
offset 24: blocks_per_chunk    u64     (Zstd チャンクあたりのブロック数, デフォルト 64)
offset 32: chunk_count         u64     (ceil(block_count / blocks_per_chunk))
offset 40: idx_offset          u64     (チャンクインデックスのバイトオフセット)
offset 48: ── Zstd チャンク (可変長) ─────────────────────────────────
  各チャンク: 独立した Zstd フレーム
    中身 = ECOCOL02 形式のデルタブロック × blocks_per_chunk 個のバイト列
at idx_offset:
  [(first_value: u64, byte_offset: u64, compressed_size: u32, n_blocks: u32) × chunk_count]
```

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
| 3,000万トリプル | ~240 MB | ~469 KB |
| 8億トリプル | ~6.4 GB | ~10 MB |

SkipIndex は `.skip` ファイルに永続化し、次回起動は `read_to_end` で即座にロードします（最初のビルド時のみ c0 の全スキャンが走りますが、ログにメッセージが出ます）。

### PredicateIndex (.pidx) — POS 述語→範囲マップ

POS インデックスの各述語について `[lo, hi)` のエントリ範囲を in-memory HashMap に保持します。

```
ファイルフォーマット: magic(8B) + pred_count(8B) + entries[(pred:u64, lo:u64, hi:u64) × pred_count]
エントリサイズ: 24 バイト × 述語数
述語数百の場合 → ~数十 KB
```

POS スキャン時に述語が定数であれば、PredicateIndex を参照して **c0 全体を走査せずに** 正確な `[lo, hi)` 範囲を取得できます（SkipIndex の upper-bound ではなく exact range）。

**ディスク使用量の概算（ECOCOL04 / 全6インデックス）:**

| トリプル数 | インデックス6種（.zst）| 辞書（dict_sorted.bin）| GSPO付き | 合計目安 |
|-----------|----------------------|----------------------|---------|--------|
| 1,000万   | ~200 MB              | ~200 MB              | +180 MB | ～600 MB |
| 1億       | ~2 GB                | ~2 GB                | +1.8 GB | ～6 GB   |
| 8億       | ~8.3 GB              | ~13 GB               | +26 GB  | ～50 GB  |

列ごとの圧縮効果（目安）:

| 列 | 特徴 | 圧縮率（ECOCOL04）|
|---|---|---|
| c0（1キー目） | 長連続 → delta が小さい | 10〜100× |
| c1（2キー目） | グループ内で昇順 | 3〜20× |
| c2（3キー目） | グループ境界でリセット → delta が大きい | 1.5〜5× |
| 述語列（POS/PSO の c0）| 述語数が少なく同一値の連続が非常に長い | 100×以上 |

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

## 起動時キャッシュ — PredCache・PathCache・TypeCache

HDD ランダム I/O のコストが高い環境では、頻用述語の全ペアをあらかじめ RAM に読み込んでおくことでクエリ時の POS スキャンを完全に回避できます。EcoRDF は3種類の起動時キャッシュを提供します。

### PredCache — 単述語ペアキャッシュ (`predcache.rs`)

指定した RAM 予算の範囲で、POS インデックスから各述語の `(subject, object)` ペア全件を `Vec<(TermId, TermId)>`（ソート済み）として読み込みます。

**ロード戦略**: 述語を「ペア数 × 16 バイト」の大きい順に並べ、per-predicate cap を超えない範囲で予算を消費します。

```
budget = pred_cache_mb × 1 MiB
per-pred cap = pred_cache_per_pred_cap_mb (0 のとき pred_cache_mb / 2)

例: pred_cache_mb=2048, per_pred_cap_mb=200
  → faldo:begin    (N ペア × 16B ≤ 200MB) ✓ キャッシュ
  → faldo:position (N ペア × 16B ≤ 200MB) ✓ キャッシュ
  → rdf:type や巨大述語 (> 200MB) → cap 超過のためスキップ
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
  1. POS で faldo:begin 全件 → (s, m) ペア N 件
  2. POS で faldo:position 全件 → (m, o) ペア N 件
  3. 結合 (s, o) を Vec に格納・ソート
  消費メモリ ≈ N × 16B

SPARQL クエリ時:
  ?protein faldo:begin/faldo:position ?pos
  → PathCache::get([begin_id, position_id]) でヒット
  → HDD スキャン一切なし。bind_join で各 ?protein に対して binary search → O(log M)
```

**rdf-config 統合** (`rdf_config.rs`): `prefix.yaml` + `model.yaml` を読み込み、ブランクノードを経由するパスを抽出。ローカルパスまたは GitHub ツリー URL を指定可能。

設定: `model.rdf_configs` / `model.path_cache_mb`（ecordf.toml）または `--rdf-config` / `--path-cache-mb`（CLI）。

### TypeCache — rdf:type クラス帰属キャッシュ (`type_cache.rs`)

`rdf:type` トリプルを RoaringTreemap（u64 対応の圧縮ビットマップ）として保持します。
各クラスについて「このクラスのインスタンスである TermId の集合」を RAM に持ち、
`?x a SomeClass` フィルタを O(1) lookup に変換します。

```
up:Protein      → RoaringTreemap { ~560K subjects }  ~1 MB
SomeClass       → RoaringTreemap { 数百万 subjects } ~数 MB
```

設定: `server.type_cache_mb`（デフォルト 256 MB）。

### キャッシュの永続化

PathCache と TypeCache はビルドに時間がかかるため、初回起動時にストアディレクトリへ
バイナリファイルとして保存し、2回目以降は高速ロードします。

| キャッシュ | ファイル | 初回ビルド時間 | 2回目以降ロード |
|----------|---------|------------|-------------|
| PathCache | `path_cache.bin` | 数十秒（パス数・トリプル数依存） | 数秒 |
| TypeCache | `type_cache.bin` | 数十秒（クラス数・トリプル数依存） | ~1s |
| PredCache | なし（毎回ビルド） | pred_cache_mb 量依存 | — (永続化の効果なし) |

PredCache は非圧縮データが大きく、ファイルからの読み込みよりビルドが速いため
永続化を行いません。インデックスが更新された場合（`ecordf build` 再実行後）、
参照インデックスファイル（`spo.c0` 等）の mtime と比較し自動的に再ビルドします。

### 3層のコスト選択ロジック

`executor.rs` の JOIN 選択は、キャッシュ状態を踏まえて3層に分岐します：

```
path_cached = PathCache にパス全体がある？
all_cached  = path_cached || (全ステップが PredCache にある？)

seek_ns = all_cached ? 50,000,000 ns (RAM binary search; 188MB 配列は L3 に収まらず DRAM アクセス)
                     : 150,000,000 ns (HDD/SSD SPO ランダムシーク)

bind_join_cost = N_groups × path_steps × seek_ns
hash_join_cost = first_pred_range × 200 ns  (HDD seq read 120 MB/s)

use_hash = (scan_cost < bind_cost)

if path_cached && N_left < 100_000:
    → path_cache_merge_join（binary search 戦略: N×log M アクセス）
elif path_cached && N_left >= 100_000:
    → path_cache_merge_join（linear scan 戦略: M アクセス）
elif use_hash:
    → filtered hash_join
else:
    → bind_join
```

さらに `use_hash` かつ右辺が 2-hop 以上の Sequence パスの場合、**フィルタリング hash join** を適用します（次節）。

### フィルタリング hash join (`eval_sequence_with_subject_filter`)

通常の hash join は Sequence パスのステップ 0 を全件スキャン（例: faldo:begin の全 N 件）し、ステップ 1 の HashMap が N エントリになります。左辺の JOIN 変数の主語集合が既知の場合、ステップ 0 の直後にフィルタリングして中間結果を大幅に削減できます。

```
通常の hash_join（Sequence [faldo:begin, faldo:position]）:
  step 0: POS(faldo:begin)  → N (s, m) ペア
  step 1: batch_scan        → N エントリの HashMap を構築

フィルタリング hash_join（左辺の主語集合が既知: K 件、K << N）:
  step 0: POS(faldo:begin)  → N (s, m) ペア
  retain: subject_filter で s ∉ {K IDs} を除去 → K (s, m) ペア
  step 1: batch_scan        → K エントリの HashMap を構築

削減比: N/K 倍（K が小さいほど効果大）
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
| **FILTER (IN / NOT IN)** | `Expression::In` / `NotIn`; eval_bool で TermId または文字列比較 |
| **FILTER EXISTS { } / NOT EXISTS { }** | `Expression::Exists` / `NotExists`; サブパターンを outer binding で実行（相関評価）|
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
| Leapfrog streaming カーソル | 多変数対応は実装済み。ただし現実装は Vec 全量 collect のため巨大述語では OOM リスクあり（`LftCursor` への置き換えが課題）|
| FILTER NOT EXISTS の最適化 | 現在は相関評価（全行についてサブパターンを逐次実行）。大規模データではアンチジョインへの変換が必要 |
| STRDT / STRLANG / BNODE / RAND / UUID / SHA系ハッシュ | 未実装 |

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
│   ├── col_delta.rs   デルタ+Zstd列圧縮フォーマット
│   │                    ECOCOL02: デルタ符号化ブロック (256値/ブロック、固定長)
│   │                    ECOCOL03: 述語境界アライン版（pos.c1等で効率向上）
│   │                    ECOCOL04: delta + Zstd ブロック圧縮 (.c0.zst/.c1.zst/.c2.zst)
│   │                      64ブロック(16384値)を Zstd フレームに圧縮
│   │                      TermId の上位バイトが常に0であることを Zstd が検出 → 2-20× 追加圧縮
│   │                    DeltaColFile::open — magic 検出でフォーマット自動判別
│   │                    encode_column / encode_column_pred_aligned / encode_column_zstd
│   │                    delta_path(p) → p+".dz"  /  zstd_path(p) → p+".zst"
│   ├── index.rs       memmap2ベースの列指向ソート済み整数配列
│   │                    ─ 列指向フォーマット (c0/c1/c2): 各列が独立した mmap ファイル
│   │                       優先順位: .zst (ECOCOL04) > .dz (ECOCOL02/03) > raw (.c0)
│   │                    ─ PSO/SOP/OPS: Option<IndexFile>（ファイル不在なら None）
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
│   │                    get(pred) → Option<PredPairs> — クエリ時プローブ
│   │                    bytes_used() — 使用メモリ量
│   │                    ※永続化なし（6.4GB 非圧縮はファイル読み込みよりビルドが速いため）
│   ├── path_cache.rs  多ホップパスキャッシュ: PathCache / PathPairs
│   │                    build(compound_paths, dict, index, budget) — 起動時実体化
│   │                    get(pred_ids: &[TermId]) → Option<PathPairs>
│   │                    bytes_used() / len()
│   │                    save_to_file / load_from_file — 永続化 (path_cache.bin)
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
│   └── main.rs        CLI: build / serve / query / stats / compress-cols / recompress-zstd
│                        build: --resume-phase2 / --auto-compress
│                        serve: --host / --port / --cors / --config / --warmup-mb
│                               --pred-cache-mb / --pred-cache-per-pred-cap-mb
│                               --rdf-config / --path-cache-mb
│                        compress-cols: --zstd (delta 後に Zstd も適用)
│                        recompress-zstd: --ordering (特定 ordering のみ処理)
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
| `build.auto_compress` | false | ビルド後に delta + Zstd 圧縮（ECOCOL04、`.c0.zst`）を自動適用。HDD/SSD 環境で推奨 |
| `query.max_intermediate_rows` | 50,000,000 | 中間結果の行数上限（OOM防止） |
| `query.bind_join_threshold` | 10,000 | bind_join / hash_join の切り替え閾値 |
| `server.host` | `127.0.0.1` | バインドアドレス |
| `server.port` | `7878` | TCPポート |
| `server.cors_origins` | `""` | CORS設定（`"*"` or カンマ区切りオリジン） |
| `server.max_concurrent_queries` | `0` | 同時クエリ数上限（0=無制限） |
| `server.warmup_mb` | `0` | 起動後バックグラウンドでページキャッシュへ読み込む MB 数 |
| `server.pred_cache_mb` | `1024` | 述語キャッシュの RAM 予算（MiB）。0=無効 |
| `server.pred_cache_per_pred_cap_mb` | `0` | 述語ごとの上限（0=`pred_cache_mb/2`） |
| `server.type_cache_mb` | `256` | TypeCache (rdf:type RoaringTreemap) の RAM 予算（MiB）。0=無効 |
| `model.rdf_configs` | `[]` | rdf-config ディレクトリまたは GitHub URL のリスト。PathCache の材料 |
| `model.path_cache_mb` | `0` | パスキャッシュの RAM 予算（MiB）。0=無効 |

### HTTPサーバー

```bash
# ローカル起動（設定はecordf.tomlまたはデフォルト値）
./target/release/ecordf serve --dir ./store

# CLIフラグはconfigファイルより優先
./target/release/ecordf serve --dir ./store --host 0.0.0.0 --port 8080

# CORS許可（全オリジン）
./target/release/ecordf serve --dir ./store --cors '*'

# コールドスタート対策: 起動直後に 8 GB 分のインデックスをページキャッシュへ読み込む
./target/release/ecordf serve --dir ./store --warmup-mb 8192

# 述語キャッシュ: faldo:begin/position をキャッシュ、巨大述語は per-pred-cap で除外
./target/release/ecordf serve --dir ./store \
  --pred-cache-mb 2048 \
  --pred-cache-per-pred-cap-mb 200

# rdf-config パスキャッシュ: faldo パスを 512 MB 予算で事前実体化
./target/release/ecordf serve --dir ./store \
  --pred-cache-mb 2048 --pred-cache-per-pred-cap-mb 200 \
  --rdf-config https://github.com/dbcls/rdf-config/tree/master/config/YOUR_DB \
  --path-cache-mb 512

# 起動ログ例:
#   Opening dictionary...
#   Opening indexes...
#     indexes opened in 2.31s
#   Loading statistics...
#   Opened store: 142,357,891 triples, 28,456,123 terms
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
| ビルド時ピーク RAM | 2パス外部ソートにより定数。Phase 1: `dict_chunk_mb × スレッド数` MB。Phase 2A: `chunk_size × 72B`。Phase 2B (Streaming): Phase 1 と同予算（`dict_chunk_mb × スレッド数`）。`auto_compress = true` 時は追加で Zstd 圧縮バッファ（≤ 数十 MB）|
| 並列クエリ処理 | 対応（tokio blocking pool） |
| Property Path (* 転移閉包) | BFS（対応済） |
| Named Graphs (GRAPH句) | GSPO索引（対応済） |

### 並列クエリ処理

**クエリ間並列**と**クエリ内部の部分的並列化**の両方を実装しています。

```
tokio worker threads (num_cpus):  HTTPコネクション管理・I/O
tokio blocking pool (最大512):    クエリ実行 (spawn_blocking)
Semaphore (max_concurrent_queries): アプリ層の同時数キャップ
```

各クエリは `spawn_blocking` でブロッキング専用スレッドプールに委譲されるため、非同期ワーカースレッドをブロックしません。`max_concurrent_queries = 0`（デフォルト）の場合はtokioの上限（512）まで並列実行できます。

**クエリ内部の並列化（rayon）:**

| 処理 | 実装 |
|------|------|
| ORDER BY ソート | `par_sort_unstable_by`（大結果セットで ~4× 高速化） |
| hash_join probe フェーズ | `par_iter().flat_map()` による並列プローブ |
| UNION ブランチ（トップレベル）| `rayon::join` で両ブランチを並列実行 |

並列ブランチ実行には `Executor::fork()` を使用し、全インデックス・キャッシュを Arc クローンで共有しつつスレッドローカル状態（`scan_dontneed_bytes`, `pushdown_limit`）をリセットします。

### スケール時の制約

- **1クエリの内部処理**: ORDER BY ソート・hash_join probe フェーズ・UNION ブランチは rayon で並列化済み。ただし LFTJ 列挙など主要な実行ループはシングルスレッドのまま。
- **Leapfrog の Vec collect**: 多変数対応は実装済み（`lft_enumerate` による再帰的深さ優先列挙）。ただし各パターンの候補値を `Vec` に全量 collect してから交差するため、巨大述語の直接スキャンが必要なパターンでは OOM リスクがあります。
- **クエリ時辞書**: `ReadonlyDict`（2パス構築後）はホットキャッシュ (≤ 4M エントリ / ~400 MB) + mmap で動作します。レガシー1パス時は `dict.bin` を全件 HashMap に展開するためデータセット規模に比例します。

---

---

## インデックス改善 (2024実装)

以下の6つの改善をまとめて実装しました。再インデックスコストを考慮して1回の実装にまとめています。

### 改善1: 述語境界アライン Delta エンコーディング (`col_delta.rs` + `index.rs`)

`encode_column_pred_aligned(values, boundaries, path)` を追加。`ecordf compress-cols` の `pos.c1` / `pso.c1` (述語内オブジェクト列) に述語境界でブロックを強制分割してから圧縮。

```
従来: 1ブロックが述語Aの末尾+述語Bの先頭をまたぐ → max_delta が2述語分のO範囲 → U32/U64 エンコード
改善後: ブロックは常に1述語内に収まる → max_delta が当該述語のO範囲のみ → U8/U16 エンコードに改善
```

**実装**: `compress_columns()` が `.pidx` から境界を読み取り、`pos.c1`/`pso.c1` に `encode_column_pred_aligned` を適用。他の列は従来通り `encode_column`。

### 改善2: PFOR-Delta ✅ (既実装)

`col_delta.rs` (ECOCOL02) で実装済み。`ALL_SAME` (述語列に256×圧縮), `U8/U16/U32/U64` 最小幅デルタ。

### 改善3: Roaring Bitmap TypeCache (`type_cache.rs` + `Cargo.toml`)

`TypeCache` の `HashMap<TermId, Vec<TermId>>` を `HashMap<TermId, RoaringTreemap>` に変更。

| 操作 | Vec + binary_search | RoaringTreemap |
|-----|---------------------|----------------|
| `contains()` | O(log N) | O(1) |
| 2クラス積集合 | O(N) マージ | SIMD AND, O(N/64) |
| メモリ (3.7M subjects) | 28 MB | 2–4 MB |

**実装**: `get_bitmap(class_id) -> Option<&RoaringTreemap>` を追加。executor.rs を `get_bitmap` + `bm.contains(s)` に更新。`roaring = "0.10"` を `Cargo.toml` に追加。

### 改善4: 二段 SkipIndex (`index.rs`)

`SkipIndex` に L2 アンカーを追加 (`SKIP_STRIDE_L2 = 512² = 262144`)。

```
L2 anchors: 3Bトリプルで ~11,444 × 8B ≈ 91 KB → L2 CPU キャッシュに収まる
L1 anchors: 3Bトリプルで ~5.86M × 8B ≈ 47 MB (キャッシュ外)

2段 narrow(): L2 binary search (14回 L2キャッシュヒット) → L1 window (512エントリ, 4KB) → 1 page fault
1段 narrow(): L1 binary search (23回, 47MBからランダム) → 1 page fault
```

`.skip` ファイルフォーマットを `ECOSKIP1` (v1) → `ECOSKIP2` (v2) に更新。v1 の自動読み込みも対応（L2は L1から再導出）。

### 改善5: SIP (Sideways Information Passing) 汎化 (`executor.rs`)

既存の `eval_sequence_with_subject_filter` に加え、hash_join パス全体に SIP 事前フィルタを追加。

**既存 SIP**: 2-hop Sequence パスの step 0 → step 1 削減（N → K、K << N）

**新規 SIP**: `hash_join` 選択時、`right_rs` を実行後・結合前にフィルタリング。

```
条件: left_rs.rows < right_rs.rows / 10 かつ共有変数ごとの left値集合 ≤ 100,000
効果: right_rs から join 不可能な行を除去 → hash_join のメモリと処理コストを削減
例: left=K タンパク質, right=大量 rdf:type → right を K 行にフィルタ後 join
```

SIP_MAX_LEFT_VALUES = 100,000。超過時はフィルタをスキップして従来の hash_join へ。

### 改善6: 機能的述語の自動検出と最適化 (`stats.rs` + `pred_partition.rs` + `executor.rs`)

`StoreStatistics::is_functional(pred_id)` を追加。`triple_count ≤ subject_count × 1.05` の述語を機能的と判定。

`PredPartFile::get_single_object(s) -> Option<TermId>` を追加。機能的述語の S → O 直接ルックアップ (O(log N)、Vec 割り当てなし)。

executor.rs の `bind_join` 内 pred_partition パスで、機能的述語検出時に `get_single_object` を使用:

```
非機能的述語: get_objects(s) → &[(S,O)] → Vec 割り当て → HashMap
機能的述語:   get_single_object(s) → Option<TermId> → 直接 HashMap 挿入
```

---

## 今後の拡張候補

1. **SPARQL UPDATE** — INSERT DATA / DELETE DATA / MODIFY
2. **Leapfrog streaming カーソル** — 現在の `lft_enumerate` は各パターンから値を `Vec` に全量 collect してから `leapfrog_join` する。巨大述語では OOM のリスクがある。`LftCursor` によるストリーミング交差（Vec 確保ゼロ）への置き換えが必要
3. **SPARQL Federation** — SERVICE 句による外部エンドポイント連携
5. **CONSTRUCT** — RDF グラフを返すクエリ形式
6. **FILTER NOT EXISTS のアンチジョイン最適化** — 現在は相関評価。大規模データには LEFT ANTI JOIN への変換が必要
7. **STRDT / STRLANG / BNODE / RAND / UUID / SHA 系** — 未実装の SPARQL 1.1 組み込み関数
8. **dict_sorted.bin のブロック圧縮** — Zstd ブロック圧縮 + スキップインデックスで IRI/リテラル辞書を 3-5GB に圧縮（現在 13GB）。ランダムアクセス対応が必要
9. **HyperLogLog** — subject_count / object_count の近似カウント（現在は2パス全スキャン）
10. **BOUND_VAR_FACTOR の自動調整** — 述語の distinctiveness から動的に推定倍率を計算
11. **ReadonlyDict のキャッシュ戦略改善** — ホットキャッシュを LRU 化して偏りのあるアクセスパターンに対応
12. **フィルタリング hash join の Alternative パス対応** — `PropertyPath::Sequence` は 2-hop 以上すべてで動作済み（`steps.len() >= 2`）。`Alternative` パスへの適用は未対応
13. **ecordf build 時の path/type cache 事前ビルド** — `ecordf build` で rdf-config を指定すれば初回起動をさらに高速化できる
