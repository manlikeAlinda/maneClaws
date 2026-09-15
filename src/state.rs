#![allow(clippy::collapsible_if)]

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

use crate::persist;

fn ms(hours: u64) -> u64 {
    hours * 60 * 60 * 1000
}

fn ms_minutes(minutes: u64) -> u64 {
    minutes * 60 * 1000
}

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_millis() as u64
}

pub fn today_yyyymmdd_utc() -> String {
    // Needs `time` crate formatting feature (enabled in Cargo.toml).
    let now = time::OffsetDateTime::now_utc();
    let fmt = time::macros::format_description!("[year][month][day]");
    now.format(&fmt).unwrap_or_else(|_| "19700101".to_string())
}

// NOTE: atomic write/quarantine helpers are centralized in `crate::persist`.

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Position {
    #[default]
    Flat,
    ExternalInventory {
        btc_qty: f64,
        detected_time_ms: u64,
    },
    Long {
        entry_price: f64,
        qty: f64,
        initial_stop_price: f64,
        stop_price: f64,
        #[serde(default)]
        tp_price: Option<f64>,
        peak_price: f64,
        entry_time_ms: u64,
        last_peak_time_ms: u64,
        #[serde(default)]
        stop_order_id: Option<u64>,
        #[serde(default)]
        exchange_stop_price: Option<f64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionDecision {
    Hold,
    ExitLong,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct BotState {
    // Legacy fields (kept for backward compatibility with existing JSON)
    pub starting_usdt: f64,
    pub realized_pnl_usdt: f64,

    // Equity anchor: used for "-20% dead" and "+20% unlock" logic.
    #[serde(default)]
    pub start_equity_usdt: f64,
    #[serde(default)]
    pub profit_unlocked: bool,
    #[serde(default)]
    pub anchor_bumps: u32,
    #[serde(default)]
    pub risk_reduction_until_ms: u64,

    // Current live fields
    pub equity_usdt: f64,
    pub fees_usdt: f64,
    pub is_dead: bool,
    pub peak_equity_usdt: f64,
    pub position: Position,
    #[serde(default)]
    pub last_stop_order_id: Option<u64>,
    pub cooldown_until_ms: u64,
    pub hibernation_until_ms: u64,
    pub daily_loss_start_equity_usdt: f64,
    pub day_anchor_yyyymmdd: String,
    pub trades_today: u32,

    // Session tracking
    #[serde(default)]
    pub session_start_equity_usdt: f64,
    #[serde(default)]
    pub session_pnl_usdt: f64,

    // Authentication safety (Soft Circuit Breaker)
    #[serde(default)]
    pub last_auth_error_ms: u64,
    #[serde(default)]
    pub auth_error_cooldown_ms: u64,

    // Set immediately after a real buy order is placed, before the follow-up
    // fill-detail lookup - so if that lookup fails, the next tick can resolve
    // the real order instead of the bot losing track of a live position it
    // never got to record via enter_long(). Cleared once the entry completes.
    #[serde(default)]
    pub pending_entry_order_id: Option<u64>,
    // The stop price that was already decided for the pending entry above -
    // persisted alongside it so the next tick can complete enter_long() with
    // the correct protective stop rather than recomputing one against
    // possibly-changed market conditions a tick later.
    #[serde(default)]
    pub pending_entry_stop_price: Option<f64>,
}

impl Default for BotState {
    fn default() -> Self {
        let day = today_yyyymmdd_utc();
        Self {
            starting_usdt: 0.0,
            realized_pnl_usdt: 0.0,
            start_equity_usdt: 0.0,
            profit_unlocked: false,
            anchor_bumps: 0,
            risk_reduction_until_ms: 0,
            equity_usdt: 0.0,
            fees_usdt: 0.0,
            is_dead: false,
            peak_equity_usdt: 0.0,
            position: Position::Flat,
            last_stop_order_id: None,
            cooldown_until_ms: 0,
            hibernation_until_ms: 0,
            daily_loss_start_equity_usdt: 0.0,
            day_anchor_yyyymmdd: day,
            trades_today: 0,
            session_start_equity_usdt: 0.0,
            session_pnl_usdt: 0.0,
            last_auth_error_ms: 0,
            auth_error_cooldown_ms: 0,
            pending_entry_order_id: None,
            pending_entry_stop_price: None,
        }
    }
}

impl BotState {
    pub fn new(starting_usdt: f64) -> Self {
        let day = today_yyyymmdd_utc();
        Self {
            starting_usdt,
            realized_pnl_usdt: 0.0,
            start_equity_usdt: starting_usdt,
            profit_unlocked: false,
            anchor_bumps: 0,
            risk_reduction_until_ms: 0,
            equity_usdt: starting_usdt,
            fees_usdt: 0.0,
            is_dead: false,
            peak_equity_usdt: starting_usdt,
            position: Position::Flat,
            last_stop_order_id: None,
            cooldown_until_ms: 0,
            hibernation_until_ms: 0,
            daily_loss_start_equity_usdt: starting_usdt,
            day_anchor_yyyymmdd: day,
            trades_today: 0,
            session_start_equity_usdt: starting_usdt,
            session_pnl_usdt: 0.0,
            last_auth_error_ms: 0,
            auth_error_cooldown_ms: 0,
            pending_entry_order_id: None,
            pending_entry_stop_price: None,
        }
    }

    pub fn sync_equity_and_day(&mut self, equity_usdt: f64) {
        self.equity_usdt = equity_usdt;
        if self.start_equity_usdt <= 0.0 {
            self.start_equity_usdt = equity_usdt;
        }
        if self.peak_equity_usdt <= 0.0 {
            self.peak_equity_usdt = equity_usdt;
        }
        if equity_usdt > self.peak_equity_usdt {
            self.peak_equity_usdt = equity_usdt;
        }

        let today = today_yyyymmdd_utc();
        if self.day_anchor_yyyymmdd != today {
            self.day_anchor_yyyymmdd = today;
            self.daily_loss_start_equity_usdt = equity_usdt;
            // new day, let the bot try again
            self.hibernation_until_ms = 0;
            self.trades_today = 0;
        }

        if self.equity_usdt <= 0.0 {
            self.is_dead = true;
        }

        if self.session_start_equity_usdt <= 0.0 {
            self.session_start_equity_usdt = equity_usdt;
        }
        self.session_pnl_usdt = self.equity_usdt - self.session_start_equity_usdt;
    }

    pub fn bump_trades_today(&mut self) {
        self.trades_today = self.trades_today.saturating_add(1);
    }

    pub fn in_cooldown(&self, now_ms: u64) -> bool {
        now_ms < self.cooldown_until_ms
    }

    pub fn in_hibernation(&self, now_ms: u64) -> bool {
        now_ms < self.hibernation_until_ms
    }

    pub fn in_auth_cooldown(&self, now_ms: u64) -> bool {
        now_ms < self.last_auth_error_ms.saturating_add(self.auth_error_cooldown_ms)
    }

    pub fn apply_fee(&mut self, fee_usdt: f64) {
        self.fees_usdt += fee_usdt;
        self.equity_usdt -= fee_usdt;
        if self.equity_usdt <= 0.0 {
            self.is_dead = true;
        }
    }

    pub fn enter_long(&mut self, entry_price: f64, qty: f64, stop_price: f64, tp_price: Option<f64>, now_ms: u64) {
        self.position = Position::Long {
            entry_price,
            qty,
            initial_stop_price: stop_price,
            stop_price,
            tp_price,
            peak_price: entry_price,
            entry_time_ms: now_ms,
            last_peak_time_ms: now_ms,
            stop_order_id: None,
            exchange_stop_price: None,
        };
        // Stop id belongs to the new position lifecycle.
        self.last_stop_order_id = None;
    }

    pub fn eval_death_and_unlock(&mut self, equity_usdt: f64, _now_ms: u64) -> (bool, bool) {
        if self.is_dead {
            return (false, false);
        }

        if self.start_equity_usdt <= 0.0 {
            self.start_equity_usdt = equity_usdt;
            return (false, false);
        }

        let died_now = equity_usdt <= self.start_equity_usdt * 0.80;
        if died_now {
            self.is_dead = true;
            return (true, false);
        }

        let unlocked_now = equity_usdt >= self.start_equity_usdt * 1.20;
        if unlocked_now {
            self.profit_unlocked = true;
        }

        (false, unlocked_now)
    }

    pub fn set_stop_order(&mut self, order_id: u64, stop_price: f64) {
        if let Position::Long {
            stop_order_id,
            exchange_stop_price,
            ..
        } = &mut self.position
        {
            *stop_order_id = Some(order_id);
            if stop_price.is_finite() && stop_price > 0.0 {
                *exchange_stop_price = Some(stop_price);
            }
        }
        self.last_stop_order_id = Some(order_id);
    }

    pub fn clear_stop_order_id_in_position(&mut self) {
        if let Position::Long {
            stop_order_id,
            exchange_stop_price,
            ..
        } = &mut self.position
        {
            *stop_order_id = None;
            *exchange_stop_price = None;
        }
    }

    pub fn set_exchange_stop_price_from_exchange(&mut self, stop_price: f64) {
        if !(stop_price.is_finite() && stop_price > 0.0) {
            return;
        }
        if let Position::Long {
            exchange_stop_price,
            ..
        } = &mut self.position
        {
            *exchange_stop_price = Some(stop_price);
        }
    }

    pub fn clear_last_stop_order_id(&mut self) {
        self.last_stop_order_id = None;
    }

    pub fn exit_to_flat_with_cooldown(&mut self, now_ms: u64) {
        self.position = Position::Flat;
        self.cooldown_until_ms = now_ms + ms_minutes(60);
    }

    /// Evaluates position management actions for the current tick.
    ///
    /// `bearish_bias`: when `true` (HTF EMA stack fully bearish), the system
    /// uses tighter trailing (1.5 ATR vs 2.2 ATR) and a shorter time-stop
    /// (1 hour vs 2 hours) so it does not ride a position against the broader
    /// trend for too long.
    pub fn manage_open_position(
        &mut self,
        current_price: f64,
        atr14_5m: f64,
        now_ms: u64,
        bearish_bias: bool,
    ) -> (PositionDecision, String) {
        let atr = atr14_5m.max(1e-12);

        // Trailing ATR multiplier: tighter when HTF is against us.
        let trail_atr_mult = if bearish_bias { 1.5 } else { 2.2 };
        // Time-stop patience: 1 h in bearish HTF, 2 h in normal conditions.
        let time_stop_ms   = if bearish_bias { ms(1) } else { ms(2) };

        match &mut self.position {
            Position::Flat => (PositionDecision::Hold, "No position.".to_string()),
            Position::ExternalInventory { .. } => (
                PositionDecision::Hold,
                "External inventory present. Bot will not manage it automatically.".to_string(),
            ),
            Position::Long {
                entry_price,
                initial_stop_price,
                stop_price,
                tp_price,
                peak_price,
                last_peak_time_ms,
                ..
            } => {
                if let Some(tp) = tp_price {
                    if current_price >= *tp {
                        return (PositionDecision::ExitLong, "Take-profit hit. We exit with glory.".to_string());
                    }
                }
                if current_price > *peak_price {
                    *peak_price = current_price;
                    *last_peak_time_ms = now_ms;
                }

                let r = (*entry_price - *initial_stop_price).max(1e-12);
                // Trail after +1R in normal conditions, after +0.5R when bearish.
                let trail_trigger = if bearish_bias { *entry_price + 0.5 * r } else { *entry_price + r };
                if current_price >= trail_trigger {
                    let trail = *peak_price - trail_atr_mult * atr;
                    if trail > *stop_price {
                        *stop_price = trail;
                    }
                }

                if current_price <= *stop_price {
                    return (PositionDecision::ExitLong, "Stop hit. We exit to protect you.".to_string());
                }

                if now_ms.saturating_sub(*last_peak_time_ms) > time_stop_ms {
                    let reason = if bearish_bias {
                        "HTF bearish — exiting after 1h without new highs. Protecting capital.".to_string()
                    } else {
                        "Too long with no new highs. We exit.".to_string()
                    };
                    return (PositionDecision::ExitLong, reason);
                }

                (PositionDecision::Hold, "We hold the position.".to_string())
            }
        }
    }
}

/// Called when bot_state.json exists but is unrecoverable (unreadable, or
/// invalid JSON with no usable .prev backup). Silently returning a fresh
/// BotState here would wipe live risk-governance memory (position,
/// stop_price, peak_equity, daily-loss anchors, cooldown/hibernation) with no
/// trace - refuse to trade instead, loudly, unless the operator has
/// explicitly opted into accepting a fresh state via BOT_ACCEPT_FRESH_STATE=1
/// (mirrors the existing BOT_REVIVE_DEAD manual-override pattern).
fn unrecoverable_state(starting_usdt: f64, reason: &str, detail: &str) -> Result<BotState> {
    let accept_fresh = matches!(
        std::env::var("BOT_ACCEPT_FRESH_STATE").ok().as_deref(),
        Some("1")
    );
    if accept_fresh {
        tracing::error!(
            "bot_state.json is unrecoverable ({reason}: {detail}). \
             BOT_ACCEPT_FRESH_STATE=1 is set, so proceeding with a fresh state anyway."
        );
        return Ok(BotState::new(starting_usdt));
    }
    tracing::error!(
        "bot_state.json is unrecoverable ({reason}: {detail}) and no usable .prev backup exists. \
         Refusing to trade - a fresh state would silently forget any open position, stop price, \
         and risk-governance memory. Restore bot_state.json from a backup, inspect quarantine/, \
         or set BOT_ACCEPT_FRESH_STATE=1 to explicitly accept starting over."
    );
    Err(anyhow::anyhow!(
        "bot_state.json unrecoverable ({reason}: {detail}); refusing to trade"
    ))
}

pub fn load_or_init(path: &str, starting_usdt: f64) -> Result<BotState> {
    let path = Path::new(path);
    let prev_path = persist::prev_path_for(path);

    // If we previously failed mid-write, prefer restoring the last known-good state.
    if !path.exists() && prev_path.exists() {
        let _ = std::fs::rename(&prev_path, path).or_else(|_| {
            std::fs::copy(&prev_path, path).and_then(|_| std::fs::remove_file(&prev_path))
        });
    }

    if path.exists() {
        let s = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(io_err) => {
                let _ = persist::quarantine_corrupt_file(path, "unreadable");

                // Best-effort recovery from previous snapshot before giving up.
                if prev_path.exists() {
                    if let Ok(prev_s) = fs::read_to_string(&prev_path) {
                        if let Ok(prev_st) = serde_json::from_str::<BotState>(&prev_s) {
                            let _ = save(path.to_string_lossy().as_ref(), &prev_st);
                            return Ok(prev_st);
                        }
                    }
                }

                return unrecoverable_state(starting_usdt, "unreadable", &io_err.to_string());
            }
        };

        let mut st = match serde_json::from_str::<BotState>(&s) {
            Ok(st) => st,
            Err(e) => {
                let _ = persist::quarantine_corrupt_file(path, "invalid_json");

                // Best-effort recovery from previous snapshot.
                if prev_path.exists() {
                    if let Ok(prev_s) = fs::read_to_string(&prev_path) {
                        if let Ok(prev_st) = serde_json::from_str::<BotState>(&prev_s) {
                            let _ = save(path.to_string_lossy().as_ref(), &prev_st);
                            return Ok(prev_st);
                        }
                    }
                }

                return unrecoverable_state(starting_usdt, "invalid_json", &e.to_string());
            }
        };
        if st.starting_usdt <= 0.0 {
            st.starting_usdt = starting_usdt;
        }
        if st.start_equity_usdt <= 0.0 {
            st.start_equity_usdt = st.equity_usdt.max(starting_usdt);
        }
        if st.peak_equity_usdt <= 0.0 {
            st.peak_equity_usdt = st.equity_usdt.max(starting_usdt);
        }
        if st.daily_loss_start_equity_usdt <= 0.0 {
            st.daily_loss_start_equity_usdt = st.equity_usdt.max(starting_usdt);
        }
        if st.day_anchor_yyyymmdd.is_empty() {
            st.day_anchor_yyyymmdd = today_yyyymmdd_utc();
        }
        Ok(st)
    } else {
        Ok(BotState::new(starting_usdt))
    }
}

pub fn save(path: &str, state: &BotState) -> Result<()> {
    let s = serde_json::to_string_pretty(state)?;
    persist::atomic_write_with_prev(Path::new(path), &s)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "binance_survival_bot_state_test_{label}_{}.json",
            now_ms()
        ))
    }

    #[test]
    fn load_or_init_refuses_to_trade_on_corrupt_state_with_no_prev() {
        let path = temp_state_path("corrupt_no_prev");
        std::fs::write(&path, "{ this is not valid json").unwrap();
        unsafe {
            std::env::remove_var("BOT_ACCEPT_FRESH_STATE");
        }

        let result = load_or_init(path.to_str().unwrap(), 100.0);
        assert!(
            result.is_err(),
            "corrupt state with no usable .prev must refuse to trade, not silently return a fresh state"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(persist::prev_path_for(&path));
    }

    #[test]
    fn load_or_init_accepts_fresh_state_when_explicitly_overridden() {
        let path = temp_state_path("corrupt_override");
        std::fs::write(&path, "{ this is not valid json").unwrap();
        unsafe {
            std::env::set_var("BOT_ACCEPT_FRESH_STATE", "1");
        }

        let result = load_or_init(path.to_str().unwrap(), 100.0);

        unsafe {
            std::env::remove_var("BOT_ACCEPT_FRESH_STATE");
        }

        assert!(result.is_ok(), "explicit override must still allow a fresh state");
        assert_eq!(result.unwrap().starting_usdt, 100.0);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(persist::prev_path_for(&path));
    }

    #[test]
    fn load_or_init_still_recovers_from_prev_when_available() {
        let path = temp_state_path("corrupt_with_prev");
        let good = BotState::new(250.0);
        let good_json = serde_json::to_string(&good).unwrap();
        std::fs::write(persist::prev_path_for(&path), &good_json).unwrap();
        std::fs::write(&path, "{ not valid json at all").unwrap();

        let result = load_or_init(path.to_str().unwrap(), 999.0);
        assert!(result.is_ok(), "must recover from .prev before refusing to trade");
        assert_eq!(result.unwrap().starting_usdt, 250.0);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(persist::prev_path_for(&path));
    }

    #[test]
    fn trailing_stop_moves_up_after_1r() {
        let mut st = BotState::new(1000.0);
        st.enter_long(100.0, 1.0, 90.0, None, 0);
        // R = 10, so +1R is 110
        let (_d, _msg) = st.manage_open_position(111.0, 2.0, 1, false);
        match st.position {
            Position::Long { stop_price, .. } => {
                // peak 111, trail = 111 - 4.4 = 106.6
                assert!(stop_price > 90.0);
                assert!((stop_price - 106.6).abs() < 1e-6);
            }
            _ => panic!("expected long"),
        }
    }

    #[test]
    fn time_stop_exits_after_2h_without_new_high() {
        let mut st = BotState::new(1000.0);
        st.enter_long(100.0, 1.0, 90.0, None, 0);
        // no new high, last_peak_time stays at 0
        let (d, _msg) = st.manage_open_position(99.0, 2.0, ms(2) + 1, false);
        assert_eq!(d, PositionDecision::ExitLong);
    }

    #[test]
    fn bearish_bias_exits_after_1h_not_2h() {
        let mut st = BotState::new(1000.0);
        st.enter_long(100.0, 1.0, 90.0, None, 0);
        // 1h + 1ms elapsed, no new high
        let (d, msg) = st.manage_open_position(99.0, 2.0, ms(1) + 1, true);
        assert_eq!(d, PositionDecision::ExitLong);
        assert!(msg.contains("HTF bearish") || msg.contains("bearish"));
    }

    #[test]
    fn bearish_bias_uses_tighter_trail() {
        let mut st = BotState::new(1000.0);
        st.enter_long(100.0, 1.0, 90.0, None, 0);
        // +0.5R trigger = 105, ATR = 2
        let (_d, _msg) = st.manage_open_position(106.0, 2.0, 1, true);
        match st.position {
            Position::Long { stop_price, .. } => {
                // peak 106, trail = 106 - 1.5*2 = 103.0 (tighter than normal 2.2)
                assert!(stop_price > 90.0);
                assert!((stop_price - 103.0).abs() < 1e-6);
            }
            _ => panic!("expected long"),
        }
    }
}
