# codexmu

**English** | [한국어](README.ko.md)

A Rust program that stores multiple ChatGPT accounts for Codex and automatically switches to an available account when the current one reaches its usage limit.

**No `codex-auth`, `codext`, or Zig is required.** codexmu handles auth files, OAuth refresh, usage queries, and account selection in Rust. It uses the **official Codex executable** for login and terminal / desktop integration. npm installations use Node.js as the entry point; Cargo installations do not require Node.js.

## Installation

### npm

Requires **Node.js 24 or later** and official Codex. The release package bundles macOS / Linux binaries for ARM64 and x64, without a Rust build or separate binary download during installation.

Install from the public npm registry:

```sh
npm install -g codexmu
codexmu
```

You can install a locally built package immediately. Local builds include only the current platform's executable:

```sh
# Run from the repository; this build step requires Rust
npm run build
mkdir -p dist
npm pack --pack-destination dist
npm install -g ./dist/codexmu-0.1.0.tgz
codexmu --version
```

If you already installed through Cargo, use `command -v codexmu` to check which installation your PATH selects.

### Cargo

Requires Rust 1.89 or later and official Codex. Terminal mode supports macOS / Linux; desktop app launching supports macOS. Codex must support `--remote unix://...`; the previously tested CLI version is 0.153.4.

Run from the project directory after downloading the source:

```sh
cargo install --path . --locked
codexmu --help
```

If Cargo is not on your PATH, run `source "$HOME/.cargo/env"` first. You can also run `cargo build --release` and use `./target/release/codexmu` without installing it.

## Register accounts and start

**You can register two, three, or more accounts; codexmu imposes no account-count limit.** Save the account currently signed in to Codex, then log in to additional accounts under different names:

```sh
codexmu add personal
codexmu login work --device-auth
codexmu login extra --device-auth
codexmu switch personal
codexmu list --live
codexmu
```

All registered accounts are candidates for automatic switching. For example, if `personal` and then `work` reach their limits, codexmu can switch to an available `extra` account and continue the same conversation. Selection depends on remaining usage, not registration order. To prefer some accounts, give them a higher tier with `codexmu priority NAME 1`; usage decides only within a tier, and lower tiers are used once every account above them is unavailable. `codexmu priority personal -1` keeps `personal` as the reserve. Tiers decide where a switch goes; codexmu does not move back to a higher tier while the active account still has headroom.

`login` runs official `codex login` in a temporary `CODEX_HOME`. Cancelling or failing login preserves the existing active account. Omit `--device-auth` for browser login. If your credentials exist only in the keychain and there is no `auth.json`, use `login` instead of `add`.

You can also import an existing **standard Codex auth.json**:

```sh
codexmu add work --auth-file /path/to/work-auth.json
codexmu remove unused
```

Duplicate accounts, overwriting an existing name, and deleting the active account are rejected. Names must contain 1–64 ASCII letters, digits, hyphens, or underscores. API key accounts are excluded to avoid automatically switching to usage-based billing.

## Codex terminal — macOS / Linux

After registering accounts, **run `codexmu` to open the official Codex terminal UI.** When a usage-limit error occurs, it switches to another registered account and automatically continues work in the same conversation.

codexmu merges its own colored segments into the official status line below the input area and pins that line to the bottom row:

![codexmu terminal preview](docs/terminal-preview.png)

The image is a Terminal.app capture of a local fake-account run.

```text
› Explain this project
 codexmu │ gpt-5.1 medium │ …/codexmu │ main +2 │ 5h 85% · 0h42m │ user@example.com (plus)   Context 100% left · Fast off · 5h 85% · weekly 58% · 0.153.4
```

The status line shows the session model, reasoning effort, working directory, Git branch and change count, queried remaining usage, and active account email and plan. The time is the countdown to the usage reset. Once the server acknowledges an account switch, the status line updates and briefly shows a switch notice segment. Unavailable quota data appears as `—`; narrow windows shorten or hide path, Git, and native details. The mouse wheel and PageUp/PageDown scroll the Codex output while the status line stays in place; any other key jumps back to the live view. codexmu never captures the mouse, so selecting text, copying, and Cmd+click keep working exactly as in your terminal (wheel scrolling relies on the alternate-scroll behavior that Terminal.app, iTerm2, kitty, Ghostty, and WezTerm enable by default). Your terminal controls the background and font.

