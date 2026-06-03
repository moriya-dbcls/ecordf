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
