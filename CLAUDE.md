# EcoRDF — プロジェクト総合 CLAUDE.md (タスク0: アーキテクチャ監督)

## プロジェクト概要

EcoRDF は Rust 製の RDF トリプルストアです。以下を設計目標としています。

- **メモリ効率**: memmap2 による OS 管理ページング。インデックスは仮想アドレスにマップするだけで、実 RAM はアクセスしたページのみ。
- **ビルド時メモリ効率**: 2パス外部ソートで辞書・インデックスを構築。ピーク RAM をデータセットサイズに依存しない定数に抑える。
- **クエリ性能**: Leapfrog Triejoin + コストベースオプティマイザ + 複数段キャッシュ。

## ビルドと基本コマンド

```bash
cd ecordf
cargo build --release            # gzip 対応込み（デフォルト）
cargo test                       # 単体テスト
cargo build --release --no-default-features  # gzip なし

# データロード（2パス外部ソート）
./target/release/ecordf build --dir ./store --from-file inputs.txt

# Delta 圧縮（ビルド後に実行、HDD/SSD 環境で推奨）
./target/release/ecordf compress-cols --dir ./store

# SPARQL クエリ
./target/release/ecordf query --dir ./store "SELECT ..."

# HTTP サーバー起動
./target/release/ecordf serve --dir ./store --port 17878
```
- ただし、サーバーの起動、再起動はユーザが行うものとするため、再起動が必要な場合は連絡する。
- また、起動は完了まで時間がかかることがあるため、debug.logに"Server ready"があるかを１分毎に確認して作業を進める。

## モジュール構成と担当タスク

```
src/
├── triple.rs          → タスク1 (型定義)
├── col_delta.rs       → タスク1 (Delta 圧縮)
├── index.rs           → タスク1 (インデックス本体)
├── dict.rs            → タスク2 (レガシー辞書)
├── dict_builder.rs    → タスク2 (2パス辞書ビルダー)
├── loader.rs          → タスク3 (N-Triples/N-Quads パーサー)
├── store.rs           → タスク3 (ストアファサード)
├── config.rs          → タスク3 (設定)
├── sparql/
│   ├── ast.rs         → タスク4 (AST 型定義)
│   ├── parser.rs      → タスク4 (手書き再帰下降パーサー)
│   ├── plan.rs        → タスク4 (ExecutionPlan 型)
│   └── executor.rs    → タスク5 (クエリ実行エンジン)
├── stats.rs           → タスク6 (述語統計)
├── predcache.rs       → タスク6 (述語キャッシュ)
├── path_cache.rs      → タスク6 (多ホップパスキャッシュ)
├── type_cache.rs      → タスク6 (型キャッシュ、RoaringTreemap)
├── pred_partition.rs  → タスク6 (オンディスク述語パーティション)
├── rdf_config.rs      → タスク7 (rdf-config 統合)
├── server.rs          → タスク7 (HTTP サーバー)
└── main.rs            → タスク7 (CLI)
```

各タスクの詳細は `docs/task{1-7}-*.md` を参照。

## タスクの割り振りと並列処理ルール

Claude Code（親タスク）は作業要件をタスク1〜7に切り分け、各タスクごとに `screen` セッションを立てて別途 claude を起動することで並列処理する。子タスクは `report/task[1-7].md` へ報告し、親タスクが取りまとめる。

### 並列化の粒度
- **タスク番号（1〜7）単位**で並列化する。`task5a/5b/5c` のような細分割はしない。
- 同一タスク番号内の複数改善は **1つの screen インスタンス**にまとめて渡す。
- タスク番号が異なる場合のみ並列化する（担当ファイルが重複しないため競合しない）。

### screen 起動方法
`.claude/settings.json` の allowlist により、子タスクは `--dangerously-skip-permissions` 不要で主要操作を自動承認する。
```bash
# メインリポジトリを直接編集（コピー不要）
PROJECT_ROOT=$(git rev-parse --show-toplevel)
screen -dmS task5 bash -c "cd $PROJECT_ROOT && claude < prompts/task5.txt"
screen -dmS task6 bash -c "cd $PROJECT_ROOT && claude < prompts/task6.txt"
```

