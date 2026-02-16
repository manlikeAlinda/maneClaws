# Binance Survival Bot — Structural Report (Evidence-Based)

Date: 2026-02-16

This report is intentionally evidence-linked. Every structural claim points to a code location.

## 1. Scope and build footprint
- The project is a Rust 2024 crate with Tokio async runtime and Reqwest HTTP client; dependencies and edition are declared in [Cargo.toml](Cargo.toml#L1-L18).
- The executable entrypoint configures tracing and runs either one-shot or loop mode; see [src/main.rs](src/main.rs#L1-L260).

## 2. Entrypoints and runtime modes (Practice vs Live)
- The bot decides PRACTICE vs LIVE via `BOT_LIVE_TRADING=1` AND `BOT_LIVE_CONFIRM=YES`; logic lives in [src/execution.rs](src/execution.rs#L1-L60).
- The entrypoint constructs config, decides loop mode via args/env, and executes `run_once` repeatedly in loop mode; see [src/main.rs](src/main.rs#L1-L260).

## 3. Deterministic filesystem layout (base/state/cache/lock)
- Paths are centralized and derived from `BOT_BASE_DIR` (absolute required) with targeted overrides (`BOT_STATE_PATH`, `BOT_DATA_DIR`, `BOT_LOCK_PATH`); see [src/paths.rs](src/paths.rs#L1-L89).
- The Windows scheduler wrapper forces deterministic working directory and sets `BOT_BASE_DIR` and `BOT_LOCK_PATH`; see [run_bot_30min.ps1](run_bot_30min.ps1#L1-L80).

## 4. Single-instance safety
- The bot acquires an exclusive lock file (`fs2`), and fails with a “lock busy” error if another instance holds it; see [src/single_instance.rs](src/single_instance.rs#L1-L55).
- The entrypoint treats “lock busy” as a clean exit to avoid double-trading; see [src/main.rs](src/main.rs#L1-L260).

## 5. Configuration and safety latches
- LIVE is a two-factor latch (enable + explicit confirmation) and defaults to PRACTICE if either is missing; see [src/execution.rs](src/execution.rs#L1-L60) and the scheduler default in [run_bot_30min.ps1](run_bot_30min.ps1#L12-L35).
- The bot supports base URL injection via `BOT_BASE_URL` (defaulting to `https://api.binance.com`); see [src/main.rs](src/main.rs#L1-L260).
- API keys may come from process env or dotenv; the entrypoint logs presence/length and trims whitespace before use; see [src/main.rs](src/main.rs#L1-L260).

## 6. HTTP behavior (timeouts, retries, backoff)
- HTTP calls share a centralized policy (timeout, retry count, exponential backoff) with env override knobs; see [src/http_policy.rs](src/http_policy.rs#L1-L140).
- Public endpoints (ping/price/klines/exchangeInfo) are called via the retry helper; examples include [src/app.rs](src/app.rs), [src/pipeline/mod.rs](src/pipeline/mod.rs), [src/candles.rs](src/candles.rs#L90-L170), and [src/exchange_info.rs](src/exchange_info.rs#L1-L70).

## 7. State model and persistence guarantees
- Bot state is a serde JSON struct with defaults for backward compatibility; position includes `Flat`, `Long`, and `ExternalInventory`; see [src/state.rs](src/state.rs#L90-L170).
- Persistence uses atomic write with a `.prev` snapshot and quarantines corrupt/unreadable state into a `quarantine/` folder; load/restore logic is in [src/state.rs](src/state.rs#L1-L120) and [src/state.rs](src/state.rs#L260-L380).

## 8. Trading pipeline structure (data → features → signals → risk → sizing)
- Candle acquisition supports cached JSON files with freshness checks; see [src/candles.rs](src/candles.rs#L1-L120).
- Features are computed from candles (EMA/ATR/Donchian/vol/z-score, plus SMA/RSI) in [src/features.rs](src/features.rs#L1-L260).
- Entry signal is “trend breakout long-only” and produces an action + stop price; see [src/signals.rs](src/signals.rs#L1-L90).
- Risk sizing applies daily-loss and drawdown gates, then rounds to exchange step size; see [src/risk.rs](src/risk.rs#L1-L160) and [src/sizing.rs](src/sizing.rs#L1-L120).

## 9. Private endpoints, order placement, and PRACTICE safety
- PRACTICE uses Binance `/api/v3/order/test` while LIVE uses `/api/v3/order`; the mode switch is enforced in [src/execution.rs](src/execution.rs#L40-L200) and calls into [src/binance_orders.rs](src/binance_orders.rs#L1-L120).
- There is an integration-style test that runs a stubbed HTTP server and asserts PRACTICE never hits the live order endpoint; see [tests/practice_safety.rs](tests/practice_safety.rs#L1-L220).
- When keys are missing or wallet access is rejected, the app stays in watch-only (no private endpoints used beyond the attempted balance fetch) and sets a WAIT decision; see [src/pipeline/mod.rs](src/pipeline/mod.rs).

## 10. Operational outputs and observability
- Log output is intentionally “two-tier”: IMPORTANT mode emits only friendly lines plus warnings/errors, while DEBUG mode enables crate debug; see [src/main.rs](src/main.rs#L1-L260).
- Friendly “grandmother” lines are centralized in `log_say`; see [src/log_say.rs](src/log_say.rs#L1-L80).
- Optional JSON audit logging is env-gated and can append to a file; see [src/pipeline/mod.rs](src/pipeline/mod.rs).

---

## Appendix A: Execution verification status
- SD task board shows SD-001..SD-008 complete: [docs/execution/TASK_BOARD.md](docs/execution/TASK_BOARD.md#L1-L40)
- Final checks record `cargo test` and `cargo clippy -- -D warnings` as PASS: [docs/execution/FINAL_CHECKS.md](docs/execution/FINAL_CHECKS.md#L1-L40)

---

## Appendix B: PRACTICE Wallet Access Failure Report (historical)

# Binance Survival Bot — PRACTICE Wallet Access Failure Report

Date: 2026-02-11

## Objective
Identify and fix the root cause of the PRACTICE-run message about wallet access failure, and surface the exact Binance reason safely (no secrets) with concrete evidence.

## Reproduction
1. Ensure you are in PRACTICE mode (default unless `BOT_LIVE_TRADING=1`).
2. Run:
   - `cargo run`

## Findings (Evidence)
### Environment loading
From startup logs:
- `Env source: dotenv loaded = false, .env exists = false`
- `BINANCE_API_KEY present = true, length = 3, source = process_env`
- `BINANCE_API_SECRET present = true, length = 3, source = process_env`

This proves:
- The process is **not** loading a `.env` file in the working directory.
- The API key/secret are coming from the **process environment**.
- Both values are present but extremely short (length 3), consistent with placeholders / truncated values.

### Signed Spot account request
The bot calls **Spot** account endpoint:
- `GET /api/v3/account` (base `https://api.binance.com`)

Request instrumentation (no secrets):
- Query (no signature): `recvWindow=5000&timestamp=<ms>`
- Signature length: `64` hex chars (expected for HMAC-SHA256 hex)

Response evidence:
- HTTP status: `401`
- Binance JSON payload (parsed):
  - `code = -2014`
  - `msg = "API-key format invalid."`

This is emitted in logs as:
- `Account response: http_status=401 binance_code=Some(-2014) binance_msg=Some("API-key format invalid.") ...`

## Root cause (one sentence)
Your `BINANCE_API_KEY` value is **not a valid Binance API key format** (Binance returned `code=-2014` / `"API-key format invalid."`), and in this run it is also only **3 characters long**, indicating it is placeholder/truncated.

## Fix implemented (minimum safe changes)
Code changes are intentionally minimal and safe (no secrets printed):

1. **Env source + key presence logging**
   - Logs whether `.env` was loaded and whether keys are present (and their lengths), plus a best-effort source label: `process_env` vs `dotenv`.
   - Implemented in: `src/main.rs`

2. **Instrumented signed `/api/v3/account` path**
   - Logs method/endpoint, timestamp used, local time, UTC wall time, query length, signature length.
   - Logs response HTTP status and body (truncated to 500 chars).
   - Parses and logs Binance `code` and `msg` when present.
   - Adds `recvWindow=5000`.
   - Implemented in: `src/account.rs`

3. **PRACTICE no longer crashes on wallet rejection**
   - PRACTICE falls back to watch-only and prints a reason-specific message using Binance `code`/`msg`.
   - Implemented in: `src/app.rs`

4. **Time sync fallback (only when needed)**
   - If Binance returns `code=-1021`, the bot calls `GET /api/v3/time`, computes offset, logs it, and retries the account call once.
   - Implemented in: `src/account.rs`

## What you need to do (Binance UI / local setup)
1. Replace the placeholder/truncated values with the real key pair:
   - Set `BINANCE_API_KEY` to the API key (long alphanumeric string)
   - Set `BINANCE_API_SECRET` to the secret (long string)
   - Avoid surrounding quotes and avoid trailing spaces.

2. Ensure API key permissions are correct:
   - Enable **Read** permission.
   - If you intend to trade later in LIVE, enable **Spot & Margin Trading** (but LIVE trading remains disabled unless `BOT_LIVE_TRADING=1`).

3. If you use IP restrictions:
   - Add this machine’s public IP to the key’s whitelist.

4. Windows note (`setx` behavior):
   - If you used `setx` to set env vars, you must open a **new terminal** (or restart VS Code) for new values to be visible.

## Verification
After setting correct values, re-run `cargo run` and confirm:
- Startup prints key present with a realistic length.
- `/api/v3/account` returns HTTP `200` and balances are printed.

---

## Appendix: Error code quick map
- `-2014`: API-key format invalid (wrong value in `BINANCE_API_KEY`)
- `-2015`: Invalid API-key, IP, or permissions
- `-1021`: Timestamp outside `recvWindow` (clock skew)
