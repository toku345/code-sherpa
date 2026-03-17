# ADR-004: セキュリティアーキテクチャ — 実行環境とクレデンシャル管理

- **ステータス:** Accepted（Phase 0 方針）
- **日付:** 2026-03-16
- **関連:** ADR-002 (shell-kick, 未作成), セクション 2.4（エージェント接続方式）

## コンテキスト

code-sherpa は AI エージェントに GitHub リポジトリの操作権限を渡してコードを書かせる。このとき、以下の脅威に対処する必要がある:

1. **ホスト OS / ローカル環境の破壊** — エージェントがホスト上のファイルを削除・改変する
2. **クレデンシャルの窃取** — prompt injection で `~/.ssh/` や `~/.claude/` 配下の認証情報を読み取り、外部に送信する
3. **渡したクレデンシャルの権限内での暴走** — GitHub token を使って main を force push する、大量のゴミ PR を作る、CI を大量に走らせてコスト爆発させる等

脅威 1・2 は実行環境の隔離で対処できるが、脅威 3 は sandbox も container も microVM も無力である。正当な API コールを sandbox は区別できない。

## 検討した選択肢

### A. Docker Sandbox (`docker sandbox run claude`)

Docker Desktop の sandbox 機能（Model Runner）。macOS/Windows では microVM、Linux ではコンテナベースの隔離を提供する。

- **利点:** ワンコマンドで起動できる。`--dangerously-skip-permissions` がデフォルトで有効になり、permission prompt が不要。
- **問題点:**
  - Linux（DGX Spark）では microVM ではなくコンテナベースの隔離になり、セキュリティ上の優位性が薄い。
  - Max プラン（OAuth 認証）との相性が悪い。`apiKeyHelper: "echo proxy-managed"` が OAuth を検出できず認証が壊れる既知の問題がある（docker/for-mac#7842）。ワークアラウンド（`CLAUDE_CODE_OAUTH_TOKEN` + `hasCompletedOnboarding` フラグ注入）は脆く、Claude Code のアップデートで壊れる可能性がある。
  - devcontainer 同様、CLI 中心のワークフローとの摩擦がある。ライフサイクル管理が code-sherpa のオーケストレータから見て余計な複雑さになる。

### B. 素の Docker コンテナ + YOLO モード

自前の Dockerfile で Claude Code 入りコンテナを構築し、`--dangerously-skip-permissions` で実行する。

- **利点:** CLI 親和性が高い。docker-compose でフェーズごとのコンテナ分離が自然にできる。
- **問題点:**
  - 「箱の外は守るが箱の中は無法地帯」になる。脅威 2（クレデンシャル窃取）に対して、コンテナ内に認証情報をマウントする必要があるため、コンテナ内では保護されない。
  - Claude Code の sandbox をコンテナ内でネストさせようとすると `enableWeakerNestedSandbox` が必要になり、sandbox の強度が落ちる。
  - Max プランの OAuth 認証をコンテナ内で扱う問題は Docker Sandbox と同様に残る。

### C. Claude Code の built-in sandbox + ホスト直接実行（採用）

Claude Code の sandbox モードを有効にし、ホスト PC 上で直接 `claude -p` を実行する。

- **利点:**
  - sandbox は OS レベルのプリミティブ（Linux: bubblewrap, macOS: Seatbelt）で filesystem と network の両方を隔離する。
  - ネットワークは unix domain socket 経由のプロキシを通るため、`allowedDomains` で outbound を制御でき、inbound も構造的に遮断される。prompt injection による exfiltration を防げる。
  - sandbox は Claude Code が spawn するサブプロセスにも適用される。
  - 認証の問題が発生しない。ホスト上で `claude login` した認証情報をそのまま使える。Max プランとの相性問題がない。
  - Claude Code プロセス自体は `~/.claude/` を読めるが、AI がツール経由で実行するコマンドやファイル読み取りは sandbox がブロックする。
- **制約:**
  - 脅威 3（GitHub token の権限内での暴走）には対処できない。これは別のレイヤー（branch protection, fine-grained PAT）で対処する。
  - Linux の bubblewrap 実装はまだこなれていない部分がある（sandbox の中でさらに sandbox を開く場合等）。ただし code-sherpa ではネストの必要がないため問題にならない。

### D. Claude Agent SDK（Python）+ API キー課金

Agent SDK を使い、フェーズごとに `allowed_tools` を変えて権限を制御する。

- **利点:** フェーズごとの権限分離が SDK レベルで自然に表現できる。認証は API キーで安定。
- **問題点:** Anthropic は Agent SDK の利用に対して API キー認証を推奨しており、Max プランの定額利用ができない。自律パイプラインではコストが予測困難。
- **判断:** Max プランの定額内で運用する方針のため、現時点では不採用。パイプラインが価値を証明した後、コスト対効果を再評価する。

## 決定

**選択肢 C を採用する。** Claude Code の built-in sandbox をホスト上で有効にし、`claude -p` で直接実行する。

一見「コンテナで隔離した方が安全」に見えるが、AI エージェントの脅威モデル（prompt injection による意図しない操作）に対しては、行動一つ一つに制約をかける sandbox の方が的確に機能する。コンテナ + YOLO は「箱の外は守るが箱の中は無法地帯」であり、認証情報をコンテナ内にマウントする必要がある以上、肝心のクレデンシャル保護が弱くなる。

## セキュリティ構成（Phase 0）

### レイヤー 1: 実行環境の隔離 — Claude Code sandbox

