# Account-switch reliability implementation plan

**Goal:** Preserve rotated tokens and responsive RPC forwarding across account changes, with native MCP regression coverage.

**Architecture:** Keep the existing account store, locks, refresh journal, and official login RPC. Run session refresh outside the forwarding loop; bound the RPC reply separately from the refresh transaction so a late rotated token can still be saved.

**Tech stack:** Existing Rust/Tokio and Python standard-library fixtures; no new dependencies.

**Spec:** User-approved order: token overwrite detection/prevention, refresh timing and forwarding, duplicate login removal, MCP switch regression checks.

## Constraints

- Preserve existing priority/early-switch changes and all unrelated files.
- Use temporary homes and fake credentials; never read personal auth or logs.
- Keep refresh journaling, account identity validation, store locks, permissions, cancellation, and bounded quota recovery.
- Keep README.md and README.ko.md synchronized.

## Ordered work

- [ ] In `src/accounts.rs`, add a regression for external rotation followed by same-account switch and list. Observe failure, then reconcile current authentication before refreshing/activating the target.
- [ ] In `tests/check.py`, hold the fake token endpoint while sending another RPC and an approval. Require forwarding before release and an error response before the native 10-second deadline. In `src/bridge.rs`, track a refresh task, its original request/account and reply deadline; preserve late token persistence and do not apply old-account results after a switch. Keep existing pre-expiry refresh.
- [ ] Add a metadata-only auth change regression; compare `login_params()` rather than full JSON in `src/bridge.rs` and update the cached active account even without login.
- [ ] Extend the native fixture in `tests/check.py` with an MCP ping before/after account failover and verify an unchanged stdio connection remains usable. Update both READMEs with the checked behavior and limitations.
- [ ] Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo build`, fake/native integration checks, and the three documented terminal checks. Inspect the final diff for unrelated edits.
