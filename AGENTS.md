# Vera Harness contribution instructions

- Keep the static prompt under 1,000 approximate tokens.
- Preserve the no-SDK agent loop and the provider/OS extension boundaries.
- Never log, serialize to sessions, or pass to child processes OAuth credentials.
- New mutating operations must flow through `PermissionPolicy`, `PathGuard`, and the session preimage journal.
- Run `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test` before a release.
- Do not add SQLite, `clap`, `ratatui`, Node/Bun runtime dependencies, or repository indexing.

