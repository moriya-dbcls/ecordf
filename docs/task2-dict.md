# タスク2: 辞書ビルダー

## 担当ファイル

- `src/dict.rs` — レガシーインメモリ辞書（1パス・後方互換用）
- `src/dict_builder.rs` — 2パス外部ソート辞書ビルダー

## このタスクの責務

RDF 文字列（IRI・リテラル）と u64 ID の相互変換。ビルド時は外部ソートで RAM を節約し、
クエリ時は mmap でページキャッシュを活用する。

## 主要な構造体

### QueryDict（実行時辞書の統一インターフェース）

```rust
pub enum QueryDict {
    Mmap(ReadonlyDict),     // 2パスビルド後のクエリ時（推奨）
    Legacy(LegacyDict),     // 旧 1パスビルドの後方互換
}
impl QueryDict {
    pub fn lookup(&self, s: &str) -> Option<TermId>
    pub fn decode(&self, id: TermId) -> String
    pub fn encode(&self, s: &str) -> TermId   // 新エントリを追加（クエリ時のリテラル生成用）
    pub fn display(&self, id: TermId) -> String  // 人間が読める形式
    pub fn len(&self) -> u64
}
```

### ReadonlyDict（mmap ベース、2パスビルド後に使用）

```rust
pub struct ReadonlyDict { ... }
impl ReadonlyDict {
    pub fn open(path: &Path) -> io::Result<Self>
    pub fn lookup(&self, s: &str) -> Option<TermId>  // O(log N) binary search
    pub fn decode_id(&self, id: TermId) -> Option<String>
    pub fn len(&self) -> u64
    pub fn write_legacy_dict(&self, path: &Path) -> io::Result<()>  // dict.bin 出力
}
```

`dict_sorted.bin` を mmap し、オフセットテーブルで O(log N) バイナリサーチ。
ホットキャッシュ（≤4M エントリ）で頻用タームの再探索を高速化。

### DictBuilder（Phase 1: 文字列収集）

```rust
pub struct DictBuilder { ... }
impl DictBuilder {
    pub fn new(chunk_dir: &Path, max_bytes: usize) -> Self
    pub fn insert(&mut self, s: &str) -> io::Result<()>
    pub fn finish(self) -> io::Result<Vec<PathBuf>>  // ソート済みチャンクファイルのパス一覧
}
pub fn merge_string_chunks(chunks: &[PathBuf], output: &Path) -> io::Result<u64>
```

チャンクが 64 を超える場合は階層マージ（MAX_FAN_IN=64）。各レベルのバッチを Rayon で並列処理。

### DictScanner（Phase 2B: Streaming Join 用）

```rust
pub struct DictScanner { ... }
impl DictScanner {
    pub fn new(path: &Path) -> io::Result<Self>
    pub fn next(&mut self) -> Option<(&str, TermId)>  // 辞書を順次スキャン
}
```

8 MiB バッファで sequential I/O。`dict_sorted.bin` を先頭から1回スキャンして LocalMap を構築。

## dict_sorted.bin フォーマット (ESRT0001)

```
magic(8) = "ESRT0001"
count(8)          — ユニーク項数
offsets_start(8)  — オフセットセクションの開始バイト位置
── 文字列セクション (byte 24 から) ──
for each term (辞書順):
  len(u32) + bytes(len × u8)
── オフセットセクション ──
for each term i: u64 (文字列セクション内の絶対バイト位置)
```

ID は辞書順インデックス（0-based）で割り当てられる。

## 2パスビルドの流れ

```
Phase 1: 全ファイルをストリームして文字列を収集
  → DictBuilder がチャンクをソート・dedup してディスクへ
  → merge_string_chunks で dict_sorted.bin を生成

Phase 2A (term_count ≤ 1B):
  → ReadonlyDict を mmap し binary search でID解決
  → load_triples_parallel で並列ロード

Phase 2B (term_count > 1B, UniProt 規模):
  → ファイルをバッチ処理: collect_strings → Join（dict sequential scan）→ load_triples
  → ランダム I/O ゼロ（dict を1回シーケンシャルスキャン）
```

## クエリ時の辞書の特性

`QueryDict::encode()` は `&self` で呼べる（interior mutability: `RwLock`）。
`BIND(CONCAT(...) AS ?x)` 等で生成されたリテラルを実行時に辞書へ追加できる。
追加エントリはメモリ上のみで `dict_sorted.bin` には保存されない。

## レガシー dict.bin フォーマット

後方互換のみ。term_count > u32::MAX の場合は書き出しをスキップ（非致命的）。
クエリ時は `dict_sorted.bin` が存在すれば必ずそちらを使う。

## 注意事項

- `term_count > 1B` の閾値は `STREAMING_PHASE2_THRESHOLD` 定数（store.rs）で管理。
- Phase 1 の並列スレッド数は `config.build.parallel_threads`（0=全コア）。
- チャンクサイズ（RAM 使用量）は `config.build.dict_chunk_mb` × スレッド数。
