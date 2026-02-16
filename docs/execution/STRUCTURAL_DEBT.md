# Structural Debt Register

Date: 2026-02-16

This register lists structural/architectural debt items that remain after SD-001..SD-008 hardening. Each item includes evidence links.

Severity guide:
- P0: safety/real-money risk
- P1: high operational reliability risk
- P2: maintainability / correctness risk
- P3: polish / ergonomics

## Debt Items

### DEBT-001 (P2) Monolithic `run_once` orchestration
- Symptom: The runtime pipeline (connectivity gating → balances → rules → candles → features → signals → risk → execution → persistence) is implemented as a single large function, making changes and testing more brittle.
- Evidence: Formerly [src/app.rs](../../src/app.rs) now extracted into [src/pipeline/mod.rs](../../src/pipeline/mod.rs)
- Impact: Higher chance of regressions when touching any stage; harder to unit-test stages in isolation.
- Suggested remedy: Extract pure/side-effect-free steps into small functions/modules (e.g., `pipeline::fetch_market_data`, `pipeline::decide`, `pipeline::execute`) while keeping the public API stable.

Status: COMPLETE (extracted core orchestration into `pipeline::run_once_core` and introduced staged helpers).

### DEBT-002 (P1) Candle cache writes are non-atomic and lack corrupt quarantine
- Symptom: Candle cache is written via `fs::write` without atomic swap or quarantine on parse failure.
- Evidence: [src/candles.rs](../../src/candles.rs#L1-L220)
- Impact: A partial write (crash/interrupt) could poison subsequent runs until cache expires; error recovery is weaker than state persistence.
- Suggested remedy: Reuse the state-style atomic write pattern for cache writes, plus quarantine invalid JSON.

Status: COMPLETE (cache reads quarantine unreadable/invalid JSON and treat as miss; writes use shared atomic helper).

### DEBT-003 (P2) Monetary values use `f64` end-to-end
- Symptom: USDT/BTC quantities, fees, equity, and risk sizing are all represented as floating point.
- Evidence: [src/state.rs](../../src/state.rs#L1-L456), [src/risk.rs](../../src/risk.rs#L1-L260), [src/sizing.rs](../../src/sizing.rs#L1-L260)
- Impact: Rounding/precision edge cases; can produce off-by-a-tick/step behavior and drift in PnL/equity accounting.
- Suggested remedy: Introduce fixed-point integers (e.g., satoshis, cents) or a decimal type for accounting; keep `f64` only for indicators if desired.

Status: DEFERRED (large cross-cutting change across state/risk/sizing/execution; deferred to avoid a risky, oversized diff).

### DEBT-004 (P1) Retry/backoff has no jitter
- Symptom: Exponential backoff is deterministic; multiple instances (or retries across multiple endpoints) can synchronize.
- Evidence: [src/http_policy.rs](../../src/http_policy.rs#L1-L160)
- Impact: Thundering-herd behavior under rate limits or transient outages.
- Suggested remedy: Add small random jitter (and possibly `Retry-After` handling for 429), while preserving current env controls.

Status: COMPLETE (adds deterministic-testable jitter and honors 429 `Retry-After`).

### DEBT-005 (P2) Per-request timestamp signing can drift without explicit periodic sync
- Symptom: `binance_auth` supports offset tracking but is only adjusted by specific error-path logic (account `-1021`). Other signed endpoints use local time directly.
- Evidence: [src/binance_auth.rs](../../src/binance_auth.rs#L1-L200), [src/binance_orders.rs](../../src/binance_orders.rs#L1-L240), [src/account.rs](../../src/account.rs#L1-L220)
- Impact: Time-skew errors may appear on order endpoints even if account sync hasn’t occurred recently.
- Suggested remedy: Consider a lightweight periodic/boot-time time sync when keys are present (still safe in PRACTICE), or a shared signing helper that always uses `now_ms_with_offset()`.

### DEBT-006 (P2) Base URL injection is powerful but under-validated
- Symptom: `BOT_BASE_URL` is accepted as an arbitrary string.
- Evidence: [src/main.rs](../../src/main.rs#L1-L260)
- Impact: Misconfiguration can silently redirect traffic to a wrong host; SSRF-like risks if someone controls env.
- Suggested remedy: Validate scheme/host (e.g., require `https://` in normal operation) and optionally log a warning when non-https is used.

### DEBT-007 (P3) Logging verbosity and sensitive-context hygiene needs a policy
- Symptom: Some paths log detailed request context (lengths, timestamps) at `info!`, and JSON audit can append arbitrary event payloads to a file.
- Evidence: [src/account.rs](../../src/account.rs#L1-L160), [src/app.rs](../../src/app.rs#L220-L320)
- Impact: Operational noise; risk of accidentally adding sensitive fields to audit events later.
- Suggested remedy: Standardize which diagnostics are `debug` vs `info`, and define an allowlist for audit fields.

### DEBT-008 (P2) Stop-loss order lifecycle is not fully reconciled
- Symptom: State tracks an optional `stop_order_id`, and execution supports stop-loss-limit placement, but order reconciliation/cancellation behavior is not clearly centralized.
- Evidence: [src/state.rs](../../src/state.rs#L90-L200), [src/execution.rs](../../src/execution.rs#L1-L220)
- Impact: Potential for orphaned stop orders if flow changes or if orders are partially filled/canceled.
- Suggested remedy: Centralize stop-order management (place/replace/cancel) with explicit reconciliation steps.
