# タスク4: SPARQL パーサー・AST・プラン

## 担当ファイル

- `src/sparql/ast.rs` — SPARQL 1.1 AST 型定義
- `src/sparql/parser.rs` — 手書き再帰下降パーサー
- `src/sparql/plan.rs` — ExecutionPlan enum

## このタスクの責務

SPARQL 文字列を受け取り、AST を経由して executor が処理できる ExecutionPlan に変換する。
文法の追加・修正はこのタスクで完結する（executor への影響を最小化）。

## AST 主要型

### Expression（全一覧）

```rust
pub enum Expression {
    // 値
    Variable(String), Literal(Literal), Iri(String),

    // 算術
    Add, Sub, Mul, Div, Neg,

    // 比較
    Eq, Ne, Lt, Le, Gt, Ge,

    // 論理
    And, Or, Not,

    // 型検査
    Bound(String), IsIri, IsLiteral, IsBlank, IsNumeric,

    // 文字列（全て実装済み）
    Str, Lang, Datatype, LangMatches,
    UCase, LCase, Concat, Contains, StrStarts, StrEnds,
    Strlen, Substr, StrBefore, StrAfter, EncodeForUri, Replace,

    // 正規表現
    Regex, 

    // 数値
    Abs, Round, Ceil, Floor,

    // 日時
    Year, Month, Day, Hours, Minutes, Seconds, Now,

    // 制御
    If, Coalesce, SameTerm,
    Iri2,            // IRI() / URI() 関数

    // 集計
    Count, Sum, Min, Max, Avg, GroupConcat, Sample,
}
```

### GraphPattern

```rust
pub enum GraphPattern {
    Bgp(Vec<TriplePatternAst>),
    Optional(Box<GraphPattern>, Box<GraphPattern>),
    Union(Box<GraphPattern>, Box<GraphPattern>),
    Join(Box<GraphPattern>, Box<GraphPattern>),
    Filter(Box<GraphPattern>, Expression),
    Extend(Box<GraphPattern>, Expression, String),  // BIND
    Subquery(Box<SelectQuery>),
    Values(ValuesClause),
    Graph(Term, Box<GraphPattern>),
    PathPattern { s: Term, path: PropertyPath, o: Term },
    Empty,
}
```

### PropertyPath

```rust
pub enum PropertyPath {
    Iri(String),
    Sequence(Vec<PropertyPath>),    // /
    Alternative(Vec<PropertyPath>), // |
    ZeroOrMore(Box<PropertyPath>),  // *
    OneOrMore(Box<PropertyPath>),   // +
    ZeroOrOne(Box<PropertyPath>),   // ?
    Inverse(Box<PropertyPath>),     // ^
}
```

## ExecutionPlan

```rust
pub enum ExecutionPlan {
    Empty,
    Scan { pattern: TriplePattern, variables: Vec<(String, usize)> },
    ScanAst(TriplePatternAst),
    ScanBound { base: TriplePattern, free_vars: Vec<(String, usize)>, outer_vars: Vec<(String, usize)> },
    Join(Box<ExecutionPlan>, Box<ExecutionPlan>),
    LeapfrogJoin { patterns: Vec<TriplePattern> },
    Optional(Box<ExecutionPlan>, Box<ExecutionPlan>),
    Union(Box<ExecutionPlan>, Box<ExecutionPlan>),
    Filter(Box<ExecutionPlan>, Expression),
    Extend(Box<ExecutionPlan>, Expression, String),
    Values(ValuesClause),
    PathPattern { s: Term, path: PropertyPath, o: Term },
    Subquery(Box<SelectQuery>),
    GroupBy { ... },
    OrderBy { ... },
    Limit { ... },
    Distinct(Box<ExecutionPlan>),
}
```

## パーサーの構造

手書き再帰下降パーサー（`lexer.rs` なし、文字列から直接トークナイズ）。

```rust
pub fn parse_query(input: &str) -> Result<Query, ParseError>
// 内部:
// parse_select_query → parse_where_clause → parse_group_graph_pattern
// parse_expression → parse_primary → parse_builtin_call
```

### 関数名の大文字小文字

パーサーはトークンを小文字化してからマッチする。SPARQL は大文字小文字を区別しない。

```rust
"strlen" => Expression::Strlen(...)
"strbefore" => Expression::StrBefore(...)
// 同様に全関数名を小文字でマッチ
```

### 新しい組み込み関数を追加する手順

1. `ast.rs` に `Expression::NewFunc(...)` variant を追加
2. `parser.rs` の `parse_builtin_call` に `"newfunc" => ...` を追加
3. `executor.rs` の `eval_term` または `eval_bool` に `Expression::NewFunc` の処理を追加

## 未対応の SPARQL 1.1 構文

- `CONSTRUCT` クエリ（AST 定義あり、executor で `Unsupported` を返す）
- `SERVICE` 句
- `SPARQL UPDATE`（INSERT/DELETE）
- `LOAD` / `CLEAR` / `CREATE` / `DROP`
- `DESCRIBE` クエリ
- `EXISTS` / `NOT EXISTS` はサポート

## 注意事項

- `TriplePatternAst` は文字列を持つ（パース直後の表現）。
  `TriplePattern` は TermId を持つ（executor が辞書引きした後の表現）。
- PropertyPath は BGP ではなく `PathPattern` として executor に渡される。Leapfrog の対象外。
- ブランクノード `_:b`、`[]`、`[pred obj]` はパース時に `Term::Variable("_:b")` へ変換される。
