# agents-live

English | [日本語](README.ja.md)

A tool for people who use Claude Code and Codex from the command line.
A read-only terminal dashboard that continuously shows the state of the **Codex** and **Claude Code** sessions running on your machine, along with the subagents they start.
Its goal is to let you keep track of your subagents and weekly limits at a glance, at all times. To spare your SSD, it keeps disk writes to a minimum.

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

## Features

- **Codex and Claude Code on one screen.** Rows are grouped by session, with subagents listed as child rows under the main thread. You can also show just Claude Code or just Codex.
- **Tracks subagents and background work.** For Claude Code, this covers `Agent` / `Task` subagents and `Bash` commands started with `run_in_background` or moved to the background after a timeout; each stays `RUN` until it finishes.
- **Remaining weekly and 5-hour limits**, and the time elapsed since the weekly reset.
- **30-day token count and an API-equivalent cost estimate.** Treat it as a just-for-fun reference figure.
- **Read-only.** It never changes Codex or Claude Code state and never connects to the network, so it is safe to use and keeps your data private.

## Requirements

- Developed and tested on macOS (x86). Other Unix-like systems are untested; Windows is not supported.
- A Rust (stable) toolchain. `cargo` fetches the crates it depends on automatically at build time.
- A C compiler, because SQLite is built from source into the binary. On macOS, install the Xcode Command Line Tools (`xcode-select --install`).
- `lsof`, used for Codex.

## Installation

```sh
git clone https://github.com/oikawas/agents-live.git
cd agents-live
cargo install --path rust
```

This installs `~/.cargo/bin/agents-live`.

## Usage

```sh
agents-live                        # show both Codex and Claude Code
agents-live --claude off           # Codex only
agents-live --codex off            # Claude Code only
agents-live --once --color never   # print one snapshot and exit (for diagnostics)
```

Press `q`, `Esc`, or `Ctrl-C` to quit (no Enter needed). When the terminal is too short, the header stays visible and the rows at the bottom are replaced by `… N more rows omitted`.

## Reading the screen

### Top line

Shows the clock and, for each source (Codex / Claude), the time elapsed since its weekly limit last reset (`DdHHhMMm`). A source whose reset time is not known yet shows `-d--h--m`. The sources appear in the same order as the groups (`--group-order`). This line is white in every theme.

### Per-source summary line

Each source gets one line, above its first PID group:

```text
CODEX   Weekly 86%  8.2B $4,321   2 running · 1 subagents · 1 processes
CLAUDE  Weekly 94%  5h 88%  1.2B $512   1 running · 1 subagents · 1 processes
```

| Field | Meaning |
|---|---|
| `Weekly nn%` | Remaining weekly limit |
| `5h nn%` | Remaining 5-hour limit. Always shown for Claude; shown for Codex only when a 5-hour window is reported |
| `8.2B $4,321` | 30-day token count and estimated cost (see below) |
| `running` / `subagents` / `processes` | Running rows, subagents, and processes |

Codex and Claude figures are computed separately and never combined.

When the remaining weekly limit is 95% or more, the summary line turns green to signal that the limit has just reset. Below 5%, the figure alone is highlighted in red.

### Thread rows

Each row shows the state, name, current activity, elapsed time, model, reasoning effort, cumulative tokens, and task name. Values that are unavailable appear as `--`.

| Symbol | State |
|---|---|
| `●` RUN | Running |
| `◌` IDLE | Main thread waiting for input |
| `✓` DONE | Subagent finished |
| `◐` START | Starting up |
| `!` STOP | Interrupted or stopped |

- `◆` marks the main thread; `└─` marks a subagent.
- With color, `RUN` rows are shown in full-width reverse video.
- Fast sessions get `⚡` (`fast` / `priority`) or `⚡⚡` (`ultrafast`) before the model name.
- Reasoning effort `high` is shown on a light-blue background, `xhigh` on yellow, and `ultra` on red.
- A Claude Code session whose model is idle while a background shell is still alive stays `IDLE` and shows `bg shell` as its activity.
- Claude rows use a warm color palette, and Codex rows a separate cool one.
- A name set with Codex's `/rename` appears on the next refresh.

### All-idle banner

When every visible session is idle and every subagent has finished, the top line becomes a full-width banner (`████  ALL AGENTS IDLE  ████`). With only one source shown, it reads `ALL CODEX SESSIONS IDLE` or `ALL CLAUDE SESSIONS IDLE`. It can be turned on or off with an option.

## Options

