# ADR-007: 実装言語を Python から Rust に切り替える

- **ステータス:** Accepted
- **日付:** 2026-06-24
- **関連:** design.md セクション 3.1（言語選定・**本 ADR が supersede**）・3.2（実装スタイル）・2.1（決定論的 Manager）・2.4（shell-kick）, ADR-006（Landscape 再検証）

## コンテキスト

design.md §3.1 は 2026-03 時点で Python を採用し Rust を不採用とした。ADR-006 で確認した前提変更（目的変更・課金モデル・ネイティブ primitive・shell-kick 採用の確定）により、この言語判断を再評価する。

再評価のトリガーとなった新しい動機:

1. **配布の摩擦:** Python はインタプリタ / venv / `uv` を前提とし、自分が日常的に使うツールとして配布・起動が煩雑。`cargo install --path .` でローカルインストールし、対象リポジトリ内で `sherpa <issue_number>` を実行できる形を目指す。プロジェクト名は `code-sherpa` のまま維持し、実行コマンド名だけを `sherpa` とする。
2. **Rust を使いたい:** ADR-006 で目的が「実用ツール」に変わり、キャリア signal ではなく個人的な学習意欲として Rust を採りたい。

加えて、**今が切り替えの最良タイミング**である。現存コードはプリミティブ層（`run_cmd` / `run_agent` / `load_prompt` ＋ `Stage` enum ＋ `PipelineContext`, 約 111 行）とテストのみで、ステートマシン本体は未着手。沈んだコストが最小であり、8 ステージを実装した後の切替は高コストになる。

## 再評価: design.md §3.1 の論拠はどう変わったか

### Python を選んだ理由の再検証

| §3.1 の Python 採用理由 | 2026-06 の再評価 | 効力 |
|---|---|---|
| プロトタイプ速度（`subprocess` ＋ `json.loads`） | Rust でも `std::process::Command` ＋ `serde_json` で実現。プロンプトは外部ファイル化済みで、頻繁に変わる部分は再コンパイル不要 | 弱まる |
| ポートフォリオ適性（AI/ML キャリア転換の証明） | ADR-006 で目的が実用ツールに変更。**論拠消滅** | 無効 |
| SDK 移行の自然さ（Agent SDK が Python） | code-sherpa は SDK ではなく shell-kick を採用済み（§2.4）。課金変更で SDK の定額メリットも消滅（ADR-006） | 大幅減衰 |
| 試行錯誤との相性（即時実行） | プロンプトは `docs/prompts/*.md` に外部化済みでホットリロード可能。compile コストは安定部（遷移ロジック）にのみ乗る | 部分的に緩和 |

### Rust を退けた理由の再検証

| §3.1 の Rust 不採用理由 | 2026-06 の再評価 |
|---|---|
| glue のボイラープレート（`String`/`&str`, `Result`） | 残る。ただし小規模（後述の行数目標）で許容範囲 |
| compile サイクルが遅い | プロンプト外部化で頻繁な反復は再コンパイル不要。影響は限定的 |
| （§3.1 が認めた利点）型安全なステートマシン、enum + match の網羅性チェック | **`Stage` 遷移制御の本丸であり、決定論的 Manager（§2.1）の設計思想と強く合致する利点** |

## 決定

**実装言語を Rust に切り替える。** design.md §3.1 の言語決定を supersede する。

### 設計方針

- **アーキテクチャは不変。** shell-kick / テキストリレー（§2.4）・決定論的 Manager（§2.1）・ステージ分離（§2.2）はそのまま。言語非依存なので移植のみ。
- **依存は最小限**（design.md §3.2 の「薄いスクリプト」精神を Rust に引き継ぐ）:

  | クレート | 用途 | 備考 |
  |---|---|---|
  | `serde` / `serde_json` | `claude -p --output-format json` の解析、構造化観察ログ（ADR-005） | 必須 |
  | `clap` | CLI 引数（`sherpa <issue_number>`）。実行対象は現在ディレクトリの Git リポジトリで、`git rev-parse --show-toplevel` により repo root へ正規化し、`origin` remote から `owner/repo` を推定する | `std::env::args` で代替も可。idiomatic な `clap` を採用 |
  | `anyhow` | エラー伝播（fail loud, CLAUDE.md） | 型付きが要れば `thiserror` を併用 |
  | `wait-timeout` | subprocess のタイムアウト（`std::process` に timeout 付き wait が無いため） | Manager のデッドライン制御（ADR-006）に必須 |
  | `tempfile`（dev） | `load_prompt` テストの一時ディレクトリ | dev-dependency。バイナリには含まれない |

  **`tokio` は初期は入れない。** パイプラインは逐次実行で、ブロッキングな subprocess 呼び出しで足りる（pipe のドレインはスレッドで処理）。複数 Issue 並列（§7）に踏み込む段階で再評価。
