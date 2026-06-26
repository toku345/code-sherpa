# implementation-notes: pipeline walking skeleton (Rust)

> このファイルは別セッションへの実装ハンドオフ用スペック。実装を進めながら「設計判断 / 逸脱 / トレードオフ / 未解決の問い」を追記すること（CLAUDE.md の implementation-notes 規約）。

## これは何か

GitHub Issue #10「pipeline happy-path stages」の **Rust 版 walking skeleton**。1つの Issue を `IssueFetch → … → PrCreation → CodeReview` まで end-to-end で1本通す決定論ステートマシンを実装する。

- 旧 `feat/pipeline-happy-path` ブランチは **Rust 移行前の Python 実装**（`pipeline.py`、merge-base が uv/mypy 時代）。main の Rust 移行に置換済みの死んだ枝。流用しない。設計スペック commit `b57c29b` に流用できる記述があるかは任意確認。
- 本ブランチ `feat/pipeline-walking-skeleton` は現 main（Rust primitives + CLI skeleton）起点。

## 起点の事実

- `src/lib.rs`: primitives — `run_cmd` / `run_agent`（`claude -p --output-format json` を stdin プロンプト起動、`result` 文字列を返す）/ `parse_agent_output` / `load_prompt`（`{{var}}` 置換）/ `Stage` enum（8種）/ `PipelineContext`。subprocess は timeout・プロセスグループ kill・pipe ドレイン済み。
- `src/main.rs`: issue 番号を受け、`git rev-parse` ＋ origin remote から `owner/repo` を解決（github.com 限定・slug 検証）。`main()` は `"pipeline orchestration is not implemented yet"` で意図的に bail。
- テストはグリーン（lib 25 / bin 7）。
- 参照: `docs/design.md` §2.1/§2.2/§2.3/§6.3、`docs/decisions/004`（セキュリティ3層）・`005`（構造化観察ログ）・`006`（worktree 隔離を adopt）・`007`（Rust・依存最小・enum+match）。

## Goal

決定論ドライバループ（ステージ遷移・リトライ上限3・エスカレーション）を中心に、各ステージ最小実装で end-to-end を1本通す。`main.rs` の bail を実パイプライン起動に置換。

## Constraints

- アーキテクチャ不変（shell-kick / text-relay / 決定論 Manager）。新規クレートは原則追加しない（`gh`/`git` は subprocess、既存 primitives を再利用）。
- 最終マージは自動化しない（design.md §1.2）。CodeReview は判定提示で停止し人間に委ねる。
- fail loud。`origin` は github.com 限定の既存検証を踏襲。行数目標 400〜600 行（ADR-007）。

## ステージ構成（改訂版）

| ステージ | 担当 | v0 実装 | フィードバック辺 |
|---|---|---|---|
| IssueFetch | Manager | `gh issue view <n> --repo <owner/repo> --json title,body` → ctx | — |
| PlanCreation | Agent A | `plan.md`（新規）→ ctx.plan（メモリ上、ディスクに書かない） | ← PlanReview reject |
| PlanReview | Agent B | `plan-review.md`（新規）→ **ReviewVerdict を fail-closed パース** | reject→PlanCreation（最大3→escalate） |
| BranchCreation | Manager | **`git worktree add` で隔離**（下記）→ ctx.worktree_path/branch_name | — |
| Implementation | Agent A | 既存 `implement.md`（plan, last_error）を worktree cwd で実行 | ← Test fail |
| TestExecution | Manager | 決め打ちゲートを順次実行（下記、コマンド注入可能に） | fail→Implementation（最大3→escalate） |
| PrCreation | Manager | **dry-run default ＋ `--publish` gate**、push + `gh pr create`（one-shot） | — |
| CodeReview | Agent B | `code-review.md`（新規）→ **判定提示して停止** | （v0 では changes ループ無し） |

## 着手前に潰す blocker（Codex レビューで確定）

