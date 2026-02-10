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
