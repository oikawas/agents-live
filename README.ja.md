# agents-live

[English](README.md) | 日本語

Claude Code / CodexをCLIベースで使う人のためのツールです。
ローカルで動いている **Codex** と **Claude Code** のセッションと、それらが起動したサブエージェントの状態を、ターミナルに表示し続ける読み取り専用のダッシュボードです。
Childの状況や、Weekly limitを常時簡単に把握することが目的です。SSDへの負担を考慮して、ディスクへの書き込みは最小になるようにしています。

```text
AGENTS LIVE  09:22:11  Codex🔄: 2d14h03m  Claude🔄: 0d05h41m
────────────────────────────────────────────────────────────────────────────────────────────────
CODEX  Weekly 86%  8.2B $4,321   2 running · 1 subagents · 1 processes
╭─ CODEX  PID 10261  my-project  01a06f14  2 running
│ ● RUN   ◆    MAIN     exec      2m37s     gpt-6-sol medium  12.4M tok  Refactor the parser
│ ● RUN   └─   Herschel exec      1m40s     gpt-6-sol xhigh   3.1M tok   Review the new tests
╰───────────────────────────────────────────────────────────────────────────────────────────────
CLAUDE  Weekly 94%  5h 88%  1.2B $512   1 running · 1 subagents · 1 processes
╭─ CLAUDE  PID 55116  web-app  1c564b8e  2 running
│ ● RUN   ◆    MAIN     Bash      4m12s     claude-opus-5-5 high  4.2M tok  Add a login form
│ ● RUN   └─   bash     running   3m05s     --              --    -- tok    Run the test suite
╰───────────────────────────────────────────────────────────────────────────────────────────────
q / Esc / Ctrl-C to stop
```

## 特徴

- **Codex と Claude Code を1画面に表示**します。プロセス（PID）ごとにまとめ、メインのスレッドの下にサブエージェントを子行として並べます。Claude Code / Codexどちらか一方だけを表示することもできます。
- **サブエージェントとバックグラウンドの作業も追跡**します。Claude Code では、`Agent` / `Task` ツールのサブエージェントを追跡します。`run_in_background` で起動したり、時間切れでバックグラウンドへ移されたりした `Bash` コマンドも、完了するまで `RUN` と表示します。
- **週次制限と5時間制限の残り**、**リセットからの経過時間**を表示します。
- **直近30日のトークン数と、API料金に換算した概算コスト**を表示します。あくまでお遊びの参考値として見てください。
- **読み取り専用**です。Codex や Claude Code の状態を変更せず、ネットワーク接続もしません。安全かつプライバシーを保って利用できます。

## 動作環境

- macOS（x86） で開発・確認しています。他の Unix 系 OS は未検証で、Windows には対応していません。
- Rust（stable）のツールチェーンが必要です。依存するクレートは、ビルド時に `cargo` が自動で取得します。
- C コンパイラが必要です。SQLite をソースから組み込んでビルドするためで、macOS では Xcode Command Line Tools（`xcode-select --install`）で入ります。
- Codex では `lsof` コマンドを使います。

## インストール

```sh
git clone https://github.com/oikawas/agents-live.git
cd agents-live
cargo install --path rust
```

`~/.cargo/bin/agents-live` にインストールされます。

## 使い方

```sh
agents-live                        # Codex と Claude Code の両方を表示
agents-live --claude off           # Codex だけを表示
agents-live --codex off            # Claude Code だけを表示
agents-live --once --color never   # 1回だけ表示して終了（診断用）
```

`q`、`Esc`、`Ctrl-C` のいずれかで終了します（Enter は不要です）。ターミナルの高さが足りないときは、ヘッダーを残したまま、下の行を `… N more rows omitted` に置き換えます。

## 画面の見方

### 最上部の行

時刻と、由来（Codex / Claude）ごとの週次制限のリセットからの経過時間（`DdHHhMMm`）を表示します。リセット時刻がまだ分からない由来は `-d--h--m` と表示します。並び順は `--group-order` のグループ順と同じです。この行は、どのテーマでも白で表示します。

### 由来ごとの集計行

