# FINAL CHECKS

This file will be updated as tasks complete.

## Commands

- `cargo test`
- `cargo clippy -- -D warnings`

## Observed outputs

- 2026-02-16: `where cargo` -> not found (Rust toolchain not on PATH)
- 2026-02-16: `rustc --version` -> "The term 'rustc' is not recognized"
- 2026-02-16: `cargo --version` -> "The term 'cargo' is not recognized"
- 2026-02-16: `cargo --version` -> `cargo 1.93.1 (083ac5135 2025-12-15)`
- 2026-02-16: `rustc --version` -> `rustc 1.93.1 (01f6ddf75 2026-02-11)`
- 2026-02-16: `cargo test` -> PASS
- 2026-02-16: `cargo clippy -- -D warnings` -> PASS
- 2026-02-16: post-DEBT-008 `cargo test` -> PASS
- 2026-02-16: post-DEBT-008 `cargo clippy -- -D warnings` -> PASS
- 2026-02-16: post-stop-replace-tracking `cargo test` -> PASS
- 2026-02-16: post-stop-replace-tracking `cargo clippy -- -D warnings` -> PASS
- 2026-09-09: pre-audit `cargo clippy -- -D warnings` -> FAIL (6 errors in `src/backtest.rs` and
  `src/telemetry.rs`, both added after the 2026-02-16 checks above and never covered by this file)
- 2026-09-09: post-audit (risk.rs sizing-cap fix, main.rs `--backtest`/`--fetch-history` CLI,
  backtest.rs `data_dir` parameterization + clippy cleanup, telemetry.rs arg-count allowlist)
  `cargo test` -> PASS (59 tests)
- 2026-09-09: post-audit `cargo clippy -- -D warnings` -> PASS
- 2026-09-09: note — `cargo clippy --all-targets -- -D warnings` (stricter than this file's
  command; also lints test code) still has one pre-existing, untouched issue in
  `src/http_policy.rs`'s test module. Not covered by the command this file checks.

## Status

- PASS: final checks completed successfully (against the `cargo clippy -- -D warnings` command
  this file specifies — see the `--all-targets` note above for a known gap in that command's coverage).

## How to unblock (Windows)

- Install Rust via `rustup-init.exe` (default stable toolchain is fine).
- Close and reopen PowerShell / VS Code to refresh PATH.
- Re-run:
  - `cargo --version`
  - `cargo test`
  - `cargo clippy -- -D warnings`
