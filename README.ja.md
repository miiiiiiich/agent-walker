# agent-walker

[![npm](https://img.shields.io/npm/v/agent-walker)](https://www.npmjs.com/package/agent-walker)
[![downloads](https://img.shields.io/npm/dt/agent-walker)](https://tanstack.com/stats/npm?packageGroups=%5B%7B%22packages%22%3A%5B%7B%22name%22%3A%22agent-walker%22%7D%5D%7D%5D&range=30-days)
[![CI](https://github.com/miiiiiiich/agent-walker/actions/workflows/ci.yml/badge.svg)](https://github.com/miiiiiiich/agent-walker/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#ライセンス)

**English: [README.md](README.md)**

![agent-walker のダッシュボード](docs/demo.gif)

### トークン量だけでは分からない、AI の使い方を知りたくないですか？

agent-walker は Claude Code や Codex CLI がローカルに残しているログから、あらゆる指標を計算し可視化するツールです。
知り合いとあなたのを見比べたり、SNSでシェアして、楽しんでください！ #agent-walker

```sh
bunx agent-walker
# npx agent-walker
```

## 何が分かるか

各エージェントの status や usage コマンド以上の情報を確認できます。

すべてのセクションが同じ期間、直近 30 日を見ています。

| 知りたいこと | セクション |
|---|---|
| よく使う日はいつで、どの時間帯に活発に使っているか | ACTIVITY / BY HOUR |
| 日々の消費がどのモデルに割れているか | TOKENS PER DAY |
| どのプロジェクトで、どのモデルと設定で消費しているか | PROJECTS / MODELS / MODES |
| どのツールとサブエージェントが仕事をしているか | TOOLS / SUBAGENTS |
| どのスキルにトークンが流れたか（Claude） | SKILLS |
| 同時に何本のエージェントを走らせているか | PARALLEL AGENTS |
| エージェントにどのくらいの時間、仕事をさせられているか | TURN LENGTH |
| エージェントが実際に働いた時間と、自分が返すまでの速さ | WORKING TIME |
| 無駄なコンテキストを消費していないか | CONTEXT |
| API 換算だといくらか | COST |
| プランの上限にどこまで近づいたか（Codex） | LIMITS |
| AI クレジットをどれだけ使ったか（Copilot） | CREDITS |
| よく使うモデル・最も使った日・ピーク時間・最長セッション・連続日数 | SIGNAL |

## share

`s` を押すと、SNS で共有できる画像がクリップボードに入ります。プロジェクト名などのプライベートな情報は載りません。

トークン使用量に応じてランクと動物が変わり、よく使っている時間帯に応じて色が変わります。

![agent-walker の codename カード](docs/card.png)

## 対応エージェント

Claude Code / Codex CLI / OpenCode / Cursor / GitHub Copilot CLI / Grok Build / Antigravity。全部自動検出です。

エージェントごとに、何をどう読んでいるかのドキュメントがあります: [Claude Code](docs/claude.md) / [Codex](docs/codex.md) / [OpenCode](docs/opencode.md) / [Cursor](docs/cursor.md) / [Copilot](docs/copilot.md) / [Grok](docs/grok.md) / [Antigravity](docs/agy.md)

## プライバシー

agent-walker はログをどこのサーバーにも送りません。ネットワークを使うのは、料金表の取得（取れたらその日は再利用）と、Cursor にサインインしているときの利用状況の取得だけです。

## 操作

| キー | 動作 |
|---|---|
| 数字 | タブ移動 |
| `r` | リロード |
| `s` | share 画像をコピー |
| `q` / `Esc` / `Ctrl-C` | 終了 |

<details>
<summary>コマンド引数</summary>

| 引数 | 動作 |
|---|---|
| `--share <path>` | share 画像を PNG に書き出して終了 |
| `--json` | 集計と日時付きイベントを JSON に出力（試験的） |
| `--days <N>`（`--json` のみ） | 集計日数（既定 30）。TUI は 30 日固定 |
| `--no-cache` | ログを全部読み直す |

</details>

## JSON

`--json` は TUI と同じ集計と、その元になった日時付きイベントを 1 つの JSON に出力します。試験的な機能で、スキーマはマイナーリリースでも変わることがあります。

```sh
agent-walker --json | jq '{schema_version, window, total}'
```

## 注意

コストは API 換算の見積もりです。料金は [LiteLLM](https://github.com/BerriAI/litellm) の価格表を使っています。

## 謝辞

[ccusage](https://github.com/ccusage/ccusage) は AI コーディングエージェントのローカル利用量トラッキングの先駆者で、agent-walker が扱う問題のいくつかを最初に特定・解決しました。[THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES) も参照してください。

## ライセンス

MIT または Apache-2.0、好きな方で。