各由来の最初の PID グループの上に、次の形の行を1回だけ表示します。

```text
CODEX   Weekly 86%  8.2B $4,321   2 running · 1 subagents · 1 processes
CLAUDE  Weekly 94%  5h 88%  1.2B $512   1 running · 1 subagents · 1 processes
```

| 項目 | 意味 |
|---|---|
| `Weekly nn%` | 週次制限の残り |
| `5h nn%` | 5時間制限の残り。Claude では常に表示します。Codex では、5時間の枠が報告されたときだけ表示します |
| `8.2B $4,321` | 直近30日のトークン数と概算コスト（後述） |
| `running` / `subagents` / `processes` | 実行中の行数、サブエージェント数、プロセス数 |

Codex と Claude の数値は、それぞれ別に集計します。互いに合算はしません。

週次制限の残りが95%以上になると、リセットを知らせるために集計行を緑で表示します。5%未満になると、数値だけを赤で強調します。

### スレッドの行

各行には、状態、名前、現在の活動、経過時間、モデル、reasoning effort、累積トークン数、作業名を表示します。取得できない値は `--` と表示します。

| 記号 | 状態 |
|---|---|
| `●` RUN | 実行中 |
| `◌` IDLE | メインが次の入力を待っている |
| `✓` DONE | サブエージェントが完了した |
| `◐` START | 初期化中 |
| `!` STOP | 中断または停止した |

- `◆` はメインのスレッド、`└─` はサブエージェントを表します。
- カラー表示では、`RUN` の行を画面幅いっぱいに反転表示します。
- 高速モードのセッションは、モデル名の前に `⚡`（`fast` / `priority`）または `⚡⚡`（`ultrafast`）を付けます。
- reasoning effort のうち、`high` は水色、`xhigh` は黄色、`ultra` は赤の背景で表示します。
- Claude Code のセッションは、モデルは待機中でもバックグラウンドのシェルが生きているとき、`IDLE` のまま活動欄に `bg shell` と表示します。
- Claude の行は暖色系で、Codex の行は別の寒色系の配色で表示します。
- Codex の `/rename` で付けた名前は、次の更新で反映されます。

### 全セッション待機バナー

表示中のセッションがすべて待機中で、サブエージェントもすべて完了すると、最上部の行が画面幅いっぱいのバナー（`████  ALL AGENTS IDLE  ████`）になります。片方の由来だけを表示しているときは `ALL CODEX SESSIONS IDLE` または `ALL CLAUDE SESSIONS IDLE` と表示します。オプションで on / off可能です。

## オプション

| オプション | 説明 | 既定値 |
|---|---|---|
| `--codex on\|off` | Codex を表示するか | `on` |
| `--claude on\|off` | Claude Code を表示するか | `on` |
| `--codex-home PATH` | Codex のデータディレクトリ | `~/.codex` |
| `--claude-home PATH` | Claude Code のデータディレクトリ | `~/.claude` |
| `--once` | 1回だけ表示して終了する | |
| `--interval SEC` | 更新間隔（秒） | `0.7` |
| `--process PID` | 指定した Codex の PID だけを表示する | |
| `--hide-done` | 完了したサブエージェントを隠す（別名 `--hide-done-children`） | |
| `--show-guardians` | Codex 内部の guardian スレッドも表示する | |
| `--group-order codex-first\|claude-first` | どちらの由来のグループを上に置くか | `codex-first` |
| `--group-sort created\|session-name` | 各由来の中での PID グループの並び順 | `created` |
| `--compact-groups on\|off` | グループの間の空行を省くか | `on` |
| `--tokens on\|off` | 行ごとの累積トークン数を表示するか | `on` |
| `--no-wrap` | 幅が狭いときも、モデルなどの折り返し行を出さない | |
| `--theme cool\|codex\|cyberpunk` | Codex の行の配色 | `cool` |
| `--color auto\|always\|never` | ANSI カラーを使うか | `auto` |
| `--weekly-reverse` | `Weekly nn%` の数値を反転表示する | |
| `--all-idle-banner on\|off` | 全セッション待機バナーを表示するか | `on` |
| `--all-idle-bgcolor COLOR` | バナーの背景色（black / red / green / yellow / blue / magenta / cyan / white） | `blue` |
| `--all-idle-red-screen` | 全セッション待機時に、画面全体をバナーの色で塗る | |

