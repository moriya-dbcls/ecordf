## CompoundPath 型対応 (2026-06-03)
- 変更: path_cache.rs を Vec<(String,Cardinality)> 型に対応
- ビルド: 成功
- テスト: 成功 (27 passed; 0 failed)

### 変更内容
1. `use crate::rdf_config::CompoundPath;` → `use crate::rdf_config::{Cardinality, CompoundPath};`
2. `resolve_path` シグネチャを `&[String]` → `&[(String, Cardinality)]` に変更、イテレータを `|(iri, _)|` に更新
3. テスト内の `Vec<String>` データを `Vec<(String, Cardinality)>` に更新（ヘルパー関数 `e()` を追加）
