use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

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

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("bot_state.json");
    parent.join(format!("{file_name}{suffix}"))
}

fn move_file_best_effort(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed creating dir: {}", parent.display()))?;
    }

    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::copy(src, dst)
                .with_context(|| format!("Failed copying {} -> {}", src.display(), dst.display()))?;
            fs::remove_file(src)
                .with_context(|| format!("Failed removing after copy: {}", src.display()))?;
            Ok(())
        }
    }
}

fn quarantine_corrupt_file(path: &Path, reason: &str) -> Result<PathBuf> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let quarantine_dir = parent.join("quarantine");
    fs::create_dir_all(&quarantine_dir)
        .with_context(|| format!("Failed creating quarantine dir: {}", quarantine_dir.display()))?;

    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("bot_state.json");
    let ts = now_ms();
    let pid = std::process::id();
    let dst = quarantine_dir.join(format!("{file_name}.corrupt.{reason}.{ts}.{pid}"));
    move_file_best_effort(path, &dst)?;
    Ok(dst)
}

fn atomic_write_with_prev(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed creating state dir: {}", parent.display()))?;
    }

    let ts = now_ms();
    let pid = std::process::id();
    let tmp_path = path_with_suffix(path, &format!(".tmp.{ts}.{pid}"));

    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .with_context(|| format!("Failed opening temp state file: {}", tmp_path.display()))?;
        f.write_all(contents.as_bytes())
            .with_context(|| format!("Failed writing temp state file: {}", tmp_path.display()))?;
        f.sync_all().with_context(|| {
            format!(
                "Failed syncing temp state file to disk: {}",
                tmp_path.display()
            )
        })?;
    }

    let prev_path = path_with_suffix(path, ".prev");
    if path.exists() {
        let _ = fs::remove_file(&prev_path);
        move_file_best_effort(path, &prev_path)
            .with_context(|| format!("Failed moving old state to prev: {}", prev_path.display()))?;
    }

    if let Err(e) = move_file_best_effort(&tmp_path, path) {
        // Best effort rollback.
        let _ = fs::remove_file(&tmp_path);
        if !path.exists() && prev_path.exists() {
            let _ = move_file_best_effort(&prev_path, path);
        }
        return Err(e);
    }

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Position {
    Flat,
    Long {
        entry_price: f64,
        qty: f64,
        initial_stop_price: f64,
        stop_price: f64,
        peak_price: f64,
        entry_time_ms: u64,
        last_peak_time_ms: u64,
        #[serde(default)]
        stop_order_id: Option<u64>,
    },
}

impl Default for Position {
    fn default() -> Self {
        Position::Flat
    }
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

    // Current live fields
    pub equity_usdt: f64,
    pub fees_usdt: f64,
    pub is_dead: bool,
    pub peak_equity_usdt: f64,
    pub position: Position,
    pub cooldown_until_ms: u64,
    pub hibernation_until_ms: u64,
    pub daily_loss_start_equity_usdt: f64,
    pub day_anchor_yyyymmdd: String,
    pub trades_today: u32,
}

impl Default for BotState {
    fn default() -> Self {
        let day = today_yyyymmdd_utc();
        Self {
            starting_usdt: 0.0,
            realized_pnl_usdt: 0.0,
            equity_usdt: 0.0,
            fees_usdt: 0.0,
            is_dead: false,
            peak_equity_usdt: 0.0,
            position: Position::Flat,
            cooldown_until_ms: 0,
            hibernation_until_ms: 0,
            daily_loss_start_equity_usdt: 0.0,
            day_anchor_yyyymmdd: day,
            trades_today: 0,
        }
    }
}

impl BotState {
    pub fn new(starting_usdt: f64) -> Self {
        let day = today_yyyymmdd_utc();
        Self {
            starting_usdt,
            realized_pnl_usdt: 0.0,
            equity_usdt: starting_usdt,
            fees_usdt: 0.0,
            is_dead: false,
            peak_equity_usdt: starting_usdt,
            position: Position::Flat,
            cooldown_until_ms: 0,
            hibernation_until_ms: 0,
            daily_loss_start_equity_usdt: starting_usdt,
            day_anchor_yyyymmdd: day,
            trades_today: 0,
        }
    }

