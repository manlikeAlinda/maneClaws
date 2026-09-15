//! Regression test for the order-placement idempotency fix (audit finding 2.1):
//! if a `place_order` POST appears to fail (connection dropped before a
//! response arrives) but Binance actually received and filled it, the client
//! must reconcile via `newClientOrderId`/`origClientOrderId` and report the
//! real order rather than letting a caller retry into a duplicate order.

use binance_survival_bot::binance_orders;
use reqwest::Client;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn http_response_json(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.as_bytes().len(),
        body
    )
}

/// Starts a stub Binance server where the first POST /api/v3/order is
/// accepted and read, then the connection is dropped with no response
/// (simulating a lost response after Binance actually placed the order), and
/// any GET /api/v3/order?...origClientOrderId=... lookup reports that order
/// as filled.
async fn start_stub_server(
    order_actually_placed: bool,
) -> (tokio::task::JoinHandle<()>, SocketAddr, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let post_count = Arc::new(AtomicUsize::new(0));
    let post_count_bg = post_count.clone();

    let handle = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
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
            let parts: Vec<&str> = first.split_whitespace().collect();
            let method = parts.first().copied().unwrap_or("");
            let path = parts.get(1).copied().unwrap_or("/");

            if method == "POST" && path.starts_with("/api/v3/order?") {
                post_count_bg.fetch_add(1, Ordering::SeqCst);
                // Drop the connection with no response at all: the order was
                // "placed" from Binance's perspective, but the client never
                // sees a response for this attempt.
                drop(socket);
                continue;
            }

            use tokio::io::AsyncWriteExt;
            let response = if path.starts_with("/api/v3/time") {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis();
                http_response_json(&format!("{{\"serverTime\":{now_ms}}}")).into_bytes()
            } else if method == "GET" && path.contains("origClientOrderId=") {
                if order_actually_placed {
                    let body = r#"{
                        "symbol":"BTCUSDT",
                        "orderId":999888777,
                        "status":"FILLED",
                        "executedQty":"0.001",
                        "cummulativeQuoteQty":"50.00"
                    }"#;
                    http_response_json(body).into_bytes()
                } else {
                    use tokio::io::AsyncWriteExt;
                    let body = r#"{"code":-2013,"msg":"Order does not exist."}"#;
                    let resp = format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.as_bytes().len(),
                        body
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                    continue;
                }
            } else {
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                continue;
            };

            let _ = socket.write_all(&response).await;
        }
    });

    (handle, addr, post_count)
}

#[tokio::test]
async fn place_order_reconciles_instead_of_reporting_failure_when_order_actually_placed() {
    unsafe {
        std::env::set_var("BOT_HTTP_TIMEOUT_SECS", "1");
        std::env::set_var("BOT_HTTP_MAX_RETRIES", "0");
    }

    let (_h, addr, post_count) = start_stub_server(true).await;
    let base = format!("http://{addr}");
    let client = Client::new();

    let result = binance_orders::place_order(
        &client,
        "test-key",
        "test-secret",
        &base,
        "BTCUSDT",
        "symbol=BTCUSDT&side=BUY&type=MARKET&quantity=0.001",
    )
    .await;

    unsafe {
        std::env::remove_var("BOT_HTTP_TIMEOUT_SECS");
        std::env::remove_var("BOT_HTTP_MAX_RETRIES");
    }

    assert_eq!(
        post_count.load(Ordering::SeqCst),
        1,
        "expected exactly one POST attempt (no blind retry that could double-place)"
    );

    let ack = result.expect(
        "place_order should reconcile via origClientOrderId and report success, not error out",
    );
    assert_eq!(ack.order_id, 999888777);
}

#[tokio::test]
async fn place_order_still_reports_failure_when_order_was_never_placed() {
    unsafe {
        std::env::set_var("BOT_HTTP_TIMEOUT_SECS", "1");
        std::env::set_var("BOT_HTTP_MAX_RETRIES", "0");
    }

    let (_h, addr, _post_count) = start_stub_server(false).await;
    let base = format!("http://{addr}");
    let client = Client::new();

    let result = binance_orders::place_order(
        &client,
        "test-key",
        "test-secret",
        &base,
        "BTCUSDT",
        "symbol=BTCUSDT&side=BUY&type=MARKET&quantity=0.001",
    )
    .await;

    unsafe {
        std::env::remove_var("BOT_HTTP_TIMEOUT_SECS");
        std::env::remove_var("BOT_HTTP_MAX_RETRIES");
    }

    assert!(
        result.is_err(),
        "reconciliation confirmed no such order exists - must still report the original failure, \
         not silently claim success"
    );
}
