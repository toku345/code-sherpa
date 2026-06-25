# ADR-006: Landscape 再検証と方針リフレッシュ（2026-06）

- **ステータス:** Accepted
- **日付:** 2026-06-24
- **関連:** design.md セクション 3.1（言語選定）・3.2（実装スタイル）・6.6（セキュリティ）, ADR-004（セキュリティ）, ADR-005（観察ログ）, ADR-007（Rust 採用）

## コンテキスト

design.md の主要な設計判断は 2026-03 のセッションに基づく。その後約 3 ヶ月、実装はプリミティブ層（`run_cmd` / `run_agent` / `load_prompt` ＋ `Stage` enum ＋ `PipelineContext`）で止まり、ステートマシン本体は未着手のまま休眠していた（直近コミットは dependabot による依存更新のみ）。

この間に外部環境と前提が動いたため、コードを書き進める前に design.md の前提を再検証する。再検証は loop engineering の知見（外部状態の永続化・停止条件の分類・ラウンド予算）の取り込み検討も兼ねる。

### 前提変更 1: プロジェクトの目的変更

- **旧:** 転職活動用ポートフォリオ。「Python で AI オーケストレーターを設計・実装できる」ことの証明（design.md §3.1）。
- **新:** 当面 転職活動を開始できない状況に変わったため、**自分が日常的に使う実用ツール**として仕上げることが第一目的。
- **含意:** 「自作の学習・signal 価値」を理由にした build 寄りの判断が弱まり、**実用優先（build/adopt の adopt 側）**に重心が移る。一方で Rust は「キャリア signal」ではなく「個人的に使いたい・学びたい」が動機として残る。

### 前提変更 2: 課金モデル（最重要・現状動揺中）

- design.md §6.6 / ADR-004(D) は「Agent SDK は Max 定額が使えず API 課金が必要」を前提に、`claude -p` shell-kick（Max 定額で動く）を選好した。
- 2026-06-15、Anthropic は **`claude -p`・Agent SDK・GitHub Actions 等の programmatic 利用を定額枠から分離し、別建ての Agent SDK credit（Max 5x=$100 / 20x=$200）＋超過は API レート**へ移す変更を告知。ただし**現在 pause 中**で、一次情報は「For now, nothing has changed: ... still draw from your subscription's usage limits」と明記。
- **含意:**
  - 現状: `claude -p` は依然 Max 定額枠で動く。緊急の作り直しは不要。
  - 方向: programmatic 利用は将来メータリングされる公算が高い。**「リトライ3回」が直接ドル課金になりうる**ため、設計は billing-aware にする。
  - 副次: 「SDK は課金・shell-kick は定額」という §6.6/ADR-004(D) の論拠は消滅しつつある。SDK vs shell-kick は今後「課金」ではなく「機能と複雑さ」で判断する。

### 前提変更 3: ネイティブ primitive の獲得（コアと重複）

Claude Code が休眠中に以下を獲得し、code-sherpa が自作予定だったオーケストレーション層と重複し始めた。

- **Dynamic Workflows**（2026-05-28, research preview）: JS スクリプトで数百サブエージェントを決定論制御。最大同時 16・通算 1000。
- `/loop`・`/schedule`（定期/自走起動）, `--worktree` / `isolation: worktree`, subagent, background task。

含意は「build/adopt の線引き」の節で扱う。

### 前提変更 4: Codex 接続の制約

- Codex CLI は ChatGPT プラン or API キー。**API キー mode は cloud 機能（GitHub/Slack 連携）が削られ、新モデルも遅延**。App Server（JSON-RPC）は最もリッチだが統合コスト高（design.md §2.4 既知）。

## 決定

### 1. 決定論的パイプライン アーキテクチャを維持する

ネイティブ primitive と重複するが、code-sherpa の差別化は「GitHub Issue を山頂（マージ）まで運ぶ**ドメイン特化の縦パイプライン**（plan→review→implement→test→PR→merge ＋ リトライ/エスカレーション/停止条件）」にある。Dynamic Workflows は loop/branch/中間結果を持つ反復オーケストレーションで `claude -p`/Agent SDK からも使え、決定論的 Manager と**実質的に重なる部分がある**。それでも初期不採用とするのは、GitHub Issue の永続状態・PR/merge 境界・Codex 併用・監査ログを自前で保持する必要があるため（汎用 fan-out では代替しきれない差別化軸）。決定論的 Manager（design.md §2.1）・テキストリレー（§2.2）・Agent A/B 分離（§2.3）は維持する。

### 2. build / adopt の線引き

| 区分 | 対象 | 方針 |
|------|------|------|
| **build（自作）** | ステージ遷移ステートマシン, リトライ/エスカレーション, 停止条件, 状態永続化, ラウンド予算 | 縦パイプラインの中核。自作する。 |
| **adopt（既製活用）** | worktree 隔離 | `git worktree` / `--worktree` を利用 |
| **adopt 検討** | Automations（§7 のポーリング/ハートビート） | 自作ポーラーより `/schedule` 等ネイティブ起動に寄せる選択肢を優先検討 |

