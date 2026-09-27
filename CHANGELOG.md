# Changelog

English | [日本語](CHANGELOG.ja.md)

User-facing changes, by version. Run `agents-live --version` to see which version you have.

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
