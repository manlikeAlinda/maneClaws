use crate::binance_orders;
use crate::order;
use anyhow::{anyhow, Result};
use reqwest::Client;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Practice,
    Live,
}

pub fn is_live_trading_enabled(value: Option<&str>) -> bool {
    matches!(value, Some("1"))
}

pub fn is_live_confirmed(value: Option<&str>) -> bool {
    matches!(value.map(|s| s.trim()), Some("YES"))
}

pub fn is_live_trading_armed_from_env() -> bool {
    let live = std::env::var("BOT_LIVE_TRADING").ok();
    let confirm = std::env::var("BOT_LIVE_CONFIRM").ok();
    is_live_trading_enabled(live.as_deref()) && is_live_confirmed(confirm.as_deref())
}

pub fn mode_from_env() -> Mode {
    if is_live_trading_armed_from_env() {
        Mode::Live
    } else {
        Mode::Practice
    }
}

pub fn mode_log_line(mode: Mode) -> &'static str {
    match mode {
        Mode::Practice => "Mode: PRACTICE (no money moves).",
        Mode::Live => "Mode: LIVE (real money).",
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn execute_buy_market(
    client: &Client,
    mode: Mode,
    api_key: &str,
    api_secret: &str,
    base: &str,
    symbol: &str,
    qty: f64,
    qty_precision: usize,
) -> Result<Option<u64>> {
    if qty <= 0.0 {
        return Err(anyhow!("Refusing to buy: qty <= 0"));
    }

    let o = order::NewOrder {
        symbol,
        side: "BUY",
        order_type: "MARKET",
        quantity: format!("{qty:.p$}", p = qty_precision),
    };
    let qs = o.to_query_string();

    match mode {
        Mode::Practice => {
            binance_orders::test_order(client, api_key, api_secret, base, &qs).await?;
            Ok(None)
        }
        Mode::Live => {
            let ack = binance_orders::place_order(client, api_key, api_secret, base, symbol, &qs).await?;
            Ok(Some(ack.order_id))
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn execute_sell_market(
    client: &Client,
    mode: Mode,
    api_key: &str,
    api_secret: &str,
    base: &str,
    symbol: &str,
    qty: f64,
    qty_precision: usize,
) -> Result<Option<u64>> {
    if qty <= 0.0 {
        return Err(anyhow!("Refusing to sell: qty <= 0"));
    }

    let o = order::NewOrder {
        symbol,
        side: "SELL",
        order_type: "MARKET",
        quantity: format!("{qty:.p$}", p = qty_precision),
    };
    let qs = o.to_query_string();

    match mode {
        Mode::Practice => {
            binance_orders::test_order(client, api_key, api_secret, base, &qs).await?;
            Ok(None)
        }
        Mode::Live => {
            let ack = binance_orders::place_order(client, api_key, api_secret, base, symbol, &qs).await?;
            Ok(Some(ack.order_id))
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn place_stop_loss_limit_sell(
    client: &Client,
    mode: Mode,
    api_key: &str,
    api_secret: &str,
    base: &str,
    symbol: &str,
    qty: f64,
    qty_precision: usize,
    stop_price: f64,
    limit_price: f64,
    price_precision: usize,
) -> Result<Option<u64>> {
    if qty <= 0.0 {
        return Err(anyhow!("Refusing to place stop: qty <= 0"));
    }
    if stop_price <= 0.0 || limit_price <= 0.0 {
        return Err(anyhow!("Refusing to place stop: bad prices"));
    }
    if limit_price > stop_price {
        return Err(anyhow!("Refusing to place stop: limit_price > stop_price"));
    }

    // Binance Spot supports STOP_LOSS_LIMIT with stopPrice + price + timeInForce.
    let qs = format!(
        "symbol={}&side=SELL&type=STOP_LOSS_LIMIT&timeInForce=GTC&quantity={:.qp$}&stopPrice={:.pp$}&price={:.pp$}",
        symbol,
        qty,
        stop_price,
        limit_price,
        qp = qty_precision,
        pp = price_precision
    );

    match mode {
        Mode::Practice => {
            binance_orders::test_order(client, api_key, api_secret, base, &qs).await?;
            Ok(None)
        }
        Mode::Live => {
            let ack = binance_orders::place_order(client, api_key, api_secret, base, symbol, &qs).await?;
            Ok(Some(ack.order_id))
        }
    }
}

pub async fn cancel_order(
    client: &Client,
    mode: Mode,
    api_key: &str,
    api_secret: &str,
    base: &str,
    symbol: &str,
    order_id: u64,
) -> Result<()> {
    match mode {
        Mode::Practice => Ok(()),
        Mode::Live => {
            let _ = binance_orders::cancel_order(client, api_key, api_secret, base, symbol, order_id)
                .await?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_flag_only_when_one() {
        assert!(!is_live_trading_enabled(None));
        assert!(!is_live_trading_enabled(Some("0")));
        assert!(!is_live_trading_enabled(Some("yes")));
        assert!(is_live_trading_enabled(Some("1")));
    }

    #[test]
    fn live_confirm_only_when_yes() {
        assert!(!is_live_confirmed(None));
        assert!(!is_live_confirmed(Some("")));
        assert!(!is_live_confirmed(Some("yes")));
        assert!(is_live_confirmed(Some(" YES ")));
        assert!(is_live_confirmed(Some("YES")));
    }

    #[test]
    fn mode_log_is_plain() {
        assert!(mode_log_line(Mode::Practice).contains("PRACTICE"));
        assert!(mode_log_line(Mode::Live).contains("LIVE"));
    }
}
