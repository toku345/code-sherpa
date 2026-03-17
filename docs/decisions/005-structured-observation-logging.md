# ADR-005: パイプライン各ステージに構造化された観察ログ（Structured Observation）を設けること

- **ステータス:** Accepted
- **日付:** 2026-03-17
- **関連:** セクション 2.2（パイプラインフロー）, セクション 5（セルフホスティング計画）Phase 1

## コンテキスト

- code-sherpa のパイプラインは issue 検出 → plan → implementation → test → code review → merge の複数ステージで構成される
- 現時点（Phase 0）ではパイプラインの基本動作を作ることが最優先であり、自己改善ループの実装は時期尚早
- しかし、将来的にはパイプラインの失敗パターン分析や、指示ファイル（AGENTS.md等）の改善サイクルを回したい
- cognee-skills の「Self-improving skills for agents」（observe → inspect → amend → evaluate ループ）は Phase 1 以降の参考アーキテクチャとして有力
- 後から Observe レイヤーを足す際に、ログが print 文や非構造化テキストだと改修コストが大きくなる

## 決定

Phase 0 の段階から、パイプラインの各ステージで以下を構造化データとして出力する「観察の口」を設ける：

- ステージ名
- 入力（何を受け取ったか）
- 出力（何を生成したか）
- 成否（success / failure / partial）
- 失敗時のエラー情報
- 実行時間
- タイムスタンプ

**実装方針:**

- structlog 等の構造化ロガー、または各ステージの結果を dataclass / Pydantic model で返す形式を採用する
- Phase 0 では保存先は JSONL ファイルで十分。DB やグラフストアへの移行は Phase 1 以降で検討する
- 自己改善ループ（inspect / amend / evaluate）の実装は Phase 0 のスコープ外とする

## 根拠

- 「観察の仕込み」は設計コストが小さい割に、将来の拡張性へのレバレッジが高い
- cognee-skills の observe → inspect → amend → evaluate ループを将来導入する場合、構造化された実行記録が前提条件となる
- Phase 0 で入れない場合、後から全ステージのログ出力を改修するコストが発生する

## 将来の検討候補

### cognee-skills

Phase 1 以降で inspect / amend / evaluate ループの導入を検討する際の参考アーキテクチャ。スキルをグラフ構造で管理し、失敗パターンの探索・指示の自動改善・ロールバック可能な評価サイクルを提供する。

- https://github.com/topoteretes/cognee

### Entire

code-sherpa が内部で Claude Code 等のエージェントにコードを書かせるフェーズにおいて、エージェントの作業過程（プロンプト、レスポンス、変更ファイル）をコミットに紐づけて追跡する用途で再評価する。パイプラインステージの観察ログとはレイヤーが異なり、「なぜそのコードがそう書かれたか」のトレーサビリティに寄与する。Phase 0 のスコープ外だが、生成コードの品質追跡が課題になった段階で検討する。

- https://entire.io/
- https://github.com/entireio/cli

## 参考資料

- [cognee-skills: Self-improving skills for agents](https://github.com/topoteretes/cognee)
- [@tricalt の投稿（2026-03-13）](https://x.com/tricalt/status/2032179887277060476)
- [Entire CLI](https://github.com/entireio/cli)
