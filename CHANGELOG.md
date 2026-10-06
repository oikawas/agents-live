# Changelog

English | [日本語](CHANGELOG.ja.md)

User-facing changes, by version. Run `agents-live --version` to see which version you have.

## 0.2.3 (unreleased)

### Fixed

- A Claude subagent could stay RUN after it had finished. Claude Code occasionally records a subagent's completion notice without the ID this tool matched on; such notices now mark it DONE too.

## 0.2.2 (2026-10-03)

### Fixed

- Added the rate for Codex's new model `gpt-6.1-sol`. It had no rate, so its usage was missing from the 30-day token count and cost, and the cost carried a `+?`.
- Added the rate for Claude's new model `claude-sonnet-5-5` (Sonnet 5.5).
- Claude Code records Haiku 4.5 under its dated name (`claude-haiku-4-5-20251001`), so Haiku usage was missing from the 30-day token count and cost. Dated names now count as the same model.

## 0.2.1 (2026-09-27)

### Fixed

- In 0.2.0, the subagents a Codex session left behind after its main thread ended were each shown as a separate group. They are now shown together as one group.

### Known issues

- The Codex 0.157 daemon (`codex app-server`) sometimes keeps a session's main thread or subagents after its terminal has been closed. Until it lets go, the ended session keeps showing as `IDLE` or `DONE`. This is on the Codex side, so we are waiting for a Codex fix.

## 0.2.0 (2026-09-27)

### Changed

- Since Codex 0.157, a single daemon (`codex app-server`) runs every session. Separate sessions used to be shown together under one PID group; each session now gets its own group, with `app-server <PID>` in its header.

### Added

- `agents-live --version` (`-V`) prints the version.

### Known issues

- Since Codex 0.157, a session whose terminal has been closed keeps showing as `IDLE` for as long as the daemon holds its thread.

## 0.1.0 (2026-09-27)

- First public release.

The Japanese changelog ([CHANGELOG.ja.md](CHANGELOG.ja.md)) is the source document; this English version is a translation of it.
