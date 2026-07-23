# Changelog

All notable changes to Cordy are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org). Release notes are generated from Conventional Commits.

## [Unreleased]

Initial public release preparation: OpenCode-inspired TUI, hot-swap models and providers across
all four API families, builtin toolset with a native output optimizer, background jobs, MCP,
skills, sub-agents, autonomous goals, session persistence, permission rules, eight themes,
and config hot-reload.

## [0.1.7]

### Added
- **Session goals — a real autonomous loop.** `/goal <objective>` sets an objective the session
  keeps working on by itself: every finished turn feeds the next one until the work is done, the
  budget runs out, or the agent is genuinely stuck. Six statuses (`active`, `paused`, `blocked`,
  `usage limited`, `limited by budget`, `complete`), and the model gets `get_goal`, `create_goal`
  and `update_goal` — it may declare a goal complete or blocked, while pausing, resuming and
  raising budgets stay with you.
  - Usage is charged **as each tool finishes**, not at the end of a turn, so a budget can stop a
    run mid-turn; at that moment the model is told to wrap up instead of starting new work.
  - Completion is audited: the agent must prove each requirement against the current worktree
    before marking a goal complete, and may only report `blocked` after the same obstacle recurs
    for three consecutive turns.
  - Budgets: `--budget <tokens>`, `--cost <usd>`, `--turns <n>` (defaults under `[goal]` in
    config). Whichever is reached first ends the run.
  - `/goal` alone shows status; `edit`, `pause`, `resume`, `clear` do what they say. A status chip
    in the footer tracks objective, tokens and elapsed time. The goal is stored beside the session
    log, survives `--resume`, and is inherited by a fork — a restored goal waits for you before
    spending anything.
- **`apply_patch`** — a new builtin that applies a multi-file, multi-hunk patch (add, update,
  rename, delete) as one reviewed unit. Context is matched with graduated tolerance — exact, then
  ignoring trailing whitespace, then all whitespace, then normalizing typographic punctuation — so
  a patch whose context drifted slightly still lands. The patch is fully resolved before anything
  is written: the diff you approve is exactly what is applied, and a patch that doesn't fit leaves
  the worktree untouched. `edit` remains the right tool for a single unique replacement.

### Changed
- **`/ralph` is gone**, replaced by `/goal` (the old name still works as an alias for
  `/goal resume`). The ralph loop discarded its context every iteration and tracked progress in
  `.cordy/progress.md`; goals keep the conversation, lean on auto-compaction, and treat the
  worktree — not a notes file — as the record of what has been done. `.cordy/progress.md` and
  `.cordy/goal.md` are no longer written.

### Fixed
- Adjacent same-role messages are merged for the Anthropic Messages API, which requires strictly
  alternating roles.

## [0.1.6]

### Added
- **Text-encoded tool calls are recovered** — models that emit tool calls as plain text instead of
  the structured `tool_calls` field (Gemma, Qwen/Hermes-style, and OpenAI-compatible endpoints that
  render calls via their chat template) now work. Recognized formats: `<tool_call>{json}</tool_call>`
  (and the `<|tool_call> … <tool_call|>` and `[TOOL_CALL]` variants), the `call:NAME{key:<|">v<|">}`
  DSL, and ```` ```tool_code ```` Python-style calls. Only runs when the provider returned no
  structured call, so native function-calling is untouched.

## [0.1.5]

### Added
- **Proactive background-job events** — when a background job finishes on its own (a build
  completes, a dev server crashes, a watched condition is met), the agent is notified and can act
  on it automatically, even if you haven't typed anything.

### Fixed
- **Tool calls display live** — a tool now appears in the transcript the moment it starts running
  (with a one-line arg preview and a "running…" marker) and fills in its output when done, instead
  of only showing up after it finishes.
- **Interrupt aborts a hung tool** — Esc now cancels a stuck tool call immediately (dropping it
  also kills any spawned child process via kill-on-drop); the turn ends with the tool marked
  interrupted and the conversation left in a valid state.

## [0.1.4]

### Fixed
- **Terminal-native paste of large text no longer submits** — a paste via ctrl+shift+v / ctrl+v /
  right-click (which the terminal delivers as a key stream, not a paste event) is now detected even
  when the characters trickle a couple milliseconds apart, so it coalesces into a single paste
  instead of leaking through one key at a time and letting an embedded newline fire the message.
  (alt+v, which reads the clipboard directly, already worked.)

## [0.1.3]

Paste robustness and a message action menu.

### Fixed
- **Large / chunked pastes never auto-submit** — the input reader now stays in a self-extending
  "paste window" for the whole paste, so a newline in the middle (or a lone Enter a terminal
  delivers in a separate chunk, as Windows ConPTY does for big pastes) becomes a newline, never a
  submit. Previously a long paste could fire the message immediately.

### Changed
- **Clicking a message opens an action menu** instead of rewinding immediately — pick **Copy**,
  **Rewind & edit**, or **Delete from here** (navigable by mouse or ↑/↓ + Enter, Esc to close).
  Assistant/tool/system messages offer **Copy**.

## [0.1.2]

Follow-up input fixes.

### Fixed
- **Pasted newlines never submit, robustly** — post-paste grace window absorbs a trailing Enter
  delivered a few ms late (terminals that split a paste into chunks), plus a short coalescing
  window while a key-burst is forming.
- **Queue no longer spams turns** — messages typed while busy are combined into a single prompt and
  sent as one turn when the agent frees up.
- **Interrupt keeps the queue** — Esc cancels the running turn but no longer discards queued
  messages; they send once the turn unwinds.
- **Image paste reachable** — clipboard paste is bound to alt+v and ctrl+shift+v as well as ctrl+v,
  since terminals commonly intercept ctrl+v for their own text paste.

### Added
- **Click to rewind** — left-click a past user message to drop it and everything after (from the
  display and the live conversation) and reload its text into the input for editing/resending.

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

[Unreleased]: https://github.com/redstone-md/Cordy/compare/v0.1.6...HEAD
[0.1.6]: https://github.com/redstone-md/Cordy/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/redstone-md/Cordy/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/redstone-md/Cordy/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/redstone-md/Cordy/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/redstone-md/Cordy/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/redstone-md/Cordy/compare/v0.1.0...v0.1.1
