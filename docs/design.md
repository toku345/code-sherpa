# code-sherpa 設計ドキュメント

> Issue を山頂（マージ）まで導くシェルパ
> — GitHub Issue の検知から PR 作成・マージ判断までを自律的に走りきるパイプラインマネージャー

## 1. プロジェクト概要

### 1.1 何であるか

code-sherpa は、GitHub Issue を入力として受け取り、プラン作成・レビュー・実装・テスト・コードレビューの一連のステージを自動で進行させるパイプラインマネージャーである。各ステージで AI コーディングエージェント（Claude Code, Codex CLI 等）をサブプロセスとして呼び出し、ステージ間の判定・遷移・リトライは決定論的なプログラムとして制御する。

### 1.2 何でないか

- **フリートオーケストレーター（Agent Orchestrator / Symphony 等）ではない。** 複数 Issue の同時並列管理は初期スコープ外。1 Issue のライフサイクルを確実に走りきることに集中する。
- **コーディングエージェントそのものではない。** コードを書くのはあくまで Claude Code や Codex CLI であり、code-sherpa はそれらを「道具として」各ステージで起動する存在。
- **ノールックマージツールではない。** 初期スコープでは最終マージの判断は人間が行う。自動化の範囲は信頼度に応じて段階的に拡大する（セクション 6.4 参照）。

### 1.3 既存ツールとのポジショニング

| ツール | 特徴 | code-sherpa との違い |
|--------|------|---------------------|
| Composio Agent Orchestrator | 並列エージェントフリート管理。プラグインアーキテクチャ。TypeScript 製。 | code-sherpa は 1 Issue の深いパイプライン制御に特化。 |
| OpenAI Symphony | 仕様書(SPEC.md)中心。Linear ポーリング + Codex app-server。Elixir 製。 | code-sherpa は GitHub Issues + Shell 直接キック。言語は Python。 |
| Claude Code / Codex CLI 単体 | 与えられたプロンプトの範囲で作業するのは得意だが、ステージ遷移のメタ制御が弱い。 | code-sherpa がメタ制御レイヤーを提供する。 |


## 2. アーキテクチャ

### 2.1 設計原則

1. **マネージャーは LLM ではなく決定論的プログラムとして書く。** LLM にステージ遷移の判断を委ねるとブレるリスクがある。ステージ遷移の判定・リトライ・エスカレーションはコードで制御する。LLM の出力（approve/reject 等）は遷移条件の入力として使うが、遷移のトリガーとタイムアウト管理はマネージャーが握る。
2. **各ステージの入出力を監査可能なログとして残す。** エージェントにテキストを渡し、テキストを受け取る「テキストリレー」方式。ステージ間でエージェントのコンテキストが切れることは、各ステージが独立して検証可能というメリットでもある。
3. **テスト・アプリ起動などの確実性が求められる操作は LLM に任せない。** subprocess や docker で決定論的に実行し、終了コードと stdout/stderr で判定する。

### 2.2 パイプラインフロー

```
[GitHub Issue 検知]  ← Manager: GitHub API ポーリング
       ↓
[プラン作成]         ← Agent A: Issue 内容 + コードベース構造 → plan.md
       ↓
[プランレビュー]     ← Agent B: plan.md の妥当性検証 → approve/reject
       ↓  ← reject → プラン作成に戻す（最大3回、超過→エスカレーション）
[ブランチ作成]       ← Manager: git worktree / checkout -b
       ↓
[実装]               ← Agent A: plan.md に従って実装
       ↓
[テスト実行]         ← Manager: uv run pytest, ruff, mypy
       ↓  ← fail → Agent A にログを渡して修正（最大3回）
[Smoke Test]         ← Manager: docker compose up → curl /health → smoke test script
       ↓  ← fail → Agent A に差し戻し
[PR 作成]            ← Manager: git push, gh pr create
       ↓
[コードレビュー]     ← Agent B + CodeRabbit / pr-review-toolkit
       ↓  ← changes requested → Agent A が修正 → テスト実行から再実行（最大3回）
[マージ判断]         ← Human: 通知を受けて approve/reject
       ↓  approve
[マージ & クリーンアップ] ← Manager: gh pr merge --squash, worktree 削除, Issue クローズ
```

### 2.3 担当分け

| 担当 | 役割 | 具体例 |
|------|------|--------|
| ⚙️ Manager（決定論的プログラム） | 確実性が求められる操作、ステージ遷移の判定 | Issue 検知, ブランチ作成, テスト実行, アプリ起動, PR 作成, マージ, クリーンアップ, リトライ上限管理 |
| 🤖 Agent（LLM） | 創造性・判断力が求められる作業 | プラン作成, プランレビュー, 実装, テスト失敗の修正, コードレビュー, レビュー指摘対応 |
| 👤 Human（人間） | 最終判断・エスカレーション | マージ判断, 3回リトライ失敗時の介入 |

