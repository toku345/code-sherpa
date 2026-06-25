# code-sherpa

GitHub Issue を検知から PR 作成・マージ判断まで自律的に進行させるパイプラインマネージャー（Rust）。

各ステージで AI コーディングエージェント（Claude Code / Codex CLI）をサブプロセスとして起動し、ステージ間の判定・遷移・リトライは決定論的なプログラムが制御する。設計は [`docs/design.md`](docs/design.md) を参照。

現時点の Rust 実装はパイプライン primitives と CLI skeleton までで、ステージ orchestration は未実装。

## 開発

```bash
cargo build
cargo test --all
cargo run --bin sherpa -- <issue_number>
```

## ローカルインストール

```bash
cargo install --path .
```

インストール後は、対象リポジトリの中で実行する。`origin` remote から `owner/repo` を推定する。

```bash
cd target-repo
sherpa <issue_number>
```
