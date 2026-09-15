# Strategy Decisions Log

This file records strategy-level decisions (not structural/code debt — see
[STRUCTURAL_DEBT.md](STRUCTURAL_DEBT.md) for that) and the evidence behind them. Read
this before wondering why a strategy is commented out or why the bot isn't trading live.

---

## 2026-09-09 — Live trading halted; mean-reversion disabled; trend_breakout/squeeze_breakout shelved

### Decision

- **Live trading stopped.** The Scheduled Task "Binance Survival Bot - 30min Bursts"
  (daily trigger, runs [run_bot_30min.ps1](../../run_bot_30min.ps1)) is being disabled/
  deleted via Task Scheduler. As defense-in-depth, the script itself now hard-exits
  before any trading activity and no longer defaults `BOT_LIVE_TRADING` to armed.
- **`mean_reversion_range_signal` disabled**, not deleted. Its dispatcher call site in
  [src/signals.rs](../../src/signals.rs) (`entry_long_signal`, `Regime::Ranging` arm) is
  commented out. The function and its unit tests are untouched and still compile —
  reference material for a future redesign, not reachable from live/practice/backtest
  until someone deliberately re-enables it.
- **`trend_breakout` and `squeeze_breakout` are shelved, not killed.** Still wired into
  the dispatcher, untouched in code. No further filter-engineering time on them until
  more historical data makes their samples (n=151 and n=56 over a 274-day slice) usable.

### Why: the evidence chain

This bot's backtesting engine ([src/backtest.rs](../../src/backtest.rs)) existed fully
built but was never wired to anything — no CLI, no test, no invocation path. Before
this date, every claim about strategy performance was either aspirational or, in the
one case with real operational data (`decision.jsonl`, `bot_state.json`), a handful of
trades netting roughly breakeven-to-slightly-negative after fees. There was no
statistically meaningful evidence either way.

**Infrastructure built to get real evidence** (all now part of the CLI, see `--help`-
style usage comments in [src/main.rs](../../src/main.rs)):
- `--fetch-history` — bulk historical klines fetch (used for 90-day and 365-day
  BTCUSDT pulls), separate from the live pipeline's ~200-candle rolling cache.
- `--backtest` / `--backtest --walk-forward=N` — runs the existing simulation engine,
  now actually reachable.
- `--backtest --start-frac=X --end-frac=Y` — chronological research/holdout split
  (75%/25% used this session) so exploration and confirmation don't share data.
- `--backtest --diagnose` — MFE/MAE by outcome, exit-reason breakdown, regime×strategy
  trade counts, feature threshold-sensitivity tables, feature-pair correlation. All
  scoped per-strategy to avoid a mixed view hiding a strategy-specific pattern.

**Three real bugs found and fixed along the way** (not just findings — code changes,
tested):
1. **Notional-cap ordering bug** ([src/risk.rs](../../src/risk.rs),
   [src/pipeline/mod.rs](../../src/pipeline/mod.rs)) — the 20%-of-equity position cap
   was applied *after* the affordability check, so a small account's uncapped
   risk-parity notional got rejected outright instead of being sized down to something
   affordable. Evidence: 859 `BLOCK_ENTRY` vs 12 `ENTER_LONG` in `decision.jsonl`.
2. **Backtest exit-fidelity gap** (`src/backtest.rs`) — the simulator never set a
   take-profit at entry (`tp_price: None` always) and didn't replicate the live
   pipeline's mean-reversion-specific `range_tp` exit, so it was testing a materially
   different (worse) exit ruleset than the one actually running live.
3. **range_tp mislabeling bug** (pipeline + backtest) — `"Mean reversion done. We take
   profit."` fired whenever price crossed the *current* (live, drifting) BB mid-band,
   not relative to entry price. On a 90-day sample this exit fired 687 times averaging
   **-0.10% net** — a loss booked under a take-profit label. Fixed by gating on
   `max(bb_mid, entry_price × (1 + round-trip cost))`. Confirmed fix: same exit then
   fired 187 times averaging **+0.18%**.
4. **Look-ahead bias in the backtest's 1h window** — `simulate()` computed the 1h
   candle window as `candles_1h[candles_1h.len()-200..]`, a fixed slice off the *end of
   the whole dataset*, for every single 5m bar regardless of that bar's actual time.
   For most of a 90-day walk this fed HTF trend context ~8 days in the *future*.
   Grepped the codebase afterward for the same pattern elsewhere — none found.

### What the corrected evidence actually shows

- **Regime mix is real, not a sampling artifact**: ~75-79% Ranging, ~14-18% Trending,
  ~7% Volatile, consistent across both a 90-day and a 365-day (274-day research slice)
  pull of real BTCUSDT data.
- **Mean-reversion (75-79% of trade volume)**: falling-knife entry signature — losers
  show near-zero favorable excursion (MFE ~0.06-0.10%) before adverse movement
  dominates (MAE ~0.20-0.31%), consistent at both n=1,116 and n=1,620. Three candidate
  confirmation filters tested at the static-table level (`velocity_1m`, `volume_z`,
  `velocity_5m`) were all flat — no usable win-rate separation at a real gate
  threshold. Full-sample backtest: -52% (90-day), -74% (274-day research slice,
  Profit Factor 0.15-0.17). Falsified at the structural level, not a parameter-tuning
  problem.
- **Trend-breakout**: healthier payoff shape (avg winner/loser ratio ~1.15-1.2:1 vs
  mean-reversion's ~0.5:1) but thin (n=62 at 90 days, n=151 at 274 days — win rate
  moved 17.7%→26.5% just from the sample-size increase, which is itself evidence it
  hadn't converged). The one filter candidate that looked real in a static table
  (`volume_z >= 2.0`, dominant bucket n=78 at 30.8% vs ~21% baseline) **failed full
  re-simulation**: gate-off vs gate-on full re-run gave 26.5% vs 26.4% win rate —
  effectively no change, because gating changes which trade sequence occurs
  (path-dependence in the event-driven `simulate()` loop), not just which subset of a
  fixed historical list survives. This is a structural property of that simulation
  loop, not specific to this one feature — treat any future static-table result the
  same way: promising-looking is not sufficient, only a full re-simulated A/B counts.
- **Squeeze-breakout**: n=25→56 across the two pulls. Never reached a usable sample.

### Bar to clear before anything returns to live capital

A redesigned entry thesis must show a **reproducible positive edge under full
re-simulation** (not a static bucket/threshold table — those are triage only, per the
trend_breakout lesson above), ideally consistent across walk-forward windows and
confirmed once against a holdout slice never touched during exploration. Until then
this is a research project against historical data, not a production trading system.

### Open items

- **Binance API returning `-2015` (invalid key/IP/permissions)** intermittently —
  observed in `critical.log` (2026-03-10/11) and again during a verification run this
  session. Not investigated (deliberately deferred — not blocking the halt). Resolve
  before the next time PRACTICE mode is needed to validate something, since it's the
  main way to confirm changes behave correctly against live market data without risking
  capital.
- **Scheduled task disable/delete** — being done manually (requires elevation this
  session didn't have). Confirm before relying on "no live trading" as complete.
