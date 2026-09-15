# Changelog

Notable changes, newest first. Tag the commit that closes each entry (`git tag <label>`) so a rollback has a named target instead of a guessed commit.

## Unreleased — pre-production audit remediation (2026-09-15)

Closed the blocking findings from the pre-production readiness audit (correctness, data integrity, security, observability, dependency/CI, performance, rollback):

- Order placement now carries a `newClientOrderId` and reconciles via `origClientOrderId` on an ambiguous failure, instead of a blind retry that could double-place an order.
- A buy that fills but whose follow-up fill-detail lookup fails no longer loses track of the resulting position — it's persisted as pending and resolved on the next tick.
- If a panic-flatten sell itself fails, local state no longer falsely claims the position was closed.
- `bot_state.json` corruption with no usable `.prev` backup now refuses to trade (loudly) instead of silently starting over with a fresh, empty risk-governance state. Override: `BOT_ACCEPT_FRESH_STATE=1`.
- Startup now refuses to run LIVE if the configured API key has withdrawal permission enabled (checked via Binance's `apiRestrictions`).
- The dashboard's control/backtest endpoints now require a per-process token (closes a CSRF path on pause/resume), cap backtest/history-fetch to one job at a time, and reject non-relative `data_dir`/`out_dir` paths and non-alphanumeric symbols.
- Order-rejection, stale-candle, and panic-flatten log lines promoted from `info!`/`debug!` to `warn!`/`error!` so they're visible without setting `BOT_LOG_LEVEL=debug`.
- Added `BOT_ALERT_SMTP_*`/`BOT_ALERT_EMAIL_*`-configured email alerting (opt-in, no-op if unset) for: bot death, an unhandled tick error, auth-cooldown engagement, and panic-flatten outcomes.
- Added CI (`.github/workflows/ci.yml`): `cargo build --locked` + `cargo test --locked` on every push.
