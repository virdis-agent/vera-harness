# Vera Harness

Vera is a macOS-first coding-agent CLI with a small, replaceable Rust core. It launches as `vera`, keeps an inline JSONL transcript, inspects repositories on demand, and uses native tools behind explicit approval boundaries.

This is an `0.x` compatibility adapter for subscription OAuth flows exposed by the providers’ own CLIs. It currently targets Apple Silicon macOS 13+ and discovers the available models through ChatGPT/Codex OAuth or xAI OAuth.

## Install

Once the repository and release assets are publicly reachable, install the latest packaged arm64 build with one command:

```sh
curl -fsSL https://raw.githubusercontent.com/virdis-agent/vera-harness/main/packaging/install.sh | sh
```

The installer verifies the release archive against `SHA256SUMS` before placing `vera` in `~/.local/bin`. The repository is currently private, so this public raw-URL command will remain unavailable until the installer endpoint is made public; private checkouts can use the source install below.

## Status

The repository contains the `0.1.0-alpha.25` core: manual CLI parsing, compact prompts, pinned OAuth/provider interfaces, protected token storage, Responses/SSE normalization, bounded tool calls, Seatbelt execution, Plan/Confirm/Auto/Yolo permission modes, Shift+Tab mode switching, adaptive Unicode-aware input editing, session-selectable tool displays, a streamlined skills-first dashboard, double Ctrl+C exit handling, path/symlink guards, atomic edit journals, versioned JSONL sessions/compaction, AGENTS.md and Skills discovery, runtime capability catalogs, loaded skills, reusable prompts, inert-by-default plugins, hooks, persistent stdio MCP, interactive questions/plans, PTY processes, guarded CDP/image inspection, worktree-isolated in-process subagents, a scrollable persistent terminal conversation, usable upgrade-safe user settings, and verified in-place upgrades.

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
vera --prompt-template review --prompt-args "focus on error handling" --output jsonl
vera auth login openai-codex
vera auth login xai-oauth --no-browser
vera models --refresh
vera -p "inspect the auth boundary" --effort high
vera inspect
vera session list
vera upgrade
```

Interactive commands include `/settings [get|set|unset] <key> [value]`, `/provider <id>`, `/models`, `/model [<id>]`, `/effort [<level>]`, `/display [grouped|minimal|detailed]`, `/skills`, `/skill load|unload <name>`, `/prompts`, `/prompt <name> [arguments]`, `/extensions`, `/extension enable|disable <name>`, `/mcp`, `/processes`, `/agents`, `/agent <id>`, `/plan`, and `/permissions [deny|ask|allow] <kind>`. `/settings` reports effective values, their source, and the editable file paths; arrays and permission rules use JSON syntax. Interactive provider, model, effort, display, permission, plugin, MCP, and loaded-skill changes persist to `.vera-global-state.json`; `/display` is also recorded in a session and restored on resume. `/model` and `/effort` open cancellable numbered pickers, and their argument forms select directly. Omitting `model` from configuration selects the provider catalog's default; sessions record the resolved model ID. The adaptive input block supports cursor movement, Home/End, Delete/Backspace, command history, Ctrl+U, Ctrl+W, and bracketed paste; Ctrl+Home/End jump through transcript history. Skills expose only metadata until loaded by command or the read-only `load_skill` tool. Plugins are inert until enabled. Press `Shift+Tab` to cycle Plan → Confirm → Auto → Yolo. In JSONL/headless mode, a question emits `needs_input` and exits nonzero; resume it interactively with `/resume <id>`.

Global defaults live in `~/.vera/config.toml`; project defaults live in `.vera/config.toml`, with project capability selections taking precedence. Reusable Markdown prompts are loaded from `~/.vera/prompts`, configured roots, enabled plugin roots, and `.vera/prompts` (project wins). MCP tools are namespaced as `mcp__server__tool`; every server start and tool call is separately permission-matched. Configure exact CDP endpoints with `browser_cdp_endpoints` or approve an endpoint interactively.

Every invocation idempotently initializes Vera's private home layout. System-managed paths include `version.json`, `installation_id`, `skills/.system/`, `plugins/cache/`, `cache/`, `rules/`, `.tmp/`, and `tmp/`. Runtime roots include `sessions/`, `archived_sessions/`, `attachments/`, `shell_snapshots/`, `vendor_imports/`, `prompts/`, `roles/`, and `logs/`. First run creates private, user-editable `config.toml`, `.vera-global-state.json`, and `keybindings.json`; existing bytes are preserved during bootstrap and upgrades. Authentication and history remain lazy, so Vera does not create empty authentication or unsupported-state placeholders. Model catalogs are cached under `cache/`, with legacy root-level catalogs remaining readable. Built-in skills load from `skills/.system/` below installed user skills in precedence. Vera intentionally uses JSON and JSONL rather than SQLite, and `AGENTS.md` remains optional user-provided instruction content.

## Security model

Repository reads are automatic. Writes, shell, network, external paths, hooks, plugins, MCP processes, and subagents are approval-gated. Plan mode blocks mutations. Paths are canonicalized and symlink escapes are rejected. Child environments are allowlisted and never receive the auth store. Tokens live under `~/.vera/auth.json` with private modes, locking, atomic replacement, refresh rotation, origin pinning, and redaction.

Sessions are append-only versioned JSONL under `~/.vera/sessions`. They record messages, typed provider inputs/tool calls, selections, loaded skills, MCP/process/subagent/worktree lifecycle, plans, pending questions, hook results, provider usage, approvals, preimages, compaction events, and task-scoped settings, never credentials. Provider-reported input usage is authoritative; before the first response the dashboard labels the full-request local estimate. `/undo` only restores Vera-managed preimages. Persistent memory and cross-session search are intentionally out of scope.

## Distribution

`packaging/install.sh` selects the arm64 macOS artifact and verifies `SHA256SUMS` before installation. Installer-managed copies update in place with `vera upgrade` (or `vera update`). Cargo- and Homebrew-managed copies should use `cargo install --path . --locked` or `brew upgrade vera-harness` instead.

## License

MIT. See [LICENSE](LICENSE).
