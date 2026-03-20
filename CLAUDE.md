# code-sherpa

Python >=3.13 のプロジェクト。パッケージマネージャは `uv`。

## 開発コマンド

```bash
uv sync           # 依存関係のインストール
uv run main.py    # 実行
```

## ドキュメント

- `docs/design.md` - アーキテクチャ・技術選択・ハーネス計画の設計ドキュメント
- `docs/decisions/` - ADR（Architecture Decision Records）。命名規則: `{NNN}-{kebab-case}.md`

## パイプライン実行

```bash
uv run python pipeline.py <issue-number>   # GitHub Issue 番号を指定
CODE_SHERPA_REPO=owner/repo uv run python pipeline.py 42  # リポジトリを明示指定
```

## Git 規約

- ブランチ命名: `{type}/{kebab-case}`（例: `chore/setup-dev-tooling`, `docs/add-structured-observation-adr`）
- GitHub Actions は commit hash でピン留め + バージョンコメント
