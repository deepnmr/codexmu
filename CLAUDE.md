# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**codexmu** is a Rust-based account manager for the official Codex desktop app. It manages multiple ChatGPT accounts (OAuth-authenticated), automatically monitors usage limits, and switches to available accounts when one hits its limit. The program bridges between Codex's JSON-RPC protocol and manages auth.json files.

Key constraints:
- Only supports ChatGPT OAuth accounts (not API keys)
- Single process per `CODEX_HOME` directory (file-based locking)
- macOS app-server mode only (desktop integration)
- Depends on official Codex binary being installed

## Build, Test, Lint

```sh
# Build
cargo build --release

# Format check
cargo fmt --check

# Lint (strict: warnings as errors)
cargo clippy --all-targets -- -D warnings

# Run all checks
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test

# Rust unit tests
cargo test

# Integration tests (requires Codex binary if --native flag used)
python3 tests/check.py

# Native integration (verifies actual RPC with official Codex; uses fake tokens)
python3 tests/check.py --native "$(command -v codex)"
```

## Architecture

### Three Core Modules

**accounts.rs** — Account and token lifecycle management
- `Auth`: Wraps auth.json data; validates structure, decodes JWT claims (email, plan, user ID), checks expiration, refreshes OAuth tokens
- `Store`: File-based account repository; atomic writes prevent corruption; per-account `blocked_until` tracks rate-limit backoff
- `Manager`: High-level orchestration—refreshes tokens, queries usage API, probes current quotas, picks next account based on available usage percent
- `Usage`: Parses OpenAI's rate-limit windows (primary / secondary, both have percent-used and reset time); considers exhausted if `limit_reached=true` or either window is 100% without reset

**bridge.rs** — JSON-RPC proxy for live app integration
- Runs as `app-server` subprocess to Codex; sits between client (Codex desktop app) and official app-server
- Detects `usageLimitExceeded` errors in turn completions; triggers account refresh via manager
- Queues requests during account switches; resumes single turn-per-thread on new account (sends prompt fragment "The account has been switched...")
- Prevents concurrent switches: blocks gated methods (`turn/start`, etc.) while credential job is in flight
- Handles `account/chatgptAuthTokens/refresh` RPC from app; refreshes stored account and returns new tokens

**main.rs** — CLI and command dispatch
- Parses global options: `--codex-home`, `--codex-bin`, `--interval`, `--no-resume`
- Subcommands: `add` (save existing auth.json), `login` (OAuth via official Codex), `list`, `switch`, `remove`, `watch` (poll and rotate auth.json), `app-server` (stdio bridge), `app` (macOS launcher)
- Bridge auto-launch: sets `CODEXMU_BRIDGE=1` env var; when Codex invokes codexmu as `CODEX_CLI_PATH`, detects this flag and runs bridge mode

### Key Invariants

1. **Token freshness**: Access tokens are decoded as JWT; if expiration claim + 60s <= now, token is considered expired and auto-refreshed before use.
2. **Atomic activation**: `auth.json` is swapped atomically (temp file + rename). Previous auth backed up to `previous-auth.json` before replacement.
3. **Refresh crash recovery**: If refresh starts but crashes, `pending-refresh.json` saves the intent. On next `lock()`, recovery runs before returning: commits new tokens if identity matches, cleans up.
4. **One bridge per home**: File lock `bridge.lock` ensures only one `app-server` instance owns a `CODEX_HOME`.
5. **Conservative quota eval**: Both windows must be valid and not expired to be considered; exhausted if either hits 100% without reset time or if OpenAI says `allowed=false`.

## Testing Approach

**Unit tests** (in source files):
- `accounts.rs`: Atomic switch, token preservation, refresh recovery, duplicate rejection, permission modes (0700 dir, 0600 file on Unix)
- `bridge.rs`: Only structured `usageLimitExceeded` errors trigger switches; network errors and generic "limit" text do not

**Integration tests** (`tests/check.py`):
- Uses temp home directories and local HTTP mocking (not real OpenAI)
- Covers: HTTP error handling, 401 refresh retry, full quota exhaustion, duplicate account rejection, atomic save, pending-refresh recovery, RPC ID collision handling, same-thread turn resumption, forward/response flow, account blinding, graceful shutdown
- `--native` flag: launches actual official Codex; verifies it switches accounts mid-conversation on HTTP 429 and completes the turn (not just retried—full model request on new account)

Protocol: Tests spawn codexmu, send JSON-RPC messages over stdio, parse responses.

## Common Workflows

**Add accounts**:
```sh
codexmu add personal --auth-file ~/.codex-personal/auth.json
codexmu login work --device-auth
codexmu switch personal
```

**Monitor usage** (file mode—not live):
```sh
codexmu list --live
codexmu watch --once
codexmu --interval 30 watch  # every 30 seconds
```

**Bridge mode** (live desktop app):
```sh
codexmu app  # macOS only; re-invokes itself via CODEX_CLI_PATH
# or manually:
codexmu app-server
```

## Token and File Layout

```
$CODEX_HOME/auth.json                           ← active (swapped atomically)
$CODEX_HOME/codexmu/accounts/<name>.json        ← per-account auth + blocked_until
$CODEX_HOME/codexmu/previous-auth.json          ← backup of prior active
$CODEX_HOME/codexmu/pending-refresh.json        ← (temp) recovery journal during OAuth
$CODEX_HOME/codexmu/store.lock                  ← per-operation lock (30s timeout)
$CODEX_HOME/codexmu/bridge.lock                 ← exclusive (app-server only)
```

Permissions (Unix): directories 0700, files 0600. Not encrypted on disk; rely on file system permissions and macOS keychain for local security.

## Configuration & Environment

| Flag | Env | Default | Notes |
|------|-----|---------|-------|
| `--codex-home` | `CODEX_HOME` | `~/.codex` | Account storage root |
| `--codex-bin` | `CODEXMU_CODEX_BIN` | `codex` | Path to official Codex binary |
| `--interval` | `CODEXMU_INTERVAL` | 60s | Min 5s; watch/app-server polling interval |
| `--no-resume` | `CODEXMU_NO_RESUME` | false | Skip auto-resume turn after switch |
| (hidden) `--usage-url` | `CODEXMU_USAGE_URL` | OpenAI endpoint | For testing / alternate endpoints |
| (hidden) `--token-url` | `CODEXMU_TOKEN_URL` | OAuth token endpoint | For testing |

Auto-detection: When `CODEXMU_BRIDGE=1`, codexmu runs `app-server` mode and passes through non-app-server commands to official Codex.

## Compatibility Notes

- **Codex Protocol**: Experimental `chatgptAuthTokens` RPC. Codex version 0.153.4 tested; breaking changes in Codex may require updates.
- **OpenAI Usage API**: Not a public stable endpoint; format/availability may change.
- **JWT Parsing**: Decodes `access_token` and `id_token`; extracts claims for user ID, email, plan type. Tolerant of missing fields (falls back to defaults).
- **macOS-specific**: `app` command uses `osascript` to detect running app and `open --env` to launch with env vars. Unix/Linux users should use `app-server` directly.

## Ponytail Notes

- Serial account probes in `prepare()` (accounts.rs:541); use bounded concurrency if account pools grow large.
- Recovery turn per account per failure chain (bridge.rs:101); no retry budget beyond that.
- No network retries; treat timeouts/HTTP errors as permanent per account per 60s.
