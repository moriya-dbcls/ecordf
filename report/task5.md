# タスク5: COUNT(*) クロス積最適化の削除 + COUNT(DISTINCT ?x) 最適化

## 作業内容 (2026-06-03 更新)

### 変更ファイル
- `src/sparql/executor.rs`

### 変更の詳細

**1. `try_count_cross_product` メソッド削除**
- メソッド本体を削除。クロス積 `COUNT(*)` は巨大な積算値（〜3.2×10^20）を返すため意味がない。通常パスに fallback させることが正しい。

**2. `execute_select` 内の呼び出しブロック削除**
- `try_count_cross_product` の呼び出しとコメントブロック（8行）を削除。

**3. `try_count_distinct_cross_product` は既に実装済みだった**
- 作業開始時点で既にメソッドが存在し、`execute_select` からも呼ばれていた。
- 実装内容はタスク仕様と一致しているため変更なし。
- 動作: `SELECT (COUNT(DISTINCT ?pep) AS ?n) WHERE { ?pep a X . ?pe a Y . }` → X スキャンの distinct(?pep) 件数を高速返却。

## ビルド結果
```
cargo build --release  → Finished (警告のみ、エラーなし)
```

## テスト結果
```
cargo test --lib       → test result: ok. 27 passed; 0 failed
```

## 完了日時
2026-06-03

## BGP融合最適化 (2026-06-03)
- 変更: optimize_bgp_with_bound の Join アームに BGP+PathPattern 融合パス追加
- ビルド: 成功 (警告のみ、エラーなし)
- テスト: 成功 (27 passed; 0 failed)
- 備考: U-2/U-3 クエリで分断されていた BGP を PathPattern 越しに統合し、U-1 相当のプランを生成するよう修正。安全条件チェック（PathPattern 出力変数が BGP トリプルに現れないこと）付き。