```sh
codexmu
codexmu "Explain this project"
codexmu run -- --model gpt-5.1
codexmu run -- resume --last

# Use the original official Codex layout without the status line
codexmu --plain
```

**Multiple `codexmu` windows can run simultaneously with the same `CODEX_HOME`.** Run `codexmu` in each terminal. They share the account list and default active account; each window's official Codex server manages its own conversations, approvals, and live authentication. When another window switches accounts, each window applies the new account during a usage check after its current turn finishes. A window receiving a usage-limit error attempts a switch immediately.

Account-store access, usage queries, and OAuth refresh are serialized by a store lock. Concurrent refreshes reuse tokens already refreshed by another window and do not overwrite the authentication of a window working with a different account. The lock is not held for the entire session.

A separate startup lock serializes server startup through the initialization response to avoid official Codex SQLite initialization conflicts in a fresh home. It releases immediately after initialization so sessions can work concurrently.

codexmu uses official Codex's `--remote unix://...` feature, verified with CLI 0.153.4. A private temporary Unix socket connects the native terminal UI to the authentication bridge, and codexmu composes the PTY display on an alternate screen with its own scrollback, so the status line stays pinned while you scroll the Codex output. On exit, codexmu removes the socket and restores terminal settings. It opens no TCP port. Use `--plain` when you need the terminal's native scrollback instead.

Pass Codex options after `run --` to avoid confusion with management commands and options. codexmu manages the `--remote` address. Use `codexmu --no-resume` to disable automatic continuation.

## Codex desktop app — macOS

**Quit the running Codex app first**, then run:

```sh
codexmu app
```

To specify the official CLI path:

```sh
codexmu --codex-bin /absolute/path/to/codex app
```

`app` uses macOS `open --env` to set `CODEX_CLI_PATH` to this binary. The app-launched `codexmu` forwards JSON-RPC between the app and official `codex app-server`. It does not modify the app installation or global configuration. An already-running app cannot receive these environment variables, so codexmu refuses to launch until you quit it.

Terminal and desktop modes share the same switching behavior:

- Query usage every 60 seconds by default, **only when no turn is running**.
- Look for another account immediately when a turn ends with `usageLimitExceeded`.
- Among available accounts in the highest priority tier, select the one with the lowest maximum usage across the usage windows present in the response.
- With `--switch-at 80`, also switch between turns once the active account reaches 80% and an account below 80% exists. An early switch is not a cooldown and sends no continuation turn.
- Send new credentials to the running official app-server through `account/login/start`, rather than only replacing a file.
- By default, send a new continuation turn in the same thread. Do not replay the original prompt or executed tool calls.
- Defer switching while another turn is running. Queue new turns during a switch while continuing to forward approval responses. Do not execute cancelled queued turns.

To switch accounts without automatic continuation:

```sh
codexmu --no-resume app
```

Automatic recovery from the same failure is limited to one attempt per account. If all accounts are exhausted, codexmu keeps monitoring usage without entering a retry loop. After an account recovers, you can ask it to continue. Ordinary network errors, server overload, and the word “limit” in model output do not trigger account switching.

## Standalone monitoring and other clients

```sh
# Evaluate once / preview the switching decision
codexmu watch --once
codexmu watch --once --dry-run

# Periodically switch auth.json
codexmu --interval 30 watch

# Server command for a JSON-RPC stdio client
codexmu app-server
codexmu app-server -- --stdio
```

`watch` manages files. **It cannot force a separately launched ordinary `codex` process to reload its in-memory authentication.** For live switching, use `codexmu`, `codexmu app`, or a `codexmu app-server` connection.

The `app-server` command supports stdio only and rejects `--listen`. Default terminal mode internally uses a private Unix socket for each session. Multiple terminals and bridges can run together; only standalone `watch` is limited to one process per home. Use `codexmu login/add/switch` instead of logging in or out through the connected UI.

You do not need a separate `codexmu watch` when running `codexmu`. A duplicate `watch` reports the owning process's PID. The OS releases the lock when the process exits; there is no need to delete lock files.

## Storage and configuration

```text
$CODEX_HOME/auth.json                       Active Codex authentication
$CODEX_HOME/codexmu/accounts/<name>.json     Account credentials, priority, and temporary exclusion time
$CODEX_HOME/codexmu/previous-auth.json       Previous active authentication backup
$CODEX_HOME/codexmu/pending-refresh.json     Interrupted OAuth refresh recovery journal
$CODEX_HOME/codexmu/terminal-<PID>.log        Per-session official server diagnostics
```

