# AGENTS.md

- This repo owns the `sessiongator` AI session browser binary.
- Claude Code and opencode source adapters, session indexing, search, resume selection, and session-specific UI behavior live here.
- Generic terminal/tooling helpers should stay in `gator`.
- Verify with `cargo fmt -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test`.
