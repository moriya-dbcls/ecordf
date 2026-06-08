# タスク5: クエリエグゼキュータ

## 担当ファイル

- `src/sparql/executor.rs` (~4500行、最大ファイル)

## このタスクの責務

ExecutionPlan を受け取り、インデックス・各種キャッシュを使って ResultSet を返す。
JOIN 戦略の選択、オプティマイザ、式評価、集計を担う。

## Executor 構造体

```rust
pub struct Executor<'a> {
    pub index:           &'a TripleIndex,
    pub dict:            &'a QueryDict,
    pub config:          QueryConfig,
    pub stats:           Option<&'a StoreStatistics>,
    pub pred_cache:      PredCache,
    pub path_cache:      PathCache,
    pub type_cache:      TypeCache,
    pub cancel:          Arc<AtomicBool>,
    pub scan_dontneed_bytes: usize,
    pushdown_limit:      Cell<Option<usize>>,
}
```

## JOIN 戦略の選択フロー

### bind_join（インデックスネステッドループ結合）

左辺の各バインディングを右辺プランに代入してインデックスをプローブする。
左辺が小さい（≤ `bind_join_threshold`）か、右辺が左辺の変数に依存する場合に使用。

**bind_join 内の4段最適化（グループ数 > PRED_SCAN_THRESHOLD=32 のとき）:**

1. **述語スキャン**: N 回ランダムプローブ → 1回 POS シーケンシャルスキャンに置き換え
2. **TermId ソート**: グループを昇順ソートしてページキャッシュ局所性を向上（≤100K グループ）
3. **バッチプリフェッチ**: 最初の 1024 グループ分の対象ページに `madvise(WILLNEED)`
4. **PSO fast path**: LIMIT pushdown 時に PSO から先頭 N 件のみ取得

### hash_join（ハッシュ結合）

左右を全件スキャンし、小さい方でハッシュテーブルを構築。
左辺が大きく（> threshold）かつ右辺が左辺変数に依存しない場合。

**SIP（Sideways Information Passing）pre-filter:**
hash_join 前に `right_rs` を `left_rs` の共有変数値セットでフィルタリング。
`left_rs.rows < right_rs.rows / 10` かつ共有変数の値集合 ≤ 100,000 のとき適用。

### Leapfrog Triejoin

複数の単純トリプルパターンが1変数を共有する場合に適用。
中間結果をメモリに展開しない。共有変数が 2 つ以上のパターンは hash_join にフォールバック。

### PathPattern（プロパティパス）

property path は BGP ではないため Leapfrog の対象外。
パスキャッシュがヒットすれば RAM から直接返す。

## グループとは

bind_join において「右辺の実行結果が同一になる左辺行の集まり」。
右辺が参照する変数（`needed`）の値の組でグルーピングする。
1グループにつき右辺を1回実行し、結果を全行に配布する。

## 述語スキャン（Predicate Scan）詳細

`try_predicate_scan_join` が担当。右辺が `ScanBound(s=外部変数, p=固定, o=自由)` の形のとき、
N 回の SPO ランダムプローブ（N × 150ms）を1回の POS シーケンシャルスキャンに置き換える。

```
N=508, 150ms/probe → 76秒
POS sequential scan → ~0.5秒
```

## eval_term / eval_bool / eval_string

### eval_term（TermId を返す）

文字列関数・数値関数・日時関数は `eval_term` に実装されている。

```rust
Expression::Strlen(e) → xsd:integer
Expression::Substr(s, start, len) → plain literal
Expression::StrBefore(s, marker) → plain literal
Expression::StrAfter(s, marker) → plain literal
Expression::EncodeForUri(e) → percent-encoded literal
Expression::Iri2(e) → IRI TermId
Expression::Year/Month/Day/Hours/Minutes/Seconds(e) → xsd:integer/decimal
Expression::Now → xsd:dateTime（UTC）
```

### eval_bool（bool を返す）

FILTER 式の評価。`IsNumeric`, `IsIri`, `IsLiteral`, `IsBlank`, `SameTerm`, `LangMatches`,
`Contains`, `StrStarts`, `StrEnds`, `Regex` 等。

### eval_string（String を返す）

辞書から TermId をデコードしてリテラルの字句形式を取り出す。

## TypeCache 統合

`?x a SomeClass` が filter パターンのとき（O が固定）、`type_cache.get_bitmap(class_id)` で
RoaringTreemap O(1) ビットマップルックアップを使う。

## 機能的述語の判定

`StoreStatistics::is_functional(pred)` が true（`triple_count ≈ subject_count`）の述語は
各主語が高々1つの目的語しか持たないため、S → O のルックアップで Vec を確保せず
単一の目的語を返せる。

## eval_sequence_with_subject_filter（フィルタリング hash join）

2-hop Sequence パスで左辺の主語集合が既知のとき、step0 の出力を即座にフィルタリングして
step1 の HashMap を大幅に削減する。

```
左辺 508 件のとき: 11.8M → 508 ペアに削減 → HashMap 508 エントリ → ~5秒（無効時18秒）
```

## SELECT 実行後処理

```
GROUP BY → apply_group_by（COUNT/SUM/MIN/MAX/AVG/GROUP_CONCAT/SAMPLE）
ORDER BY → sort with type-aware comparison
DISTINCT → dedup
LIMIT/OFFSET → truncate
```

## 注意事項

- `max_intermediate_rows`（デフォルト 50M）を超えると `overflow=true` でカット。
- `pushdown_limit` は Cell で管理（`&self` から更新できるように）。
- キャンセルフラグは各 bind_join の内側ループで確認する。
- `scan_dontneed_bytes` を超える POS/SPO スキャンの後に `madvise(DONTNEED)` でページキャッシュを解放（他サービスとの共存用）。