## Claude の週次制限と5時間制限の表示（任意）

Claude Code は、週次制限と5時間制限の残りをローカルのファイルに書き出しません。ステータスライン用のコマンドに、描画のたびに標準入力で渡すだけです。そのため、何もしなければ Claude の値は `Weekly --`、`5h --` と表示されます。それ以外の機能は、この設定をしなくても同じように動きます。

表示したい場合は、次を一度だけ実行します。

```sh
agents-live install-statusline
```

このコマンドは `~/.claude/settings.json` の `statusLine` を書き換え、先に `agents-live statusline` を実行するようにします。`agents-live statusline` は制限の値をセッションごとに `~/.claude/agents-live-statusline-cache/` へ保存し、同じ入力を元のステータスラインのコマンドへそのまま渡します。そのため、Claude Code のステータスラインの見た目は変わりません。

- 書き換える前に、元の `settings.json` を同じ場所へバックアップします（`settings.json.agents-live-statusline-backup-<日時>`）。
- 何度実行しても結果は同じです。`agents-live` を再起動する必要もありません。
- 設定には、実行した `agents-live` の場所を書き込みます。別の場所にインストールし直したら、もう一度実行してください。
- `agents-live install-statusline --uninstall` で、元のステータスラインの設定に戻せます。

このツールで設定ファイルを書き換えるのは、この任意の手順だけです。自動で実行されることはありません。

## 30日間のトークン数と概算コスト

集計行のトークン数と金額は、直近30日のローカルのセッション記録から計算した、**API料金に換算した概算**です。Codex や Claude Code のサブスクリプションの請求額ではありません。

- Codex は、OpenAI の標準API料金（input、cached input、cache write、output、長文脈の割増）で計算します。
- Claude Code は、Anthropic の標準API料金（input、モデル別の cache read、cache write、output）で計算します。サブエージェントのトランスクリプトも含めます。
- ツール呼び出しの料金と、サービスティアによる料金差は含めません。
- 単価表に無いモデルのトークンは、トークン数と金額の両方から除きます。そうしたトークンが直近30日の1%以上を占めると、金額の後ろに `+?` を付けます（例：`$1,838+?`）。新しいモデルが出たときは、単価表（`rust/src/model.rs`）への追加が必要です。
- 起動直後は、最初の集計が終わるまで `--B $--` と表示します。`--once` は集計の完了を待ってから表示します。

## 仕組み

`agents-live` はローカルのファイルを読むだけで、Codex と Claude Code の状態は変更しません。

- **Codex**：`lsof` で実行中の Codex プロセスが保持しているロックを調べます。スレッドの情報は `~/.codex/state_5.sqlite` から読みます。各スレッドのロールアウト（JSONL）は、追記された分だけを読みます。
- **Claude Code**：`~/.claude/sessions/<pid>.json` から、プロセスが生きているセッションとその状態を読みます。`busy` だけを `RUN` とします。モデル、トークン数、サブエージェントの情報は、`~/.claude/projects/` のトランスクリプト（JSONL）から、追記された分だけを読みます。
- 30日間の集計は、バックグラウンドで最大60秒に1回更新します。前回からサイズか inode が変わったファイルだけを読み直します。集計結果はメモリにだけ保持し、ディスクには書きません。

これらは Codex と Claude Code の内部のファイル形式に依存しています。どちらかの更新で形式が変わると、表示が崩れたり、値が取れなくなったりすることがあります。

## 開発

```sh
cd rust
cargo test                 # テスト
cargo run -- --once        # 1回だけ表示
```

README は日本語版（このファイル）が正本で、英語版の [README.md](README.md) はその翻訳です。

## 注意

このツールは非公式のものです。OpenAI とも Anthropic とも関係ありません。Codex は OpenAI の、Claude と Claude Code は Anthropic の製品です。

## ライセンス

[MIT](LICENSE)