| Option | Description | Default |
|---|---|---|
| `--codex on\|off` | Show Codex | `on` |
| `--claude on\|off` | Show Claude Code | `on` |
| `--codex-home PATH` | Codex data directory | `~/.codex` |
| `--claude-home PATH` | Claude Code data directory | `~/.claude` |
| `--once` | Print one snapshot and exit | |
| `--interval SEC` | Refresh interval in seconds | `0.7` |
| `--process PID` | Show only one Codex PID | |
| `--hide-done` | Hide finished subagents (alias `--hide-done-children`) | |
| `--show-guardians` | Include Codex's internal guardian threads | |
| `--group-order codex-first\|claude-first` | Which source's groups come first | `codex-first` |
| `--group-sort created\|session-name` | Order of PID groups within each source | `created` |
| `--compact-groups on\|off` | Omit the blank line between groups | `on` |
| `--tokens on\|off` | Show cumulative tokens on each row | `on` |
| `--no-wrap` | Never wrap model details onto a second row on narrow terminals | |
| `--theme cool\|codex\|cyberpunk` | Color theme for Codex rows | `cool` |
| `--color auto\|always\|never` | Use ANSI color | `auto` |
| `--weekly-reverse` | Show the `Weekly nn%` figure in reverse video | |
| `--all-idle-banner on\|off` | Show the all-idle banner | `on` |
| `--all-idle-bgcolor COLOR` | Banner background (black / red / green / yellow / blue / magenta / cyan / white) | `blue` |
| `--all-idle-red-screen` | When all sessions are idle, fill the screen with the banner color | |
| `--version`, `-V` | Print the version | |

## Claude weekly and 5-hour limits (optional)

Claude Code does not write its remaining weekly and 5-hour limits to any local file; it only passes them on stdin to the status-line command each time the status line is drawn. Without the setup below, Claude's figures read `Weekly --` and `5h --`. Everything else works the same either way.

To show them, run this once:

```sh
agents-live install-statusline
```

It edits the `statusLine` setting in `~/.claude/settings.json` so that `agents-live statusline` runs first. `agents-live statusline` saves the limit figures per session under `~/.claude/agents-live-statusline-cache/`, then passes the same input unchanged to your original status-line command, so the status line in Claude Code looks exactly as before.

- The original `settings.json` is backed up next to it first (`settings.json.agents-live-statusline-backup-<timestamp>`).
- Running it again is safe, and `agents-live` does not need a restart.
- The setting records where the `agents-live` you ran is installed. If you reinstall it somewhere else, run the command again.
- `agents-live install-statusline --uninstall` restores your original status-line setting.

This optional step is the only thing in this project that edits a configuration file. It never runs automatically.

## 30-day tokens and estimated cost

The token count and dollar figure on each summary line are an **API-equivalent estimate** computed from the last 30 days of local session records. They are not what your Codex or Claude Code subscription charges.

- Codex uses OpenAI's standard API rates (input, cached input, cache write, output, and the long-context surcharge).
- Claude Code uses Anthropic's standard API rates (input, per-model cache read, cache write, and output), including subagent transcripts.
- Tool-call fees and service-tier price differences are not included.
- Tokens from models missing from the pricing table are left out of both the token count and the cost. When such tokens make up 1% or more of the last 30 days, the cost gets a `+?` suffix (for example, `$1,838+?`). A newly released model needs an entry in the pricing table (`rust/src/model.rs`).
- Right after startup, the figures read `--B $--` until the first scan completes. `--once` waits for the scan before printing.

## How it works

`agents-live` only reads local files; it never changes Codex or Claude Code state.

- **Codex**: finds the locks held by running Codex processes with `lsof`, reads thread metadata from `~/.codex/state_5.sqlite`, and reads only the newly appended part of each thread's rollout (JSONL).
  - Since Codex 0.157, a single daemon (`codex app-server`) holds the locks of every session, so rows are split by session (root thread), with `app-server <PID>` in the header.
  - The Codex 0.157 daemon sometimes keeps a session's threads after its terminal has been closed. Until it lets go, the ended session keeps showing as `IDLE` or `DONE`. This is on the Codex side, and we are waiting for a Codex fix.
- **Claude Code**: reads live sessions and their status from `~/.claude/sessions/<pid>.json`, keeping only sessions whose process is alive. Only the `busy` status counts as `RUN`. Model, token, and subagent details come from the transcripts (JSONL) under `~/.claude/projects/`, again reading only what was appended.
- The 30-day totals are refreshed in the background at most once every 60 seconds, rereading only files whose size or inode changed. They are kept in memory only and never written to disk.

All of this depends on Codex's and Claude Code's internal file formats. An update to either tool can change those formats and break the display or leave some values unavailable.

## Development

```sh
cd rust
cargo test                 # tests
cargo run -- --once        # print one snapshot
```

The Japanese README ([README.ja.md](README.ja.md)) is the source document; this English README is a translation of it.

## Changelog

See [CHANGELOG.md](CHANGELOG.md).

## Disclaimer

This is an unofficial tool, not affiliated with OpenAI or Anthropic. Codex is a product of OpenAI; Claude and Claude Code are products of Anthropic.

## License

[MIT](LICENSE)