`CODEX_HOME` defaults to `~/.codex`. Use `--codex-home /path` for a separate account store. Login tokens are stored in local JSON without encryption. On Unix, managed directories are created with mode `0700` and authentication files with `0600`; files are replaced atomically. `list` does not print tokens. Locks and a recovery journal protect refreshes, and active tokens refreshed by an external Codex process are preserved before switching.

| Option | Environment variable | Default |
| --- | --- | --- |
| `--codex-home` | `CODEX_HOME` | `~/.codex` |
| `--codex-bin` | `CODEXMU_CODEX_BIN` | `codex` |
| `--interval` | `CODEXMU_INTERVAL` | 60 seconds; minimum 5 |
| `--no-resume` | `CODEXMU_NO_RESUME` | false |
| `--switch-at` | `CODEXMU_SWITCH_AT` | 100 (switch only at the limit); 1–100 |

Failed usage requests, responses without a valid usage window, and past reset timestamps are not treated as evidence of available quota. Accounts that reach their limits are excluded from selection for at least 60 seconds, and after a `usageLimitExceeded` error until the next reported usage reset, even if the usage report still shows headroom. `--dry-run` does not switch accounts, but may refresh OAuth tokens to keep credentials valid.

## Validation

```sh
npm test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
python3 tests/check.py

# Verify real authentication-switch RPCs with official Codex (fake test tokens)
python3 tests/check.py --native "$(command -v codex)"

# Official Codex terminal: input → A hits limit → B responds → /quit → terminal restored
python3 tests/terminal.py --codex-bin "$(command -v codex)" --resize
python3 tests/terminal.py --codex-bin "$(command -v codex)" --plain
python3 tests/terminal.py --codex-bin "$(command -v codex)" --sessions 3 --resize
```

Tests use temporary homes and a local HTTP server. They do not read personal credentials or consume real quota. Coverage includes HTTP errors, refresh after 401, full exhaustion, duplicate accounts, atomic saves, refresh recovery, RPC ID collisions, same-thread continuation after limits, approval forwarding, ordinary errors, and shutdown. `--native` verifies that after official Codex receives HTTP 429, **a subsequent model request in the same thread uses account B's token and completes successfully**, as well as checking the account change through `account/read`.

The previously documented validation environment is macOS ARM64 / official Codex CLI 0.153.4, covering builds, protocol checks, and real terminal PTY tests. Full desktop GUI operation and real-account quota exhaustion are outside that validation scope. `chatgptAuthTokens` is experimental, and the usage endpoint is not a public stable API; compatibility needs checking when Codex changes.

## Contributing

See [AGENTS.md](AGENTS.md) for the code layout, authentication and concurrency invariants, and checks appropriate to each change. Update both English and Korean READMEs when behavior or commands change.

## npm releases

Update the versions in `package.json` and `Cargo.toml` together. After pushing the code to GitHub, select **npm release → Run workflow** in Actions to build and check all four platforms and create an installable `.tgz` in the `npm-package` artifact. Linux builds use musl targets.

To publish, download the `npm-package` artifact and run `npm publish ./codexmu-<version>.tgz --access public` from a machine logged in with `npm login`. npm requires two-factor authentication on the publishing account; the command opens a browser approval step. Alternatively, add a granular npm token as the repository's **`NPM_TOKEN`** Actions secret and select **publish** in the workflow to publish from CI. Change `name` in `package.json` to rename the package. The workflow publishes only after builds and checks succeed for all four platforms.

Local `npm publish` also checks that executables for all four platforms are present. `npm pack` permits a current-platform-only package for local installation tests; do not publish that local-only `.tgz` publicly. There are no npm dependencies or installation scripts.

## References

- [Loongphy/codex-auth](https://github.com/Loongphy/codex-auth/tree/0fde29598c2e02e28e0e8bcc33a4bb8d45d7b23a): reference for auth-file structure and usage queries.
- [Loongphy/codext](https://github.com/Loongphy/codext/tree/50990b9913fd8f66456d9838dbeee572c6f10fc1): reference for authentication changes at safe turn boundaries and continuation after usage-limit errors.
- [Official Codex App Server documentation](https://developers.openai.com/codex/app-server): reference for JSON-RPC initialization, turn, and account protocols.

No runtime code downloads or invokes the binaries, source, or packages of the two reference projects.

## License

[MIT](LICENSE)