### 3. billing-aware 設計を最初から組み込む（loop engineering の知見）

- **ラウンド予算 / 全体デッドライン:** 「リトライ上限3回」を無限ループ防止だけでなく**コスト上限**として一級市民にする。パイプライン全体のデッドラインも持つ（現状はステージ単位 timeout のみ）。
- **状態の永続化（Memory）:** `PipelineContext` をプロセス内 struct（in-memory context）から外部永続化（どのステージまで到達したか）へ拡張。ADR-005 の構造化観察ログと接続し、クラッシュ後再開・監査・セルフホスティング履歴に効かせる。
- **停止条件の明示分類:** 合格 / ロールバック / 人間引き渡し / タイムアウト の4種を明示。特に**ロールバック経路（worktree 破棄・ブランチ削除）を停止条件として一級市民化**する。
- モデル tier 分け（例: plan-review は安価モデル）を将来の課金変更に備えて設計に残す。

### 4. 実装言語を Rust に切り替える

詳細は ADR-007 に分離。本 ADR では design.md §3.1 の言語決定を見直す方針のみ記録する。動機は配布の単純化（単一バイナリ）と個人的な Rust 学習。アーキテクチャが shell-kick / テキストリレーで言語非依存なこと、沈んだコストが最小（プリミティブのみ）なこと、§3.1 の反 Rust 論拠（compile サイクル → プロンプト外部化で緩和、SDK 移行 → shell-kick 採用済みで論拠減衰）が変化したことが根拠。

## 検討して不採用・保留にしたもの

### agmsg（不採用）

- [fujibee/agmsg](https://github.com/fujibee/agmsg): 共有ローカル SQLite を介した**エージェント間ピアツーピア メッセージング**基盤（Bash + SQLite）。
- **不採用理由:**
  1. **設計原則の衝突:** agmsg は agent 同士が自由に通信する swarm モデル。code-sherpa は「フリートオーケストレーターではない」（§1.2）・決定論的 Manager が唯一のルーター（§2.1）・コンテキスト分離が監査可能性のメリット（§2.2）。制御権を共有バスに明け渡すと3本柱が崩れる。
  2. **実行モデルの非互換:** agmsg の配信（monitor / turn）は持続セッション前提だが、code-sherpa は `claude -p` 使い捨て one-shot。リアルタイム性が乗らない。
  3. **コスト削減効果なし:** code-sherpa の agent 間受け渡しは `run_agent`（stdin→JSON stdout）＋ ファイル（plan.md / worktree）で既に最小。agmsg が消す「人間の copy-paste 配達」という問題が存在せず、SQLite 依存と導入コストが純増する。
- **位置づけ:** code-sherpa の部品としては不採用。手動で複数エージェントを対話協調させる**独立した道具**としては有用。将来 §7 の「複数 Issue 並列（fleet）」に踏み込む場合は再評価。

## 将来の検討候補

### apple/container での実行（実行環境レイヤーの強化）

- [apple/container](https://github.com/apple/container): Apple Silicon 上で各コンテナを軽量 VM（microVM 相当）で隔離。
- **適合度:** ADR-004 の3層脅威モデルの**第1層（実行環境）**を強化する。§6.6 が Docker Sandbox を退けた「Linux ではコンテナ隔離どまり」への、macOS 側の microVM 隔離という回答になりうる。
- **限界・未解決:**
  - §2.5 の核心「prompt injection には action-level sandbox ＞ プロセス隔離」は不変。microVM 内の行動は制限しないため sandbox の**代替ではなく補完**。
  - §6.6 の「VM/コンテナ内で Max OAuth 認証をどう通すか（token 受け渡し）」は未解決のまま。
  - macOS 専用。§6.6 が挙げた DGX Spark（Linux）実行パスは別途必要。
- **判断:** 今は実装しない。第1層強化の将来オプションとして記録（ADR-004「コンテナ化への移行判断基準」とも接続）。

## 参考資料

- [Use the Claude Agent SDK with your Claude plan（一次情報・課金）](https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan)
- [Anthropic June 15 2026 Billing Change 解説](https://codersera.com/blog/anthropic-june-2026-billing-change-claude-code/)
- [Common workflows - Claude Code Docs（Dynamic Workflows / loop / schedule）](https://code.claude.com/docs/en/common-workflows)
- [Using Codex with your ChatGPT plan](https://help.openai.com/en/articles/11369540-using-codex-with-your-chatgpt-plan)
- [fujibee/agmsg](https://github.com/fujibee/agmsg)
- [apple/container](https://github.com/apple/container)
- loop engineering: [Addy Osmani](https://addyosmani.com/blog/loop-engineering/) / [Across Studio Blog](https://zenn.dev/acrosstudioblog/articles/38509c0473683a)
