use crate::execution::Mode;
use crate::state::Position;
use tracing::{debug, info};
use std::io::Write;

// Emit a light-weight JSON line for external analyzers to consume. This complements the
// human-friendly `info!` lines above. We write both a debug-prefixed line and append a
// `decision.jsonl` record so tools can reliably parse structured events from the log directory.

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
        "Wallet: USDT balance={:.2}, BTC={:.8}, Total Equity={:.2} USDT",
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
        info!("Decision ({:?}): {} -> {}", mode, decision.trim(), reason.trim());
    }

    // Structured JSON for analyzers
    let obj = serde_json::json!({
        "event": "decision",
        "mode": format!("{:?}", mode),
        "decision": decision.trim(),
        "reason": reason.trim()
    });
    let line = obj.to_string();
    debug!("DECISION_JSON {}", line);
    if let Ok(mut fh) = std::fs::OpenOptions::new().create(true).append(true).open("decision.jsonl") {
        let _ = writeln!(fh, "{}", line);
    }
}

pub fn say_end(end: &str) {
    info!("End: {}", end.trim());
}