### 1. レビュー判定を fail-closed schema にする
`run_agent` は `result` 文字列を返すだけ。自由文 substring 判定は `"I cannot approve; reject"` 等で誤判定する（design.md §6.3）。
- `ReviewVerdict { approve | reject | changes_requested, reasons: Vec<String> }` 相当の狭い enum を定義。
- `plan-review.md` / `code-review.md` は **先頭行または JSON 本文で verdict を機械可読に**出力させる契約にする（例: 先頭行 `VERDICT: reject` ＋ 理由）。
- 完全一致でパースし、**曖昧・欠落・複数 verdict は fail loud**。曖昧文のテストを追加。

### 2. BranchCreation は worktree 隔離（ADR-006 整合）
`git checkout -b` で live tree を使うと dirty 持ち越し・再実行時のブランチ衝突・作業ツリー汚染が起きる。
- `git worktree add <path> -b sherpa/issue-<n> <base>` で隔離。Implementation/Test はその cwd で実行。
- 終了時 `git worktree remove`（成否に応じ cleanup / 保持を決める）。
- `PipelineContext.worktree_path` を活用。ブランチ既存時の reuse/abort 規則と、開始前 `git status --porcelain` clean check も入れる。

### 3. CodeReview は stop 確定、PrCreation は one-shot
changes→TestExecution で戻ると再び PrCreation に達し `gh pr create` が重複/失敗する。
- **v0 は CodeReview を「判定提示して停止」に確定**し、changes フィードバック辺を v0 スコープから外す（冪等性問題を回避）。
- changes ループ＋idempotent publish（`ctx.pr_number/pr_url` を持ち create-or-update）は後段。

### 4. PrCreation に publish gate
push + `gh pr create` は最初の不可逆な外部操作（CLAUDE.md outward-facing 確認原則・着手前ゲート）。
- **default は dry-run**、`--publish`/`--yes` を必須に。
- publish 前に diff・branch・remote・PR title/body・実行 argv をログして gate。`gh pr create` 前に既存 PR 検出。

## 実装中に織り込む（Important / Minor）

- **`claude -p` の実編集能力を検証**: 既存テストは fake のみ。headless で permission/sandbox に止まらず cwd にファイルを書けるか、`sherpa doctor` か opt-in integration test（temp repo に小ファイル生成）で確認。Implementation 前に `claude` 存在・cwd writable を fail loud。
- **観察ログを ADR-005 準拠に**: JSONL で `stage / input(要約) / output(要約) / outcome(success|failure|partial) / error / duration_ms / timestamp / attempt` ＋ raw verdict text・parsed verdict・artifact path/hash・retry edge・redacted argv。秘密値は本文に書かない。
- **retry カウンタは edge 単位**で保持し、ログに edge と attempt を出す。
- TestExecution 決め打ち（コマンド注入可能に。統合テストで実 cargo を再帰起動しないため定数デフォルト＋fake 注入）:
  ```
  cargo fmt --all --check
  cargo clippy --all-targets -- -D warnings
  cargo test --all
  ```
  最初の失敗で `ctx.last_error` に積んで Implementation へ差し戻す。

## エージェント A/B 分離（design.md §2.3）

実装と plan/code レビューは別プロンプト（plan.md/implement.md=A、plan-review.md/code-review.md=B）。v0 はプロンプト分離で担保。モデル分離（`run_agent` への model 引数）は後段。

## 後段送りで可

汎用 smoke test 設定化 / model（A/B）分離 / Git proxy pattern / 観察ログの DB・graph 化 / 複数 Issue 並列 / CodeReview changes ループ＋idempotent publish。

## 実装順序

1. ドライバループ ＋ `ReviewVerdict` schema/parser（blocker 1）
2. worktree 隔離 ＋ 決定論ステージ（IssueFetch / BranchCreation / TestExecution）
3. エージェントステージ（PlanCreation / PlanReview / Implementation / CodeReview）
4. プロンプト3種（plan / plan-review / code-review）
5. 統合テスト（fake `gh`/`git`/`claude` の PATH shim 拡張）
6. PrCreation の publish gate ＋ dry-run
7. `main.rs` の bail 撤去・実パイプライン起動

