use binance_survival_bot::app::{self, AppConfig};
use reqwest::Client;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

type SharedLog = Arc<Mutex<Vec<String>>>;

fn http_response_json(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.as_bytes().len(),
        body
    )
}

fn http_response_status(code: u16, body: &str) -> String {
    let status = match code {
        200 => "200 OK",
        400 => "400 Bad Request",
        401 => "401 Unauthorized",
        404 => "404 Not Found",
        500 => "500 Internal Server Error",
        _ => "200 OK",
    };
    format!(
        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        body.as_bytes().len(),
        body
    )
}

fn build_klines(count: usize, start_ms: i64, step_ms: i64, start_price: f64, step_price: f64) -> String {
    // Produce kline rows: [openTime, open, high, low, close, volume, ...]
    let mut rows = Vec::with_capacity(count);
    for i in 0..count {
        let t = start_ms + (i as i64) * step_ms;
        let p = start_price + (i as f64) * step_price;
        let open = p;
        let close = p + step_price * 0.5;
        let high = close + 1.0;
        let low = open - 1.0;
        let volume = 100.0 + i as f64;
        rows.push(format!(
            "[{t},\"{open:.2}\",\"{high:.2}\",\"{low:.2}\",\"{close:.2}\",\"{volume:.2}\",{t},\"0\",0,\"0\",\"0\",\"0\"]"
        ));
    }
    format!("[{}]", rows.join(","))
}

async fn start_stub_server(log: SharedLog) -> (tokio::task::JoinHandle<()>, SocketAddr) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => return,
            };

            let mut buf = vec![0u8; 8192];
            let n = match socket.read(&mut buf).await {
                Ok(n) => n,
                Err(_) => 0,
            };
            if n == 0 {
                continue;
            }
            buf.truncate(n);
            let req = String::from_utf8_lossy(&buf).to_string();
            let first = req.lines().next().unwrap_or("");

            // Example: GET /api/v3/ping HTTP/1.1
            let parts: Vec<&str> = first.split_whitespace().collect();
            let path = parts.get(1).copied().unwrap_or("/");

            {
                let mut lg = log.lock().unwrap();
                lg.push(path.to_string());
            }

            let response = if path.starts_with("/api/v3/ping") {
                http_response_json("{}").into_bytes()
            } else if path.starts_with("/api/v3/ticker/price") {
                http_response_json("{\"price\":\"480.00\"}").into_bytes()
            } else if path.starts_with("/api/v3/account") {
                // Provide balances so BUY is possible.
                let body = r#"{
                    "balances": [
                        {"asset":"USDT","free":"1000.00","locked":"0"},
                        {"asset":"BTC","free":"0.00000000","locked":"0"}
                    ]
                }"#;
                http_response_json(body).into_bytes()
            } else if path.starts_with("/api/v3/exchangeInfo") {
                let body = r#"{
                    "symbols": [
                        {
                            "symbol":"BTCUSDT",
                            "status":"TRADING",
                            "baseAsset":"BTC",
                            "quoteAsset":"USDT",
                            "filters":[
                                {"filterType":"LOT_SIZE","minQty":"0.00001","maxQty":"9000","stepSize":"0.00001"},
                                {"filterType":"PRICE_FILTER","minPrice":"0.01","maxPrice":"10000000","tickSize":"0.01"},
                                {"filterType":"MIN_NOTIONAL","minNotional":"5.0"}
                            ]
                        }
                    ]
                }"#;
                http_response_json(body).into_bytes()
            } else if path.starts_with("/api/v3/klines") {
                // Return candles that create a trending + breakout scenario.
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as i64;
                
                let spacing_ms = if path.contains("interval=1m") {
                    60_000
                } else if path.contains("interval=5m") {
                    300_000
                } else {
                    3_600_000
                };

                let start_ms = now_ms - (200i64 * spacing_ms);
                let body = build_klines(200, start_ms, spacing_ms, 80.0, 2.0);
                http_response_json(&body).into_bytes()
            } else if path.starts_with("/api/v3/order/test") {
                http_response_json("{}").into_bytes()
            } else if path.starts_with("/api/v3/order") {
                // PRACTICE must never hit this.
                http_response_status(500, "{\"msg\":\"LIVE ORDER CALLED\"}").into_bytes()
            } else {
                http_response_status(404, "{\"msg\":\"not found\"}").into_bytes()
            };

            let _ = socket.write_all(&response).await;
        }
    });

    (handle, addr)
}

#[tokio::test]
async fn practice_mode_never_calls_live_order_endpoint() {
    // Force PRACTICE; lower score threshold so synthetic breakout clears the bar.
    unsafe {
        std::env::remove_var("BOT_LIVE_TRADING");
        std::env::set_var("SIGNAL_MIN_SCORE", "0.15");
    }

    let log: SharedLog = Arc::new(Mutex::new(Vec::new()));
    let (_h, addr) = start_stub_server(log.clone()).await;

    let tmp = std::env::temp_dir().join(format!(
        "binance_survival_bot_state_{}.json",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));

    let cache_dir = std::env::temp_dir().join(format!(
        "binance_survival_bot_cache_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));

    let cfg = AppConfig {
        base_url: format!("http://{}", addr),
        symbol: "BTCUSDT".to_string(),
        state_path: tmp.to_string_lossy().to_string(),
        data_dir: cache_dir.to_string_lossy().to_string(),
        candle_cache_max_age: Duration::from_secs(0),
        api_key: "k".repeat(32),
        api_secret: "s".repeat(32),
    };

    // Ensure no existing state file interferes.
    if tmp.exists() {
        let _ = std::fs::remove_file(&tmp);
    }

    let client = Client::new();
    let snap = binance_survival_bot::dashboard::new_shared_snapshot();
    let out = app::run_once(&client, &cfg, &snap).await.unwrap();
    assert_eq!(out.mode, binance_survival_bot::execution::Mode::Practice);

    let calls = log.lock().unwrap().clone();

    let called_live = calls.iter().any(|p| p.starts_with("/api/v3/order?") || p == "/api/v3/order");
    assert!(!called_live, "PRACTICE called live order endpoint: {calls:?}");

    let called_test = calls.iter().any(|p| p.starts_with("/api/v3/order/test"));
    assert!(called_test, "Expected test order call, got: {calls:?}");

    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_dir_all(&cache_dir);
}
