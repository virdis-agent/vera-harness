# Vera Harness

Vera is a macOS-first coding-agent CLI with a small, replaceable Rust core. It launches as `vera`, keeps an inline JSONL transcript, inspects repositories on demand, and uses native tools behind explicit approval boundaries.

This is an `0.x` compatibility adapter for subscription OAuth flows exposed by the providers’ own CLIs. It currently targets Apple Silicon macOS 13+ and defaults to `gpt-5.6` through ChatGPT/Codex OAuth or `grok-4.5` through xAI OAuth.

## Install

Once the repository and release assets are publicly reachable, install the latest packaged arm64 build with one command:

```sh
curl -fsSL https://raw.githubusercontent.com/virdis-agent/vera-harness/main/packaging/install.sh | sh
```

The installer verifies the release archive against `SHA256SUMS` before placing `vera` in `~/.local/bin`. The repository is currently private, so this public raw-URL command will remain unavailable until the installer endpoint is made public; private checkouts can use the source install below.

## Status

The repository contains the `0.1.0-alpha.1` core: manual CLI parsing, compact prompts, pinned OAuth/provider interfaces, protected token storage, Responses/SSE normalization, bounded tool calls, Seatbelt execution, approvals, plan mode, path/symlink guards, atomic edit journals, JSONL sessions/compaction, AGENTS.md and Skills discovery, hooks, local plugins, stdio MCP, and bounded subagent coordination.

Subscription OAuth is intentionally isolated in compatibility adapters. Real provider accounts and live endpoint contracts should be smoke-tested before each release.

## Build

```sh
git clone https://github.com/virdis-agent/vera-harness.git
cd vera-harness
brew install rust
cargo install --path . --locked

cargo build --release
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

The release profile strips, LTOs, and aborts on panic. `scripts/check-release.sh` runs the release gates and checks the stripped arm64 binary size.

## Use

```sh
vera                         # interactive session in the current repository
vera path/to/repository
vera -p "inspect the auth boundary" --output text
vera -p "summarize the diff" --output jsonl
vera auth login openai-codex
vera auth login xai-oauth --no-browser
vera models --refresh
vera inspect
vera session list
```

Interactive commands include `/provider`, `/model`, `/plan`, `/permissions`, `/compact`, `/context`, `/diff`, `/undo`, `/resume`, `/skills`, `/mcp`, `/agents`, and `/quit`.

## Security model

Repository reads are automatic. Writes, shell, network, external paths, hooks, plugins, MCP processes, and subagents are approval-gated. Plan mode blocks mutations. Paths are canonicalized and symlink escapes are rejected. Child environments are allowlisted and never receive the auth store. Tokens live under `~/.vera/auth.json` with private modes, locking, atomic replacement, refresh rotation, origin pinning, and redaction.

Sessions are append-only versioned JSONL under `~/.vera/sessions`. They record messages, tool calls, approvals, preimages, and compaction events, never credentials. `/undo` only restores Vera-managed preimages.

## Distribution

`packaging/install.sh` selects the arm64 macOS artifact and verifies `SHA256SUMS` before installation. Homebrew users should use the formula in `packaging/homebrew/vera-harness.rb`; `vera update` directs Homebrew-managed copies to `brew upgrade`.

## License

MIT. See [LICENSE](LICENSE).
