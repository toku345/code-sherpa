# code-sherpa

Python >=3.13 のプロジェクト。パッケージマネージャは `uv`。

## 開発コマンド

```bash
uv sync           # 依存関係のインストール
uv run main.py    # 実行

# sandbox 環境でキャッシュエラーが出る場合
UV_CACHE_DIR="$TMPDIR/uv-cache" uv run pytest
```

## ドキュメント

- `docs/design.md` - アーキテクチャ・技術選択・ハーネス計画の設計ドキュメント
- `docs/decisions/` - ADR（Architecture Decision Records）。命名規則: `{NNN}-{kebab-case}.md`

## 検証コマンド

```bash
uv run ruff format --check .  # フォーマット確認
uv run ruff check .           # リンター
uv run mypy --strict .        # 型チェック
uv run pytest                 # テスト
```

## パイプライン実行

```bash
uv run python pipeline.py <issue-number>   # GitHub Issue 番号を指定
CODE_SHERPA_REPO=owner/repo uv run python pipeline.py 42  # リポジトリを明示指定
```

## 既知の sandbox 制限

- `gh` CLI: macOS Seatbelt sandbox が `com.apple.trustd.agent` をブロックし TLS 証明書検証が失敗する（[claude-code#28954](https://github.com/anthropics/claude-code/issues/28954)）。`excludedCommands` だけでは Mach service 制限は解除されないため、`allowUnsandboxedCommands: true` で `dangerouslyDisableSandbox` escape hatch を有効化している
- `codex` CLI: macOS システム設定へのアクセスがブロックされ起動不可
- `git push/pull`: `~/.ssh/known_hosts` が読み取り拒否対象のため SSH 経由不可
- 上記操作はユーザーに手動実行を依頼する

## Git 規約

- ブランチ命名: `{type}/{kebab-case}`（例: `chore/setup-dev-tooling`, `docs/add-structured-observation-adr`）
- GitHub Actions は commit hash でピン留め + バージョンコメント