### 承認が必要な操作（子タスク→親タスクへの通知）
allowlist に含まれない操作（`git push`、システムパッケージインストール等）が必要な場合、
子タスクは作業を中断し `report/need_approval.md` へ以下の形式で書き込む：
```markdown
## [タスク番号] 承認要請
- 操作: `git push origin main`
- 理由: リモートへの反映が必要
- 作業状況: executor.rs の修正完了、コミット済み
```
親タスクが `report/need_approval.md` を確認し、ユーザーへ報告して対応する。

### プロンプトファイルに含める情報
- 作業ディレクトリ（プロジェクトルート）
- 担当ファイル（CLAUDE.md のモジュール構成を参照）
- 問題の詳細・修正方針
- 報告先: `report/task[N].md`（プロジェクトルートからの相対パス）

### ポート確認
サーバー（port=17878）の起動・再起動はユーザーが行う。空いていなければ報告する。
また、親タスクはクエリの実行、ログの解析などを行い判断と報告を行う。

## 主要な型

```rust
pub type TermId = u64;              // 辞書 ID（u32 上限を超えるデータセット対応）
pub const UNBOUND: TermId = u64::MAX;  // 未バインド変数

pub struct Triple { pub s: TermId, pub p: TermId, pub o: TermId }
pub struct TriplePattern { pub s: TermId, pub p: TermId, pub o: TermId }
// UNBOUND の位置が変数（ワイルドカード）

pub enum IndexKind { Spo, Pos, Osp, Pso, Sop, Ops }
```

## インデックスフォーマット

| ファイル | 内容 | フォーマット |
|---------|------|------------|
| `spo.c0/c1/c2` | SPO 列指向インデックス (生) | ECOCOL01 |
| `spo.c0.dz` 等 | Delta 圧縮版 | ECOCOL02/03 |
| `spo.skip` | 2段スキップ索引 | ECOSKIP2 |
| `pos.pidx` | 述語2次索引 | ECOPIDX1 |
| `dict_sorted.bin` | 辞書（クエリ時） | ESRT0001 |
| `stats.bin` | 述語統計 | ECOSTAT2 |
| `gspo.bin` | Named Graph 索引 | ECOG0002 |
| `pred_parts/pp_*.bin` | 述語パーティション | ECPP0001 |

Delta 圧縮が存在する場合（`*.dz`）は自動的に優先される。

## モジュール間インターフェース（変更時は全タスクに影響）

以下を変更する場合は必ずタスク0 で影響範囲を確認する。

- `TermId` 型（現在 u64）
- `Triple` / `TriplePattern` / `Quad` 構造体
- `IndexKind` enum
- `IndexFile::scan()` シグネチャ
- `Store::query()` / `Store::open()` パブリック API
- `QueryDict` enum（ReadonlyDict / Dictionary）
- `ResultSet` 構造体（executor の出力）

## 設定ファイル（ecordf.toml）

```toml
[build]
chunk_size = 5_000_000        # 外部ソートのトリプルチャンクサイズ
dict_chunk_mb = 200           # Phase1 文字列バッファ（MB）
parallel_threads = 0          # 0 = 全 CPU コア
auto_compress_cols = false    # ビルド後に compress-cols を自動実行

[server]
pred_cache_mb = 6144          # 述語キャッシュの RAM 予算
pred_cache_per_pred_cap_mb = 1500  # 述語ごとの上限
type_cache_mb = 256           # 型キャッシュの RAM 予算
warmup_mb = 48000             # 起動時ウォームアップ MB
query_timeout_secs = 0        # 0 = タイムアウトなし

[model]
path_cache_mb = 700           # パスキャッシュの RAM 予算
rdf_configs = [...]           # rdf-config の URL またはパス
```

## 既知の技術負債

- Leapfrog Triejoin は共有変数が 2 つ以上のパターンで hash_join にフォールバック（完全実装未）
- CONSTRUCT クエリ未実装
- SPARQL UPDATE（INSERT/DELETE）未実装
- SERVICE（フェデレーション）未実装
- GROUP BY の大規模中間結果（5M行以上）ではストリーミング集計が未実装
- 1クエリの内部並列化未実装（クエリ間並列は実装済み）

## クロスモジュール変更の例

以下は複数タスクをまたぐため、タスク0 で全体調整が必要：

- インデックスフォーマット変更 → タスク1 + タスク3（ロード）+ タスク7（CLI）
- 新しいキャッシュ戦略 → タスク6（キャッシュ）+ タスク5（executor 呼び出し）+ タスク3（Store::open）
- 新 SPARQL 構文 → タスク4（パーサー）+ タスク5（executor）
- TermId を変更 → 全タスク
