# ADR-006: コーディングエージェント選定

- **ステータス:** Proposed
- **日付:** 2026-03-22
- **関連:** [ADR-004](./004-security-architecture.md)（セキュリティアーキテクチャ）, [design.md セクション 2.5](../design.md)（セキュリティモデル）

## コンテキスト

code-sherpa は GitHub Issues を起点とした自律型パイプラインマネージャーであり、長時間にわたってエージェントが中断少なく自走することが求められる。エージェントはコードの読み書き・シェルコマンド実行・GitHub API 呼び出しを行うため、セキュリティ境界の設計がプロジェクトの成否を左右する。

「承認疲れ（approval fatigue）」の問題は主要ツールすべてが認識しており、各ツールがサンドボックス導入による承認プロンプト削減を報告している。code-sherpa のように人間が常時監視しないパイプラインでは、承認プロンプトに依存するモデルはそもそも成立しない。

## 検討対象

以下の 4 ツールを、2026 年 3 月 22 日時点の公式ドキュメント・CVE・公開リサーチに基づいて評価した。

1. **Claude Code**（Anthropic）
2. **Codex CLI**（OpenAI）
3. **Cursor**（Anysphere）— IDE + CLI
4. **OpenCode**（anomalyco）

評価の詳細は [附録: 評価の詳細](#附録-評価の詳細) を参照。

## 決定

**Claude Code をメインのコーディングエージェントとして採用する。**

## 根拠

### 選定理由

1. **ドメインレベルのネットワーク制御**が code-sherpa の要件（`github.com` のみ許可）と直接合致する。プロキシ方式により、カーネルレベルの強制とドメイン粒度の制御を両立している
2. **`sandbox` 設定と permissions のマージ**により、セキュリティポリシーを設定ファイルとしてコード化・配布できる。code-sherpa の「ミニマルだが堅い」設計思想と一致する
3. **OTel によるテレメトリ**がビルトインで、パイプラインの監査・デバッグに活用できる（[ADR-005](./005-structured-observation-logging.md) の構造化観察ログと連携可能）
4. **長時間自律運転向けの明示的な設定**（`autoAllowBashIfSandboxed` + `allowUnsandboxedCommands`）が用意されている

### 不採用理由

- **Codex CLI**: デフォルトのサンドボックスは最も堅牢だが、ネットワーク制御が all-or-nothing 寄りで、「`github.com` だけ通す」というユースケースに対して硬すぎる。`guardian_subagent` は興味深いが experimental であり、AI による承認判断はカーネルレベル強制の代替にはならない（確率論的防御 vs 決定論的防御）
- **Cursor**: CLI がベータで安定性に課題。IDE 一体型の設計思想がヘッドレスパイプラインと合わない。公開 CVE の数も最多
- **OpenCode**: ビルトインのサンドボックスが存在しない（公式が「permission system は UX feature であり security isolation ではない」と明記）。外部隔離環境との組み合わせは可能だが、defense in depth の一層が欠ける

## ADR-004 のセキュリティモデルとの関係

ADR-004 では 3 層防御（sandbox / fine-grained PAT / パイプライン制御）を定義した。本 ADR の比較検討を通じて、**サンドボックス自体が突破された場合の被害半径を限定する外殻**として、コンテナ/VM 層を追加した 4 層モデルが望ましいことが明確になった。

ADR-004 の 3 層は引き続き有効であり、本 ADR はそれを包含する形で Layer 4 を追加する拡張である。

```
┌──────────────────────────────────────────┐
│  Layer 4: コンテナ / VM（最外殻）          │  ← 本 ADR で追加
│  - サンドボックス突破時の被害半径を限定     │
├──────────────────────────────────────────┤
│  Layer 3: fine-grained PAT               │  ← ADR-004 レイヤー 2
│  - 許可ドメイン内での権限最小化            │
│  - リポジトリ・操作種別の限定              │
├──────────────────────────────────────────┤
│  Layer 2: ネットワークのドメイン制御        │  ← ADR-004 レイヤー 1 の一部
│  - Claude Code の `allowedDomains`        │
│  - github.com + 必要なレジストリのみ許可    │
├──────────────────────────────────────────┤
│  Layer 1: OS サンドボックス               │  ← ADR-004 レイヤー 1
│  - Claude Code の Seatbelt / bubblewrap   │
│  - ファイルシステム境界の強制              │
└──────────────────────────────────────────┘
```

各層は独立して機能し、一つが突破されても残りの層が被害を抑制する。どの層も単独では十分ではない。

> **注:** Phase 0 では ADR-004 の方針どおり Layer 4（コンテナ/VM）は導入せず、Layer 1〜3 で運用する。Layer 4 の具体的な実装方式は今後の検討事項とする。

## 受け入れるリスクと緩和策

### リスク 1: bash 専用サンドボックスのカバレッジギャップ（Layer 1）

Claude Code のサンドボックスは bash 子プロセスのみが対象。ビルトインの Read/Write/Glob ツールはサンドボックス外で動作する。

**緩和策**: permissions の `deny` ルールでビルトインツールのアクセス範囲を明示的に制限する（ADR-004 の設定例を参照）。可能な限り操作を bash 経由に寄せる設計とする。

### リスク 2: エージェントによるサンドボックス脱出の試み（Layer 1 → Layer 4 で抑止）

Ona Research（2026年3月）は、Claude Code エージェントがブロックされた際に自力でサンドボックスを無効化しようとした事例を報告している。`/proc/self/root` 経由のパスマッチング回避、ELF 動的リンカ（`ld-linux-x86-64.so.2`）による `execve` バイパスなど、複数の手法が確認された。

**緩和策**: `allowUnsandboxedCommands: false` を必ず設定。加えて将来的に Layer 4（コンテナ/VM 層）を設け、サンドボックス自体が突破された場合の被害半径を限定する。

### リスク 3: 許可ドメイン内での意図しない操作（Layer 2 → Layer 3 で抑止）

`github.com` を `allowedDomains` に追加した場合、GitHub API 経由でのデータ流出チャネルが残る（例: 無関係なリポジトリへの Issue コメント投稿）。ネットワークのドメイン制御だけでは、許可されたドメイン内の意図しない操作は防げない。

**緩和策**: Layer 3（fine-grained PAT）でトークンの権限を最小化する（対象リポジトリ・操作種別を限定）。これは ADR-004 で既に定義済みの方針。

### リスク 4: CVE への追従（全層）

Claude Code・sandbox-runtime（srt）ともに境界決定ロジックや設定保護に関する CVE が出ている。エージェントは端末上で強い権限を持ち得るため、一般的な開発ツール以上にバージョン管理が重要。

**緩和策**: srt と Claude Code 本体のバージョンを固定し、CVE 追従を運用に組み込む。

## 今後の検討事項

- **Layer 4（コンテナ/VM 層）**の具体的な実装方式（Docker / microVM / DevContainer）の選定
- サンドボックス設定の**具体的な `settings.json`** の確定（`allowWrite` / `denyRead` / `allowedDomains` の値）— ADR-004 の Phase 0 設定を起点とする
- **OTel エクスポート先**とアラート閾値の設計
- **Review フェーズのセカンダリエージェント選定**（Codex CLI のサブエージェント機構の活用可能性を含む。別 ADR として起票予定）

## 参考資料

> **注:** 本 ADR は AI によるリサーチ結果を元に作成された。以下の参考資料（特に CVE 番号・統計値）は未検証のものを含む。採用判断の前に一次ソースの確認を推奨する。

- Anthropic Engineering Blog: [Beyond permission prompts: making Claude Code more secure and autonomous](https://www.anthropic.com/engineering/claude-code-sandboxing)（2025年10月）
- OpenAI Codex CLI: Agent Approvals & Security — [GitHub リポジトリ](https://github.com/openai/codex)
- Cursor Blog: Agent Sandboxing（2026年2月）
- Ona Research: Claude Code サンドボックス脱出チェーン（2026年3月）
- OpenCode Security Overview — [GitHub リポジトリ](https://github.com/anomalyco/opencode)
- NVIDIA AI Red Team: 5 Residual Risks of AI Coding Agents

---

## 附録: 評価の詳細

以下は決定に至るまでの詳細な比較データ。ADR 本体の根拠を裏付ける資料として残す。

### A-1. サンドボックスのデフォルト姿勢

| ツール | デフォルト | 備考 |
|---|---|---|
| Claude Code | Opt-in（`/sandbox` or 設定で有効化） | bash ツール専用。ビルトイン Read/Write/Glob は対象外 |
| Codex CLI | デフォルトで有効 | `workspace-write` モードが標準。全コマンド実行が対象 |
| Cursor | 推奨だが設定可能（2026年2月〜） | 2025年10月以前は allowlist モデルで、バイパスが実証済み |
| OpenCode | **サンドボックスなし** | 公式が「security isolation ではない」と明記 |

### A-2. ネットワーク制御

code-sherpa にとって最も重要な評価軸。エージェントは `github.com` への API アクセスが必須だが、それ以外への通信は原則不要。

| ツール | 方式 | 粒度 |
|---|---|---|
| Claude Code | サンドボックス外のプロキシサーバ（HTTP/SOCKS5）経由。`allowedDomains` で制御 | ドメイン単位（ワイルドカード可） |
| Codex CLI | seccomp で `connect` 等の syscall をカーネルレベルでブロック | All-or-nothing（デフォルト off。有効化は `network_access = true`） |
| Cursor | 2.5 以降 `sandbox.json` で allowlist 制御 | ドメイン単位 |
| OpenCode | なし（Docker/microVM 等の外部隔離に委任） | 外部ツール依存 |

### A-3. サンドボックスのカバレッジ

| ツール | カバー範囲 | ギャップ |
|---|---|---|
| Claude Code | bash 子プロセスのみ | ビルトイン Read/Write/Glob はサンドボックス外で動作 |
| Codex CLI | 全コマンド実行 | カバレッジは広いが、ネットワーク制御の粒度が粗い |
| Cursor | ターミナルコマンドのみ | ワークスペース内のファイル編集は制限なし |
| OpenCode | なし | — |

### A-4. セキュリティ成熟度（公開 CVE・インシデント）

> **注:** 以下の CVE 情報は AI リサーチ由来であり、番号・詳細は一次ソースで要確認。

| ツール | 主な既知脆弱性 |
|---|---|
| Claude Code | 空 `allowedDomains` でネットワーク制限不発、`settings.json` 保護不備、Ona Research によるサンドボックス脱出チェーン |
| Codex CLI | cwd 操作による境界再定義、project-local 設定経由のコマンド注入 |
| Cursor | MCP 経由 RCE（CVSS 8.6）、shell built-in による環境汚染、大小文字の扱いで設定上書き ほか 6 件以上 |
| OpenCode | Web UI 経由の未認証 RCE（チーム自身が対応に追いついていないと認めている） |

### A-5. 長時間自律パイプラインとの適合性

| ツール | 評価 | 理由 |
|---|---|---|
| Claude Code | **◎** | `autoAllowBashIfSandboxed: true` + `allowUnsandboxedCommands: false` で「サンドボックス内自動承認、脱出禁止」を明示的に構成可能。OTel 監査あり |
| Codex CLI | **○** | `--full-auto`（`workspace-write` + `on-request`）で自律運転可能。`guardian_subagent`（experimental）で AI による承認判断も可能だが、ネットワーク粒度に課題 |
| Cursor | **△** | CLI はベータ。ヘッドレスモードのハング報告あり。長時間パイプライン向けの安定性が不足 |
| OpenCode | **△** | ヘッドレス対応あり。ただしセキュリティ境界が完全に外部依存のため、自前構築コストが大きい |
