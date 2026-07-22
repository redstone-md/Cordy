# Changelog

All notable changes to Cordy are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org). Release notes are generated from Conventional Commits.

## [Unreleased]

Initial public release preparation: OpenCode-inspired TUI, hot-swap models and providers across
all four API families, builtin toolset with a native output optimizer, background jobs, MCP,
skills, sub-agents, autonomous ralph-loop, session persistence, permission rules, eight themes,
and config hot-reload.

## [0.1.1]

Input, provider, and reliability fixes.

### Fixed
- **Esc no longer quits the app** — it only dismisses the autocomplete popup or interrupts a
  running turn. Quit is ctrl+c (empty input), ctrl+d, `<leader> q`, or `/quit`.
- **Autocomplete is navigable** — ↑/↓ move the highlight; Tab or Enter accept the selected item.
- **Interrupt is instant** — Esc aborts a turn immediately even while the request is still hanging
  before the first token, not only mid-stream.
- **Pasted newlines never auto-submit** — bracketed paste is normalized (CRLF/CR → newline) and,
  on terminals without bracketed paste, rapid key bursts are coalesced into a paste so an embedded
  Enter can't submit.
- **Non-Latin keyboard layouts work** — keybinds and the leader chord are matched by physical key
  position (e.g. Cyrillic ЙЦУКЕН → QWERTY); typing non-Latin text is unaffected.
- **Wrapped-line navigation** — ↑/↓ move between visual rows of a long/multiline draft instead of
  jumping to history; stepping back down past the newest history entry restores the draft.

### Added
- **Message queue** — messages typed while the agent is busy are queued (shown above the prompt)
  and sent in order once the turn completes; interrupt clears the queue.
- **Clipboard image paste** — ctrl+v attaches a clipboard image (saved as PNG, sent via the vision
  pipeline), or pastes clipboard text.
- **Large-paste collapse** — a paste of 300+ characters shows as `[pasted N chars]` in the input
  and is expanded to the full text on submit.
- **All providers' models load up-front** — the picker lists models for every configured provider
  concurrently at startup, not only after switching to one.
- **Update notification** — on startup Cordy checks crates.io (GitHub releases fallback) and, if a
  newer version is out, shows a notice and a footer badge.

[Unreleased]: https://github.com/redstone-md/Cordy/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/redstone-md/Cordy/compare/v0.1.0...v0.1.1
