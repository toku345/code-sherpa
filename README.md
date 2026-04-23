# code-sherpa

> Issue を山頂（マージ）まで導くシェルパ

## What it is

code-sherpa は、GitHub Issue の検知から PR 作成・マージ判断までを自律的に走りきるパイプラインマネージャーです。プラン作成・レビュー・実装・テスト・コードレビューといった各ステージで AI コーディングエージェント（Claude Code, Codex CLI 等）をサブプロセスとして呼び出し、ステージ間の判定・遷移・リトライは決定論的なプログラムとして制御します。詳細は [docs/design.md §1.1](./docs/design.md#11-何であるか) を参照してください。

## What it isn't

- **フリートオーケストレーターではない。** 複数 Issue の同時並列管理は初期スコープ外で、1 Issue のライフサイクルを確実に走りきることに集中します。
- **コーディングエージェントそのものではない。** コードを書くのは Claude Code や Codex CLI で、code-sherpa はそれらを各ステージで起動する存在です。
- **ノールックマージツールではない。** 初期スコープでは最終マージの判断は人間が行います。

詳細は [docs/design.md §1.2](./docs/design.md#12-何でないか) を参照してください。

## Requirements

- Python >= 3.13
- [uv](https://docs.astral.sh/uv/) (package manager)

## Getting started

```bash
uv sync                       # 依存関係のインストール
```

```bash
uv run ruff format --check .  # フォーマットチェック
uv run ruff check .           # リンター
uv run mypy --strict .        # 型チェック
uv run pytest                 # テスト
```

## Documentation

- [docs/design.md](./docs/design.md) — アーキテクチャ・技術選択・ハーネス計画の設計ドキュメント
- [docs/decisions/](./docs/decisions/) — ADR（Architecture Decision Records）
- [docs/prompts/](./docs/prompts/) — パイプラインのプロンプトテンプレート

## Status

本プロジェクトは現在アクティブに開発中です。ハッピーパスのパイプライン構築を進めています（参照: [issue #10](https://github.com/toku345/code-sherpa/issues/10)）。