**重要: Agent A と Agent B は別プロンプト（または別モデル）で、自分の仕事を自分でレビューしない構造にする。**

### 2.4 エージェント接続方式

**Shell 直接キック（`claude -p` / `codex exec`）を採用する。**

```python
def run_agent(prompt: str, **kwargs) -> str:
    cmd = ["claude", "-p", prompt, "--output-format", "json"]
    result = subprocess.run(cmd, capture_output=True, text=True, check=True, **kwargs)
    return result.stdout
```

#### 検討した代替案と不採用理由

| 方式 | 利点 | 不採用理由 |
|------|------|-----------|
| Claude Agent SDK (Python/TS) | 同一コンテキスト内で会話継続可能、カスタムツール注入 | 初期段階ではテキストリレーで十分。SDK が必要になったら移行する。 |
| Codex App Server (JSON-RPC) | 最もリッチな制御、承認フローのハンドリング | Codex 専用。統合コストが高い。エージェント切り替え時に抽象化が崩れる。 |

Shell 直接キック → SDK への移行パスは Python なら摩擦が最小（Claude Agent SDK が Python で提供されている）。


## 3. 技術選択

### 3.1 言語: Python

#### 選定理由

- **プロトタイプ速度:** `subprocess.run()` + `json.loads()` で即座に動く。Bash と同程度にシンプルだが、構造化されている。
- **ポートフォリオ適性:** AI/ML エンジニアへのキャリア転換において「Python でオーケストレーターを設計・実装できる」ことの証明になる。
- **SDK 移行の自然さ:** Claude Agent SDK が Python で提供されているため、将来の移行コストが最小。
- **試行錯誤との相性:** コンパイル不要。プロンプトチューニングとアーキテクチャ探索を同時に行うフェーズで、`python pipeline.py 123` の即時実行が効く。

#### 検討した代替案と不採用理由

| 言語 | 利点 | 不採用理由 |
|------|------|-----------|
| Bash | 依存ゼロ、シェルコマンドがネイティブ | 30行超で可読性が急落。JSON パースが辛い。ステートマシンが書けない。 |
| TypeScript | Agent Orchestrator と同じ言語 | 例外処理・型のボイラープレートが多く、glue コードの比率が高い。 |
| Rust | 型安全なステートマシン、enum + match の網羅性チェック | String/&str 変換、Result ハンドリング等、glue コードに対してボイラープレート比率が高い。コンパイルサイクルが遅い。 |
| Go | シングルバイナリ、goroutine での並行処理 | Claude Agent SDK に Go バインディングがない。`if err != nil` の繰り返し。 |
| Clojure | データ指向設計、ステートマシンをデータで表現 | サブプロセス管理が Java interop 経由。面接官が Clojure を読める確率が低い。 |

### 3.2 実装スタイル: 薄いスクリプト

- フレームワーク不要、クラス階層不要
- `dataclass` + `Enum` + `match-case` + `subprocess` のみ
- 全体で 200〜300 行を目標
- 外部ライブラリは最小限（PyGithub 程度）


## 4. Harness Engineering

### 4.1 なぜ Harness が必要か

code-sherpa がエージェントを起動してコードを書かせるとき、エージェントがリポジトリの構造・規約・設計意図を理解できなければ品質の高い出力は得られない。Harness engineering は「エージェントが仕事をするための環境整備」であり、code-sherpa が機能するための前提条件。

特に code-sherpa で code-sherpa 自身を開発する（セルフホスティング）場合、harness の品質がそのまま開発の成否を決める。

### 4.2 初期 Harness 構成（Phase 0 完了時の目標）

```
code-sherpa/
├── AGENTS.md              # 目次（〜100行）。リポジトリ構造、規約へのポインタ
├── docs/
│   ├── architecture.md    # パイプラインのステージ設計、担当分け
│   ├── decisions/         # ADR (Architecture Decision Records)
│   │   ├── 001-python.md
│   │   ├── 002-shell-kick.md
│   │   └── 003-text-relay.md
│   ├── prompts/           # 各ステージのプロンプトテンプレート
│   │   ├── plan.md
│   │   ├── plan-review.md
│   │   ├── implement.md
│   │   └── code-review.md
│   └── workflow.md        # 全体フロー、リトライポリシー、エスカレーション基準
├── pipeline.py            # メインのパイプラインマネージャー
├── pyproject.toml         # uv 経由で test, lint, type check を実行
└── tests/
```

### 4.3 Harness の 3 要素

| 要素 | 内容 | code-sherpa での実装 |
|------|------|---------------------|
| Context Engineering | エージェントに必要な情報をリポジトリ内に集約 | AGENTS.md（目次）, docs/（詳細）, docs/prompts/（テンプレート）, ADR |
| Architectural Constraints | 決定論的なルールで品質を強制 | ruff（リンター）, mypy（型チェック）, CI, プロンプトテンプレートのバリデーション |
| Garbage Collection | ドキュメントの腐敗を検出・修正 | 初期は手動。安定後に code-sherpa 自身で定期チェック Issue を作る |

