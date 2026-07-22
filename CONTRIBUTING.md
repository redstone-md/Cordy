# Contributing to Cordy

Thanks for your interest. Cordy is a single Rust binary crate; the internals are documented in
module headers and `src/main.rs`.

## Development

```sh
cargo build              # default build
cargo build --features mcp   # with the MCP client
cargo test
cargo clippy --all-targets
cargo fmt
```

All four must pass before a change is merged. The test suite uses golden SSE fixtures per provider
(`tests/fixtures/`), so no API key is required to run it.

## Commit messages

Commits follow [Conventional Commits](https://www.conventionalcommits.org):

```
feat(tui): add background-job status to the side panel
fix(provider): surface the 4xx response body instead of a bare status
docs: document the /providers command
```

Types: `feat`, `fix`, `docs`, `refactor`, `perf`, `test`, `chore`, `ci`. The release CHANGELOG is
generated from these, so a clear type and scope matters.

## Pull requests

- One focused change per PR; keep diffs minimal and match the surrounding style.
- Add or update tests for behavior changes.
- Describe what changed and why. Screenshots help for UI changes.

## Reporting issues

Include your OS, terminal, the model/provider, and steps to reproduce. For a crash, the last few
lines of output and what you did before it help a lot.
