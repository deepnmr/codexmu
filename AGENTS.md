# Repository guidance

## Scope and project

These instructions apply throughout this repository. codexmu is a Rust 2024
binary (minimum Rust 1.89) that manages ChatGPT OAuth accounts and bridges the
official Codex server to terminal and desktop clients. It requires official
Codex; it does not depend on codex-auth or codext at runtime. API key accounts
are deliberately unsupported.

## Code map

- `src/main.rs`: CLI parsing, account commands, standalone watch, macOS app launch.
- `src/accounts.rs`: account validation, atomic storage, locking, OAuth refresh
  recovery, usage queries, and account selection.
- `src/bridge.rs`: JSON-RPC forwarding, live authentication, queued turns,
  approval forwarding, and continuation after structured usage-limit errors.
- `src/terminal.rs`: Unix socket transport, native terminal lifecycle, and cleanup.
- `src/dashboard.rs`: PTY compositing with its own scrollback, mouse wheel scrolling, pinned status line, resizing, and Git status.
- `tests/check.py`: local fake-account integration checks; optional native Codex checks.
- `tests/terminal.py`: official Codex PTY checks using local fake inference.
- `bin/codexmu.mjs`: Node.js 24+ entry point that replaces itself with the native binary.
- `scripts/package.mjs`, `tests/npm.mjs`: binary staging, package checks, and launcher tests.
- `.github/workflows/npm-release.yml`: four-platform build and optional npm publication.

Terminal mode supports macOS / Linux; the `app` launcher is macOS-only.
`app-server` exposes stdio only. Terminal sessions use private Unix sockets.

## Making changes

- Trace the relevant flow and all callers before editing. Reuse existing helpers
  and installed dependencies; keep changes focused on the requested behavior.
- Keep `README.md` (English) and `README.ko.md` (Korean) synchronized, including
  commands, defaults, platform support, and compatibility limitations.
- Verify documentation against the current source. In particular, multiple
  terminal and bridge sessions are supported in one `CODEX_HOME`.
- Do not edit generated `target/` output or introduce dependencies for documentation.
- Keep `package.json` and `Cargo.toml` versions aligned. Treat `vendor/` and `dist/`
  as generated package output; local packages may contain only the host binary.

## Invariants to preserve

- Never log or commit real tokens, account files, or personal server logs. Use
  temporary homes and fake credentials for tests; do not use the user's Codex home.
- Preserve account identity validation, duplicate rejection, active-account deletion
  protection, atomic auth-file replacement, and Unix permissions (0700/0600).
- Keep refresh recovery through `pending-refresh.json` and preserve tokens rotated
  by other sessions or external Codex processes. Do not replace a locked inode.
- `store.lock` serializes store operations, including usage queries and refresh.
  `server-start.lock` covers startup through initialization only. `bridge.lock`
  is the standalone watch lock, not a single-session lock for live bridges.
- Switch live authentication through `account/login/start` and wait for server
  acknowledgement. Respect active turns, queued-turn cancellation, and approvals.
- Trigger failure recovery only for structured `usageLimitExceeded` errors;
  keep recovery bounded per account and do not replay executed tool calls.
- Treat unavailable or invalid usage data conservatively. `watch` updates files;
  it does not update a separately running Codex process's in-memory credentials.
- Preserve terminal restoration, child-process cleanup, and private socket removal.

## Validation

For Rust changes, run:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

For npm launcher or packaging changes, use Node.js 24+ and run `npm run build`
followed by `npm test`. These checks require a staged native binary. The release
workflow validates all four platforms before optional publication.

For account, CLI, or bridge behavior, build the executable before integration checks:

```sh
cargo build
python3 tests/check.py
```

For protocol or terminal changes, also run the relevant native checks when official
Codex with `--remote` support is available:

```sh
python3 tests/check.py --native "$(command -v codex)"
python3 tests/terminal.py --codex-bin "$(command -v codex)" --resize
python3 tests/terminal.py --codex-bin "$(command -v codex)" --plain
python3 tests/terminal.py --codex-bin "$(command -v codex)" --sessions 3 --resize
```

The Python checks use the standard library, temporary homes, and a local HTTP
server. Both default to `target/debug/codexmu`; `CODEXMU_TEST_BIN` overrides it.
Add a focused regression check for changed nontrivial behavior using the existing
tests. For documentation-only changes, check local links, code fences, command
names, and agreement between languages; no new test framework is needed.

Report which checks actually ran and any unavailable native coverage. CLI 0.153.4
is the previously documented compatibility baseline, not a claim about newer
versions. Full desktop GUI and real quota exhaustion are not covered by the local
fake-account tests.