### 4.4 Harness の育て方

- **初日から完璧を目指さない。** AGENTS.md + docs/prompts/ + CI の 3 点セットで始める。
- **エージェントが躓いたらそれをシグナルとして扱う。** 何が足りなかったか（ドキュメント？ガードレール？テスト？）を特定し、harness にフィードバックする。
- **Mitchell Hashimoto のルール:** 「エージェントがミスをするたびに、そのミスが二度と起きないよう解決策をエンジニアリングする」


## 5. セルフホスティング計画

code-sherpa で code-sherpa 自身を開発するためのブートストラップ計画。

### Phase 0: 手動構築

- Claude Code を直接使って pipeline.py の初版を書く
- AGENTS.md, docs/, Makefile, CI を整備
- この段階で harness の土台を固める

### Phase 1: 半自動

- code-sherpa v0.1 が動くようになったら、GitHub Issue に機能追加を書いて code-sherpa に実装させる
- 例: 「smoke test ステージを追加する」「リトライ回数を設定ファイルから読むようにする」
- マージは人間が判断

### Phase 2: ドッグフーディング

- バグ修正、小さな機能追加を code-sherpa 経由で回す
- 失敗したら harness を改善するフィードバックループを回す
- code-sherpa 自身の改修 PR が、code-sherpa の品質の証明になる


## 6. 付録: 設計セッションメモ（2026-03-14）

> 以下は設計セッションでの議論の記録。正式な設計判断はセクション 1〜5 に反映済み。

### 6.1 Agent Orchestrator vs Symphony の分析から

- 両ツールとも「並列フリート管理」に寄っており、1 Issue のパイプライン制御の深さは各エージェントの能力に委ねている。code-sherpa はこの「パイプラインの各ステージをどう制御するか」に特化する。
- Symphony の SPEC.md は「オーケストレーターに必要な抽象化レイヤー」の設計ドキュメントとして参考になる。特に Tracker → Orchestrator → Workspace → Agent Runner → Observer → Logger のレイヤー分け。
- Symphony の「ワークフロー上の成功は Done ではなく次のハンドオフ状態（例: Human Review）に到達すること」という定義は、code-sherpa の「マージ判断」ステージの設計に影響。

### 6.2 エージェント接続方式の検討から

- Shell 直接キック・SDK・App Server の 3 方式を比較した結果、初期は Shell 直接キックで十分。
- 各ステージ間でコンテキストが切れることはデメリットではなく「各ステージが独立して検証可能」というメリット。
- 「テキストリレー」方式は「codebase as financial statements」アナロジーに通じる「各ステージの入出力が監査可能なログとして残る」利点がある。

### 6.3 レビューの品質について

- CodeRabbit と Claude Code の pr-review-toolkit が成熟してきており、コードレビューの品質は時間が解決してくれそう。
- プランレビューの品質基準は要注意。LLM に「このプランは良いか？」と聞くと大抵「良いですね」と返る。具体的な拒否基準（変更ファイル数上限、テスト戦略の有無、破壊的変更の検出等）をルールベースで持つか、批判的プロンプトで聞くか、設計が必要。

### 6.4 安全設計について

- 差し戻しの無限ループ問題に対しては、全ステージ統一でリトライ上限 3 回 + 人間へのエスカレーション。
- 「介在なしで進める」の範囲は段階的に拡大する。最初はマージ手前で止めて通知（PR を作るところまで自動化）。信頼度が上がったら段階的にマージまで自動化。
- レビュー指摘修正後はテスト実行からやり直すフローにし、修正が別の箇所を壊していないかを確認する。

### 6.5 動作確認について

- テストだけでなくアプリケーションを実際に起動して確認するステージも欲しい。
- ただしこれはエージェントではなくマネージャーが担当すべき。`docker compose up` → `curl` → レスポンスコード確認のような決定論的スクリプト。
- E2E テストがあればそれを回す、なければ smoke test スクリプトを事前に用意しておく。


## 7. 今後の課題・未決定事項

- [ ] プロンプトテンプレートの具体的な内容設計
- [ ] プランレビューの拒否基準の定義（ルールベース vs 批判的プロンプト vs ハイブリッド）
- [ ] GitHub API ポーリングの実装（対象ラベル、ポーリング間隔、Webhook への移行判断）
- [ ] Smoke Test の汎用的な仕組み（プロジェクトごとに異なるため設定ファイル化が必要）
- [ ] 通知手段の選定（Slack, GitHub Notifications, ntfy.sh 等）
- [ ] 複数 Issue の同時処理への拡張判断（いつ・どう拡張するか）
- [ ] CI/CD との統合方式（GitHub Actions で code-sherpa を動かすか、別のランナーか）


---

*このドキュメントは 2026-03-14 のセッションで議論した内容に基づく。*
*code-sherpa の開発が進むにつれて、ADR として個別の決定を記録し、このドキュメントは概要として維持する。*