```jsonc
// .claude/settings.json
{
  "sandbox": {
    "enabled": true,
    "allowUnsandboxedCommands": false,
    "network": {
      "allowedDomains": [
        "api.anthropic.com",
        "*.anthropic.com",
        "github.com",
        "*.githubusercontent.com",
        "*.npmjs.org",
        "pypi.org",
        "files.pythonhosted.org"
      ]
    },
    "filesystem": {
      "denyRead": ["~/.aws/credentials", "~/.ssh"]
    }
  },
  "permissions": {
    "deny": [
      "Bash(rm -rf *)",
      "Bash(chmod 777 *)",
      "Read(./.env)",
      "Read(./.env.*)",
      "Read(**/*.pem)",
      "Read(**/*.key)"
    ]
  }
}
```

> **注:** `permissions.deny` の Bash パターンはヒューリスティックな補助ガードであり、コマンドの書き方を変えるだけでバイパスできる（例: `rm -r -f`、`bash -c 'rm -rf /'`）。実質的な保護は `sandbox` の filesystem / network 隔離が担う。

**何を守るか:** ホスト OS の破壊（脅威 1）、クレデンシャルの窃取・外部送信（脅威 2）

### レイヤー 2: GitHub 権限の最小化 — Fine-grained PAT + Branch Protection

**Fine-grained Personal Access Token:**
- リポジトリスコープ: 対象リポジトリのみ
- `contents: write` — ブランチへの push に必要
- `issues: write` — Issue のコメント・クローズに必要
- `pull_requests: write` — PR 作成・コメントに必要
- `administration` — **付与しない**（branch protection rule の変更を防ぐ）

**Branch Protection Rules（Phase 0）:**
- `main` への直接 push を禁止
- PR 必須
- ステータスチェック（CI）の通過を必須
- 「Require approvals」は Phase 0 では **設定しない**

> **なぜ approval を必須にしないか:** Phase 0 では Fumitaka 個人の PAT で PR を作成するため、PR の作成者とレビュアーが同一アカウントになる。GitHub は自分が作った PR を自分で approve できないため、approval 必須にすると運用が回らない。CI パス必須 + 人間による目視確認・手動マージで実質的なガードレールとする。

**何を守るか:** クレデンシャル権限内での暴走（脅威 3）のうち、最も致命的な「main の破壊」を防ぐ

### レイヤー 3: パイプラインレベルの制御 — Manager による制約

- リトライ上限 3 回 + 人間エスカレーション（既存設計）
- PR 作成はマネージャーが `gh pr create` で行い、エージェントに直接 push させない
- マージは人間が承認（Phase 0）

**何を守るか:** 暴走の影響範囲の限定（大量ブランチ作成、CI スパム等）

## 将来の Hardening パス

現時点では実装しないが、パイプラインが価値を証明した後に検討するセキュリティ強化策:

### エージェント用アカウントの分離（PR の自己承認問題の解決）

Phase 0 では個人アカウントの PAT で PR を作成するため、approval 必須の branch protection が使えない。以下のいずれかで解決する:

- **方式 A: Machine User アカウント** — `code-sherpa-bot` のような専用アカウントを作成し、その PAT で PR を作成する。Fumitaka が approve してマージ。GitHub の利用規約上、machine user は許可されている。
- **方式 B: GitHub App** — code-sherpa を GitHub App として登録し、installation token で PR を作成する。App が作った PR は bot 扱いになり、人間が approve できる。fine-grained PAT よりさらに細かい権限制御が可能。将来的に最も筋が良い。

いずれかを導入した時点で、branch protection に「Require approvals（最低 1）」を追加する。

### Git プロキシパターン（Claude Code on the web の設計に倣う）

Claude Code に GitHub token を直接渡さず、git 操作を中継するプロキシを挟む。プロキシ側で「main への push は拒否」「PR 作成は 1 時間に N 件まで」等のポリシーを強制する。sandbox のネットワーク制限で `github.com` への直接通信を遮断し、プロキシ経由のみ許可する。

```text
[Claude Code] --sandbox network--> [Git Proxy] --validated--> [GitHub]
                                       ↑
                              ブランチ名検証、レート制限、
                              操作種別の制限をここで強制
```

### フェーズごとの権限分離

パイプラインの各フェーズで異なるスコープの token を使う:
- Issue 検知・プラン作成: `issues: read` + `contents: read` のみ
- 実装: 上記 + `contents: write`（特定ブランチのみ）
- マージ: 人間が操作（token 不要）

Shell 直接キック方式では環境変数の差し替えで実現可能。Agent SDK に移行すればより自然に表現できる。

### コンテナ化への移行判断基準

以下の条件のいずれかを満たした場合、Docker コンテナでの実行に移行する:
- code-sherpa を SaaS 化して他人のリポジトリを扱う場合（コンテナエスケープが脅威になる）
- 複数 Issue の並列処理で環境の完全分離が必要になった場合
- Claude Code プロセス自体の脆弱性が実際に報告された場合

## 参考資料

- [Claude Code Security - 公式ドキュメント](https://code.claude.com/docs/en/security)
- [Claude Code Sandboxing - 公式ドキュメント](https://code.claude.com/docs/en/sandboxing)
- [Beyond permission prompts: making Claude Code more secure and autonomous - Anthropic Engineering Blog](https://www.anthropic.com/engineering/claude-code-sandboxing)
- [【2026年最新版】Claude Codeで行うべきセキュリティ設定 10選 - Qiita](https://qiita.com/miruky/items/51db293a7a7d0d277a5d)
- [docker/for-mac#7842 - Docker Sandbox OAuth 認証問題](https://github.com/docker/for-mac/issues/7842)
