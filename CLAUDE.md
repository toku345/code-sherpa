# code-sherpa

Rust のプロジェクト（edition 2024）。ビルドツールは `cargo`。

## 開発コマンド

```bash
cargo build                       # ビルド
cargo fmt --all --check           # フォーマットチェック（--check を外すと整形）
cargo clippy --all-targets -- -D warnings  # リンター
cargo test --all                  # テスト
cargo run --bin sherpa -- <issue_number>  # 実行
```

## ドキュメント

- `docs/design.md` - アーキテクチャ・技術選択・ハーネス計画の設計ドキュメント
- `docs/decisions/` - ADR（Architecture Decision Records）。命名規則: `{NNN}-{kebab-case}.md`
- `docs/prompts/` - パイプラインのプロンプトテンプレート。`{{var}}` 構文

> 言語は当初 Python だったが、ADR-007 で Rust に移行した（配布の単純化＝単一バイナリ）。背景は ADR-006 / ADR-007 を参照。

## Git 規約

- ブランチ命名: `{type}/{kebab-case}`（例: `chore/setup-dev-tooling`, `docs/add-structured-observation-adr`）
- GitHub Actions は commit hash でピン留め + バージョンコメント
