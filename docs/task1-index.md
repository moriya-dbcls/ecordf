# タスク1: 索引・圧縮コア

## 担当ファイル

- `src/triple.rs` — TermId / Triple / TriplePattern / IndexKind 型定義
- `src/col_delta.rs` — Delta 圧縮 (ECOCOL02/03)
- `src/index.rs` — 列指向インデックス、スキップ索引、述語2次索引、TripleScan

## このタスクの責務

ディスク上のソート済み整数配列として RDF トリプルを格納し、効率的に読み書きする層。
他の全モジュール（辞書・ローダー・executor）から利用される最下層。

## 主要な構造体・関数

### triple.rs

```rust
pub type TermId = u64;
pub const UNBOUND: TermId = u64::MAX;
pub struct Triple { pub s: TermId, pub p: TermId, pub o: TermId }
pub struct TriplePattern { pub s: TermId, pub p: TermId, pub o: TermId }
pub enum IndexKind { Spo, Pos, Osp, Pso, Sop, Ops }
impl TriplePattern {
    pub fn best_index(&self) -> IndexKind  // バインド状況から最適インデックスを選択
}
```

### col_delta.rs

Delta 圧縮フォーマット（ECOCOL02/03）。256エントリをブロックとし、最小幅デルタ符号。

```rust
pub fn encode_column(values: &[u64], path: &Path) -> io::Result<()>
// ECOCOL03: 述語境界でブロックを強制分割（POS c1 の圧縮率向上）
pub fn encode_column_pred_aligned(values: &[u64], boundaries: &[usize], path: &Path) -> io::Result<()>

pub struct DeltaColFile { pub count: usize, ... }
impl DeltaColFile {
    pub fn open(path: &Path) -> io::Result<Self>  // ECOCOL02/03 両対応
    pub fn get(&self, pos: usize) -> u64           // O(1) ランダムアクセス（ブロック展開）
    pub fn iter_from(&self, start_pos: usize) -> DeltaColIter  // 効率的な順次スキャン
    pub fn lower_bound(&self, target: u64) -> usize
}
```

**ECOCOL02 vs ECOCOL03**:
- ECOCOL02: 全ブロック = 256 エントリ（最後を除く）。`start_positions[i] = i * 256`。
- ECOCOL03: 述語境界でブロック分割。ブロックインデックスに `start_pos` を格納（24B/エントリ）。

### index.rs

#### SkipIndex（2段スパース索引）

```
L2: stride=262,144  → 91 KB（CPU L2 キャッシュ常駐）
L1: stride=512      → 47 MB（大規模データ）
効果: 3Bトリプルで log₂(3B)=31ページフォルト → メモリ内比較 + 1ページフォルト
```

- `SkipIndex::narrow(key) -> (lo, hi)` — lower_bound 用：key が存在する最小窓
- `SkipIndex::upper_hint(key) -> usize` — 2番目のキー探索用：k0 の上限位置

#### PredicateIndex（述語2次索引）

```rust
// predicate_id → (lo, hi) in POS/PSO column arrays
struct PredicateIndex { ranges: HashMap<TermId, (usize, usize)> }
```

POS/PSO でのみ保持。`range_for_pattern(p=P, o=*)` を O(1) HashMap ルックアップで解決。

#### IndexFile::scan()

```rust
pub fn scan(&self, pat: &TriplePattern) -> TripleScan
```

**重要**: DeltaColumnar ストレージでは `DeltaScanState`（3本の `DeltaColIter`）を使う。
`get_raw(pos)` を使うと1エントリごとに256エントリ分ブロック展開が走り極端に遅くなる。

#### TripleIndex（全インデックスのコレクション）

```rust
pub struct TripleIndex {
    pub spo: IndexFile,
    pub pos: IndexFile,
    pub osp: IndexFile,
    pub pso: Option<IndexFile>,  // 6インデックスストアのみ
    pub sop: Option<IndexFile>,
    pub ops: Option<IndexFile>,
    pub gspo: Option<GspoIndexFile>,
}
impl TripleIndex {
    pub fn scan(&self, pat: &TriplePattern) -> TripleScan
    pub fn pos_predicate_range(&self, pred: TermId) -> Option<(usize, usize)>  // O(1)
    pub fn compress_columns(store_dir: &Path, force: bool) -> io::Result<usize>
}
```

## ファイルフォーマット

### 列ファイル（ECOCOL01 生、ECOCOL02/03 圧縮）

```
spo.c0, spo.c1, spo.c2 — (Subject, Predicate, Object) 列
pos.c0, pos.c1, pos.c2 — (Predicate, Object, Subject) 列
osp.c0, osp.c1, osp.c2 — (Object, Subject, Predicate) 列
（.dz 拡張子で Delta 圧縮版）
```

### スキップ索引（ECOSKIP2）

```
magic(8)="ECOSKIP2" + stride_l1(4) + stride_l2(4) +
l1_count(8) + l2_count(8) + total(8) +
l1_data[u64 × l1_count] + l2_data[u64 × l2_count]
```

旧フォーマット ECOSKIP1 は読み込み時に自動変換（L2 は L1 から導出）。

### 述語2次索引（ECOPIDX1）

```
magic(8) + pred_count(8) + [(pred:u64, lo:u64, hi:u64) × pred_count]
```

### open_delta でのパス解決（重要）

`.dz` ファイルを開くとき、`.skip` と `.pidx` は**生列ファイルの隣**にある。

```rust
// 正しい:
let c0_base = paths[0].with_extension("");  // "pos.c0.dz" → "pos.c0"
let skip_p = skip_path_from_c0(&c0_base);   // → "pos.skip"

// 間違い（古いバグ）:
let skip_p = skip_path_from_c0(&paths[0]);  // → "pos.c0.skip" (存在しない)
```

## 既知のバグ・TODO

- [ ] Eytzinger レイアウトで L1 アンカーの binary search を 1.5〜2× 高速化できる
- [ ] Elias-Fano 符号化で L1 アンカーを 47MB → 1.5MB に削減できる
- [ ] GspoIndexFile は列指向フォーマット未対応（従来の interleaved のみ）

## 注意事項

- `DeltaColFile::get(pos)` は O(BLOCK_SIZE) のためループ内で呼ぶと極端に遅い。順次スキャンは必ず `iter_from()` を使うこと。
- インデックスフォーマット変更時は `CLAUDE.md`（タスク0）でタスク3（ロード）との整合を確認すること。
- `compress_columns` で `pos.c1`/`pso.c1` は `encode_column_pred_aligned` を使う（他は `encode_column`）。
