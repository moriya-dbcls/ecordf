## 50Bスケール対応 (2026-06-04)
- 変更: config.rs に p2a_buf_mb 追加 (デフォルト 64)
- 変更: loader.rs の load_triples_streaming に p2a_buf_mb 引数追加
- 変更: loader.rs に Phase 2 バッチ単位チェックポイント実装 (tmp_dir/p2_progress.json)
- 変更: store.rs の呼び出し側を更新 (config.build.p2a_buf_mb を渡す)
- ビルド: 成功
- テスト: 成功 (27 passed)

### 変更詳細
1. `BuildConfig` に `p2a_buf_mb: usize`（デフォルト64）を追加
2. `load_triples_streaming` のシグネチャに `p2a_buf_mb: usize` を追加、`const P2A_BUF_BYTES` を `let p2a_buf_bytes = p2a_buf_mb.max(16) * 1024 * 1024` に置換
3. バッチループ前に `tmp_dir/p2_progress.json` からチェックポイントを読み込み、完了済みバッチをスキップ
4. バッチ完了後に `completed_batches` をチェックポイントファイルへ書き込み
5. 全バッチ完了時にチェックポイントファイルを削除

---

## CompoundPath 型対応 (2026-06-03)
- 変更: store.rs build_path_cache_from_compounds の引数型を CompoundPath に統一
- ビルド: 成功
- テスト: 成功 (27 passed)

### 変更内容
1. `use crate::rdf_config;` → `use crate::rdf_config::{self, CompoundPath};`
2. `build_path_cache_from_compounds` の引数型 `&[Vec<String>]` → `&[CompoundPath]`

### 備考
rdf_config.rs は既に `CompoundPath = Vec<(String, Cardinality)>` に更新済みだった。
path_cache.rs も既に新型を import 済みで整合している。
