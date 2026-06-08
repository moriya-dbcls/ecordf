# タスク6: キャッシュ群・統計

## 担当ファイル

- `src/stats.rs` — 述語統計（StoreStatistics）
- `src/predcache.rs` — 述語キャッシュ（PredCache）
- `src/path_cache.rs` — 多ホップパスキャッシュ（PathCache）
- `src/type_cache.rs` — 型キャッシュ（TypeCache、RoaringTreemap）

## このタスクの責務

起動時にデータをメモリ（またはディスク）にキャッシュして、クエリ時の POS フルスキャンを回避する。
全て「起動時にビルドしてクエリ時に参照する」という同じパターンを持つ。

## StoreStatistics（述語統計）

### 目的

カーディナリティ推定によるジョイン順序の最適化（オプティマイザ Tier 2）。

### 構造

```rust
pub struct PredicateStats {
    pub triple_count:  u64,  // 総トリプル数
    pub subject_count: u64,  // 異なり主語数
    pub object_count:  u64,  // 異なり目的語数
}
pub struct StoreStatistics {
    pub total_triples: u64,
    pub predicate_stats: HashMap<TermId, PredicateStats>,
}
impl StoreStatistics {
    pub fn estimate(&self, s, p, o: Option<TermId>) -> u64  // カーディナリティ推定
    pub fn is_functional(&self, pred: TermId) -> bool       // triple_count ≈ subject_count
    pub fn functional_predicate_ids(&self) -> Vec<TermId>
}
```

### 機能的述語の判定基準

`triple_count ≤ subject_count × 1.05` → 機能的述語（各主語が目的語を最大1つ持つ）。
例: `dct:identifier`、`up:mnemonic`。executor の bind_join でO(log N)直接解決に使う。

### ファイルフォーマット（stats.bin / ECOSTAT2）

```
magic(8)="ECOSTAT2" + total_triples(8) + n_predicates(8) +
[(pred_id:u64, triple:u64, subject:u64, object:u64) × n_predicates]
```

---

## PredCache（述語キャッシュ）

### 目的

頻用述語の全 (S, O) ペアを RAM に保持して POS フルスキャンを回避。

### 構造

```rust
pub type PredPairs = Arc<Vec<(TermId, TermId)>>;  // (S, O) ソート済み
pub struct PredCache { ... }
impl PredCache {
    pub fn build_sync(&self, index, budget_bytes, per_pred_cap_bytes, priority_ids: &[u64])
    pub fn get(&self, pred: TermId) -> Option<PredPairs>  // O(1) ハッシュルックアップ
    pub fn bytes_used(&self) -> usize
}
```

### ロード戦略

1. Priority 述語（rdf-config から抽出）を per_pred_cap_bytes 以内なら優先ロード
2. 残り予算で大きい順にロード（ただし per_pred_cap も適用）
3. 予算 0 以下になったら停止

### よくある設定失敗例

```toml
# 例: SIO_000216 (1290MB) と SIO_000300 (1290MB) を両方キャッシュするには:
pred_cache_mb = 6144              # 4096 では SIO_000300 が入らない
pred_cache_per_pred_cap_mb = 1500 # 1024 では SIO_000216/300 がキャップで除外
```

---

## PathCache（多ホップパスキャッシュ）

### 目的

rdf-config model.yaml の compound path（例: `[faldo:begin, faldo:position]`）を
起動時に `Vec<(TermId, TermId)>` として実体化し、クエリ時の多段 POS スキャンを回避。

### ビルドの最適化

```rust
pub fn build(compound_paths, dict, index, pred_cache, budget_bytes) -> Self
```

1. 全サブシーケンス（長さ2以上）に展開し shortest-first でソート
2. 各パスの前に `index.pos_predicate_range(first_pred)` で事前推定
3. 推定サイズ > remaining_budget なら実体化をスキップ（POS フルスキャン回避）
4. budget = 0 になったら即 break（shortest-first なので残りは全部大きい）

### pred_cache との連携

`materialise_path` は `pred_cache` を引数に取り、キャッシュ済み述語は
POS スキャン + sort なしでペアを取得できる（faldo の場合 44秒 → 数秒）。

---

## TypeCache（型キャッシュ）

### 目的

`?x a SomeClass` を O(1) ビットマップルックアップで処理（従来: POS フルスキャン）。

### 構造

```rust
pub struct TypeCache {
    rdf_type_id: Option<TermId>,
    classes: HashMap<TermId, RoaringTreemap>,  // class → subjects のビットマップ
    bytes_used: usize,
}
impl TypeCache {
    pub fn get_bitmap(&self, class_id: TermId) -> Option<&RoaringTreemap>
    pub fn contains(&self, class_id: TermId, subject_id: TermId) -> Option<bool>
    pub fn intersect_classes(&self, a: TermId, b: TermId) -> Option<Vec<TermId>>
}
```

`RoaringTreemap` は u64 対応（RoaringBitmap は u32 のみ）。
メモリは `bm.serialized_size()` で計測（28MB の Vec → 2-4MB のビットマップ）。

---

## 注意事項

- `PredCache::build_sync` は同期実行（`build_background` は廃止推奨）。
- PathCache のビルドは pred_cache が完成した後に実行すること（`store.rs` で順序が決まっている）。
- `get_class()` は `Option<Vec<TermId>>` を返す（`Vec` のコピーあり）。bitmap 検査には `get_bitmap()` を使う方が効率的。
