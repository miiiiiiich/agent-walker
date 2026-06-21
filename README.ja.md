# agent-walker

[![CI](https://github.com/miiiiiiich/agent-walker/actions/workflows/ci.yml/badge.svg)](https://github.com/miiiiiiich/agent-walker/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#ライセンス)
[![MSRV](https://img.shields.io/badge/rustc-1.93+-blue.svg)](https://www.rust-lang.org)
[![No telemetry](https://img.shields.io/badge/telemetry-none-brightgreen.svg)](#プライバシー)

**English: [README.md](README.md)**

**自分の AI エージェントが、どこを歩いたか見えてるか。**

AI エージェントがマシンに残すログから作る、ローカルのターミナル・ダッシュボード。消費トークン、API 換算コスト、プロジェクト、ツール、自律度 — 1ヶ月ぶんの稼働を1画面に。使い込むと codename がもらえる。

*抑止できるのは、見ている者だけだ。*

![agent-walker のプレビュー](docs/demo.gif)

## なぜ

サブスク型の AI ツールは、自分が実際どれだけ使っているかを教えてくれない。ディスク上のセッションログを読んで、1画面でこれに答える：

- **API なら、いくらかかってた？** — キャッシュを考慮したコスト窓（今日 / 7日 / 30日 / 期間）。サブスクの元が取れてるか分かる。
- **トークンはどこへ消えた？** — エージェント横断のリポジトリ別内訳。
- **いつ・どのモデルで？** — GitHub 風アクティビティ、モデル別の積み上げ棒、時間帯プロファイル（ローカル時刻）。
- **放置で回せてる？** — 20分超の自律稼働を重視したターン時間ヒストグラム。

## プライバシー

- ログは**マシンの外に出ない**。テレメトリも分析もなし。
- 唯一の通信は、公開モデル価格メタデータの取得（[LiteLLM 価格DB](https://github.com/BerriAI/litellm)・MIT ライセンス）。ログ内容も利用状況も一切送らない。
- リリースバイナリは [GitHub Artifact Attestations](https://docs.github.com/en/actions/concepts/security/artifact-attestations) 付き：

```sh
gh attestation verify agent-walker-aarch64-apple-darwin.tar.xz -R miiiiiiich/agent-walker
```

## 使う

インストール不要 — `npx` / `bunx` が環境に合ったバイナリを取ってきて実行：

```sh
npx agent-walker
# または
bunx agent-walker
```

| キー | 動作 |
|---|---|
| `←` `→` / `h` `l` / `Tab` | プロバイダ切替（API換算コストの多い順・Combined 最後） |
| `↑` `↓` / `j` `k` / `PgUp` `PgDn` | スクロール |
| `1`–`4` | タブへジャンプ |
| `r` | リロード |
| `s` | カードを共有（画像コピー / `~/Downloads` に保存） |
| `q` / `Esc` / `Ctrl-C` | 終了 |

フラグ：

| フラグ | 何をする |
|---|---|
| `--days <N>` | グラフの集計期間（既定 30。codename は常に直近 30 日） |
| `--share <path>` | カードを PNG 出力＋キャプション表示して終了 |
| `--agy` | Antigravity も読む（既定オフ） |
| `--no-cache` | パースキャッシュを無視して全再走査 |
| `--claude-dir` / `--codex-dir` / `--agy-dir` | 非標準のログ場所を指定 |
| `--completions <shell>` | シェル補完を出力して終了 |

## codename を共有

ダッシュボードで `s`（または `agent-walker --share card.png`）を押すと共有カードを描く。トークン数を主役にせず、*どう* 使っているか — アクティビティ、時間帯のリズム、モデル内訳、並列度とタスク時間 — を見せる。

使い込むと **codename**（昇っていくランク）がもらえる。どう決まるかは伏せてある。リポジトリ名はカードに出ないので、一目だけ確かめて投稿を。

![agent-walker の codename カード](docs/card.png)

## 読むもの

| エージェント | 場所 | メモ |
|---|---|---|
| Claude Code | `~/.claude/projects/**/*.jsonl` | トークン / モデル / ツール / サブエージェント / プロジェクト / ターン時間 — [詳細](docs/claude.md) |
| Codex CLI | `~/.codex/sessions/**/*.jsonl` | トークン / モデル / ツール / タスク時間 / プロジェクト — [詳細](docs/codex.md) |
| Antigravity CLI | `~/.gemini/antigravity-cli` | `--agy` で opt-in。セッションとツールの流れのみ（ログにトークンなし） — [詳細](docs/agy.md) |

各エージェントはトークンとキャッシュの数え方が違う。上の per-agent ページに、何を読むか・トークン/キャッシュ/コストの数え方・注意点を書いてある（英語）。

並列パース＋ファイル単位キャッシュ（`~/.cache/agent-walker/`）で warm start は ~100ms。ログは信頼しない入力として扱い、壊れた行は数えてスキップ（評価はしない）。

### 30日より前も残すには

Claude Code は既定で**約30日でトランスクリプトを整理する**ことがある（`cleanupPeriodDays` 次第）。最初はこの設定次第で直近1ヶ月しか出ないこともある。1年残すなら `~/.claude/settings.json` に：

```json
{ "cleanupPeriodDays": 365 }
```

整理はセッション開始時に走る＝すでに消えたものは戻らないので早めに。Codex はセッションを保持（設定不要）。

### 注意・制約

- コストは **API 換算の見積もり**（実請求ではない）。LiteLLM の価格、キャッシュ読み書きは別レート。日付は COST パネルに表示。
- トークンは**キャッシュ込み**（入力 + 出力 + キャッシュ書込 + 読込）。Claude は毎ターン全コンテキストを再送するので、合計の約95%はキャッシュ読込＝安価な再読込で、新規の作業量とは別物。
- **Antigravity はトークン/コスト非計上**（中身不明な protobuf）。`--agy` 時も Combined の合計トークンには加算されない（活動のみ反映）。
- 各エージェント自身の利用表示とは一致しない（集計の窓やキャッシュの数え方が違う）。
- Antigravity のログのタイムスタンプはタイムゾーンなし＝ローカル想定。
- Windows 未対応（ログ探索が `$HOME` 依存）。

## ライセンス

MIT または Apache-2.0、好きな方で。