- **プリミティブの移植:** `run_cmd` / `run_agent` / `load_prompt` / `Stage` / `PipelineContext` を Rust に移植。エラーは `Result` で伝播し、失敗時はログを出して即停止（fail loud）。
- **行数目標:** design.md §3.2 の「200〜300 行」を Rust では 400〜600 行程度に読み替える。
- **テスト:** `cargo test`。既存 Python テストの方針（`echo`/`ls` で実コマンド検証 ＋ 失敗系の検証）を踏襲。JSON 解析は `claude` 起動を避けるため `parse_agent_output` を純関数に切り出して直接テストし、`run_cmd` は実コマンド（`echo`/`sleep`/`ls`）で success/timeout/failure を検証する。
- **プロンプトは `docs/prompts/*.md` のまま**（言語非依存、変更不要）。

## 根拠

- 配布: 単一バイナリでインタプリタ / venv 不要。新しい動機 1 に直接効く。
- アーキテクチャ非依存: 核が shell-kick のため Python 固有機能を使っておらず、移植リスクが低い。
- タイミング: 沈んだコスト最小の今が切替適期。
- 適合: enum + match の網羅性チェックが決定論的ステートマシンに合致（§3.1 自身が認めた利点）。
- 目的整合: ADR-006 の「実用ツール ＋ 個人的 Rust 学習」と一致。

## 受け入れるトレードオフ

- **in-process Agent SDK の在来エルゴノミクスを一部手放す（ただし「扉が閉じる」は言い過ぎ）。** 公式 Agent SDK は Python/TS のみで Rust 版が無い。だが ADR-004 が hardening に挙げた**フェーズごとの権限分離は env（token）差し替えで shell-kick でも実現可能**であり（ADR-004 自身が「環境変数の差し替えで実現可能。SDK ならより自然」と明記）、能力そのものは失わない。env 差し替えで代替できない in-process 固有機能（hooks / sessions / structured output / 承認・user-input ハンドリング / 宣言的ツールゲーティング）は手放すが、その大半は code-sherpa の text-relay 設計が元々使わない（one-shot ＝ session 不要、`--output-format json` で structured 取得済み、承認はゲートで人間にエスカレーション、ツール制限は `.claude/settings.json` ＝ ADR-004 レイヤー1 が担当）。
  - **SDK / sidecar へ戻す再評価トリガー（狭く具体的）:** env 差し替え ＋ `.claude/settings.json` では表現しきれない、in-process の細粒度ツールゲーティング、またはステートフルな多ターン制御が必要になったとき。実際に使いたい3経路（`claude -p` / `codex exec` / Codex App Server JSON-RPC）はいずれも Rust から到達可能。
- **glue のボイラープレート ・ コア反復の compile コスト。** 小規模ゆえ許容。
- **Python の AI/ML signal を失う。** ADR-006 の目的変更により論拠が消えているため moot。

## 将来の検討候補

- **in-process SDK が必要になった場合:** agent 接続層のみ Python/TS サイドカーに切り出す、または Anthropic HTTP API を Rust から直接叩く。あるいは断念（利用者の明示スタンス）。
- **複数 Issue 並列（§7）:** その段階で `tokio` 等の async ランタイム導入を再評価。

## 参考資料

- [The Rust Programming Language](https://doc.rust-lang.org/book/)
- [std::process::Command](https://doc.rust-lang.org/std/process/struct.Command.html)
- [serde](https://serde.rs/) / [serde_json](https://docs.rs/serde_json/)
- [clap](https://docs.rs/clap/)
- ADR-006（Landscape 再検証・目的変更・課金・build/adopt）
