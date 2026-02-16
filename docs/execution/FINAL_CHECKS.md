# FINAL CHECKS

This file will be updated as tasks complete.

## Commands
- `cargo test`
- `cargo clippy -- -D warnings`

## Observed outputs
- 2026-02-16: `where cargo` -> not found (Rust toolchain not on PATH)
- 2026-02-16: `rustc --version` -> "The term 'rustc' is not recognized"
- 2026-02-16: `cargo --version` -> "The term 'cargo' is not recognized"

## Status
- BLOCKED: install Rust toolchain (rustup) or add to PATH, then re-run commands above.

## How to unblock (Windows)
- Install Rust via `rustup-init.exe` (default stable toolchain is fine).
- Close and reopen PowerShell / VS Code to refresh PATH.
- Re-run:
	- `cargo --version`
	- `cargo test`
	- `cargo clippy -- -D warnings`
