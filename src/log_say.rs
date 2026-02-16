use crate::execution::Mode;
use crate::state::Position;
use tracing::info;

pub fn say_start(mode: Mode, run_id: &str) {
    info!("Start: mode={:?} run_id={}", mode, run_id);
}

pub fn say_connected(ok: bool) {
    if ok {
        info!("Connected to Binance.");
    } else {
        info!("Not connected to Binance.");
    }
}

pub fn say_price(price: f64) {
    info!("BTC price: {:.2} USDT", price);
}

pub fn say_wallet(usdt_free: f64, btc_free: f64, equity_usdt: f64) {
    info!(
        "Wallet: USDT_free={:.2} BTC_free={:.8} Equity≈{:.2} USDT",
        usdt_free, btc_free, equity_usdt
    );
}

pub fn say_state(pos: &Position) {
    match pos {
        Position::Flat => info!("State: Flat."),
        Position::Long { qty, entry_price, stop_price, .. } => info!(
            "State: Long qty={:.8} entry={:.2} stop={:.2}",
            qty, entry_price, stop_price
        ),
        Position::ExternalInventory { btc_qty, .. } => {
            info!("State: ExternalInventory btc_qty={:.8}", btc_qty)
        }
    }
}

pub fn say_action(msg: &str) {
    info!("{}", msg);
}

pub fn say_reason(msg: &str) {
    info!("Reason: {}", msg);
}

pub fn say_decision_with_reason(mode: Mode, decision: &str, reason: &str) {
    if reason.trim().is_empty() {
        info!("Decision ({:?}): {}", mode, decision.trim());
    } else {
        info!("Decision ({:?}): {} — {}", mode, decision.trim(), reason.trim());
    }
}

pub fn say_end(end: &str) {
    info!("End: {}", end.trim());
}