    pub fn sync_equity_and_day(&mut self, equity_usdt: f64) {
        self.equity_usdt = equity_usdt;
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

    pub fn apply_fee(&mut self, fee_usdt: f64) {
        self.fees_usdt += fee_usdt;
        self.equity_usdt -= fee_usdt;
        if self.equity_usdt <= 0.0 {
            self.is_dead = true;
        }
    }

    pub fn enter_long(&mut self, entry_price: f64, qty: f64, stop_price: f64, now_ms: u64) {
        self.position = Position::Long {
            entry_price,
            qty,
            initial_stop_price: stop_price,
            stop_price,
            peak_price: entry_price,
            entry_time_ms: now_ms,
            last_peak_time_ms: now_ms,
            stop_order_id: None,
        };
    }

    pub fn set_stop_order_id(&mut self, order_id: u64) {
        if let Position::Long { stop_order_id, .. } = &mut self.position {
            *stop_order_id = Some(order_id);
        }
    }

    pub fn exit_to_flat_with_cooldown(&mut self, now_ms: u64) {
        self.position = Position::Flat;
        self.cooldown_until_ms = now_ms + ms_minutes(60);
    }

    pub fn manage_open_position(&mut self, current_price: f64, atr14_5m: f64, now_ms: u64) -> (PositionDecision, String) {
        let atr = atr14_5m.max(1e-12);
        match &mut self.position {
            Position::Flat => (PositionDecision::Hold, "No position.".to_string()),
            Position::Long {
                entry_price,
                initial_stop_price,
                stop_price,
                peak_price,
                last_peak_time_ms,
                ..
            } => {
                if current_price > *peak_price {
                    *peak_price = current_price;
                    *last_peak_time_ms = now_ms;
                }

                let r = (*entry_price - *initial_stop_price).max(1e-12);
                if current_price >= *entry_price + r {
                    let trail = *peak_price - 2.2 * atr;
                    if trail > *stop_price {
                        *stop_price = trail;
                    }
                }

                if current_price <= *stop_price {
                    return (PositionDecision::ExitLong, "Stop hit. We exit to protect you.".to_string());
                }

                if now_ms.saturating_sub(*last_peak_time_ms) > ms(2) {
                    return (PositionDecision::ExitLong, "Too long with no new highs. We exit.".to_string());
                }

                (PositionDecision::Hold, "We hold the position.".to_string())
            }
        }
    }
}

pub fn load_or_init(path: &str, starting_usdt: f64) -> Result<BotState> {
    let path = Path::new(path);
    let prev_path = path_with_suffix(path, ".prev");

    // If we previously failed mid-write, prefer restoring the last known-good state.
    if !path.exists() && prev_path.exists() {
        let _ = move_file_best_effort(&prev_path, path);
    }

    if path.exists() {
        let s = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => {
                let _ = quarantine_corrupt_file(path, "unreadable");
                return Ok(BotState::new(starting_usdt));
            }
        };

        let mut st = match serde_json::from_str::<BotState>(&s) {
            Ok(st) => st,
            Err(e) => {
                let _ = quarantine_corrupt_file(path, "invalid_json");

                // Best-effort recovery from previous snapshot.
                if prev_path.exists() {
                    if let Ok(prev_s) = fs::read_to_string(&prev_path) {
                        if let Ok(prev_st) = serde_json::from_str::<BotState>(&prev_s) {
                            let _ = save(path.to_string_lossy().as_ref(), &prev_st);
                            return Ok(prev_st);
                        }
                    }
                }

                let _ = e;
                return Ok(BotState::new(starting_usdt));
            }
        };
        if st.starting_usdt <= 0.0 {
            st.starting_usdt = starting_usdt;
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
    atomic_write_with_prev(Path::new(path), &s)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_stop_moves_up_after_1r() {
        let mut st = BotState::new(1000.0);
        st.enter_long(100.0, 1.0, 90.0, 0);
        // R = 10, so +1R is 110
        let (_d, _msg) = st.manage_open_position(111.0, 2.0, 1);
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
        st.enter_long(100.0, 1.0, 90.0, 0);
        // no new high, last_peak_time stays at 0
        let (d, _msg) = st.manage_open_position(99.0, 2.0, ms(2) + 1);
        assert_eq!(d, PositionDecision::ExitLong);
    }
}