## Done 判定基準

- [ ] `cargo build` / `cargo test --all` / `cargo clippy --all-targets -- -D warnings` / `cargo fmt --all --check` 全グリーン
- [ ] fake `gh`/`git`/`claude` で happy path 1本 ＋ PlanReview reject・Test fail のリトライ→escalate を統合テスト
- [ ] ReviewVerdict が曖昧文で fail loud するテスト
- [ ] 新規プロンプト3種を追加、verdict 契約が機械可読
- [ ] 観察ログが ADR-005 フィールド＋raw/parsed verdict で JSONL 出力
- [ ] PrCreation が dry-run default、`--publish` 必須、既存 PR 検出
- [ ] `main.rs` の bail 撤去・実パイプライン起動
- [ ] `git diff` 確認・シークレット未混入

## レビュー方針

- プラン再レビューは不要（改訂は Codex blocker の直接反映）。
- 実装完了後の **pre-PR gate は Codex `$pr-review`**（ターミナルからユーザーが起動、Claude 内から自走させない）。draft PR を先に作っておくと specialist が同じ base ref に収束する。

## 実装メモ

### 2026-06-26 walking skeleton 実装

- 設計判断: `ReviewVerdict` は JSON 契約と `VERDICT: ...` 先頭行契約の両方を受けるが、自由文 substring 判定はしない。欠落・複数 verdict・未知値は fail loud。
- 設計判断: retry は `plan_review->plan_creation` と `test_execution->implementation` の edge 単位で数え、3回目で escalation とする。
- 設計判断: worktree は repo root の兄弟 `.sherpa-worktrees/issue-<n>` に作る。v0 では CodeReview 後の確認・dry-run inspection を優先して自動削除しない。
- 逸脱: Step 5 の end-to-end fake integration test を成立させるため、PrCreation の dry-run 最小実装だけ Step 6 より先に入れた。push / `gh pr create` / 既存 PR 検出の本実装は Step 6 で追加した。
- トレードオフ: 観察ログの `timestamp` は追加クレートを避けるため RFC3339 ではなく `unix_ms:<millis>` 文字列にした。ADR-005 の必須フィールドと raw/parsed verdict は JSONL に出力する。
- トレードオフ: CLI の prompt directory は repo root の `docs/prompts` 固定にした。配布後に prompts をバイナリへ埋め込むか設定化する余地は残す。

### 2026-06-26 review follow-up

- 設計判断: デフォルト観察ログは repo root 直下ではなく、repo の兄弟 `.sherpa-worktrees/observations.jsonl` に置く。dry-run 実行だけで main worktree が dirty になり、BranchCreation の clean check と衝突するため。
- 設計判断: BranchCreation で `base_ref` を immutable な `base_commit` に解決し、worktree 作成と CodeReview diff の両方で同じ commit を使う。publish 後でも `git diff --no-ext-diff <base_commit>` が最終差分を表すようにするため。
- 設計判断: CodeReview には stat ではなく full diff を渡し、diff 取得失敗や空 diff は fail loud にした。レビュー agent が空/要約差分を approve してしまう false-green を避ける。
- 設計判断: text verdict は「最初の非空行が `VERDICT: ...`」であることを必須にした。プロンプト契約と parser のズレをなくし、先頭以外の verdict 混入を拒否する。
- 設計判断: `gh pr list` が要素を返したのに `url` が欠ける場合は `None` ではなく fail loud にする。既存 PR 検出の異常を新規 PR 作成で覆い隠さないため。
- 設計判断: `PipelineOptions` は空の `test_commands` と空コマンド、`max_retries = 0` を実行前に拒否する。silent false-green を避けるため。
- テスト判断: fake `gh`/`git`/`claude` に call log を追加し、dry-run で push/create しないこと、publish で add/commit/push/create すること、CodeReview が full diff を受け取ることを統合テストで固定した。
