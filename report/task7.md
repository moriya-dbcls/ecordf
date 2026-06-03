## ヒント提案機能 (2026-06-03)
- 変更: server.rs に detect_bnode_hints + collect_role_vars 追加
- 変更: execute_query / execute_query_with_cancel / build_query_response / format_decoded / decoded_to_json にヒント伝播
- 変更: main.rs query サブコマンドで eprintln! によるヒント表示
- ビルド: 成功
- テスト: 成功 (27 passed)

### 実装詳細

**collect_role_vars**: GraphPattern ツリーを再帰的にたどり、変数が主語に現れたか目的語に現れたかを収集する。Subquery/Values/Empty は境界として停止。

**detect_bnode_hints**: SELECT クエリのみ対象。SELECT * は全変数が投影済みのためスキップ。
- projected: SELECT 項目・GROUP BY・ORDER BY から収集
- 主語かつ目的語に現れ、projected に含まれない変数を候補として返す
- 候補が空なら空 Vec を返す（オーバーヘッドなし）

**HTTP レスポンスへの伝播**:
- JSON 形式: `head.hints` 配列に追加（SPARQL Results JSON 拡張）
- XML/TSV/CSV 形式: `X-EcoRDF-Hints` レスポンスヘッダに追加

**注意**: `pub(crate)` ではなく `pub` にした（main.rs は別クレートとして detect_bnode_hints にアクセスするため）
