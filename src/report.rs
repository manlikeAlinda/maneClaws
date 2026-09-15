//! Self-contained HTML report generator.
//!
//! `generate_html(&report)` returns a single HTML string that can be written
//! to a `.html` file and opened in any browser — no internet connection needed.
//! The page embeds all data as an inline JSON block and renders:
//!
//! - Summary metrics table
//! - Equity + drawdown curves (SVG line charts)
//! - Score-bucket calibration bar chart (SVG)
//! - Regime breakdown table
//! - Trade ledger (last 200 trades)
//!
//! `write_report(report, path)` is a convenience wrapper that calls
//! `generate_html` and writes the result to disk.

use crate::backtest::{BacktestReport, ScoreBucket};
use std::fmt::Write as FmtWrite;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn generate_html(report: &BacktestReport) -> String {
    let mut html = String::with_capacity(64 * 1024);

    let _ = write!(html, r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Backtest Report</title>
<style>
*{{box-sizing:border-box;margin:0;padding:0}}
body{{font-family:monospace;background:#0d1117;color:#c9d1d9;padding:24px}}
h1{{font-size:1.4rem;margin-bottom:20px;color:#58a6ff}}
h2{{font-size:1.1rem;margin:28px 0 12px;color:#79c0ff;border-bottom:1px solid #30363d;padding-bottom:6px}}
.metrics-grid{{display:grid;grid-template-columns:repeat(auto-fill,minmax(200px,1fr));gap:12px;margin-bottom:8px}}
.metric{{background:#161b22;border:1px solid #30363d;border-radius:6px;padding:12px}}
.metric-label{{font-size:0.75rem;color:#8b949e;margin-bottom:4px}}
.metric-value{{font-size:1.25rem;font-weight:bold}}
.pos{{color:#3fb950}}.neg{{color:#f85149}}.neu{{color:#e3b341}}
table{{width:100%;border-collapse:collapse;font-size:0.78rem}}
th{{background:#161b22;color:#8b949e;padding:6px 8px;text-align:left;border-bottom:1px solid #30363d}}
td{{padding:5px 8px;border-bottom:1px solid #21262d}}
tr:hover td{{background:#161b22}}
.chart-wrap{{background:#161b22;border:1px solid #30363d;border-radius:6px;padding:16px;margin-bottom:16px}}
svg text{{font-family:monospace;font-size:10px;fill:#8b949e}}
</style>
</head>
<body>
<h1>Backtest Report</h1>
"#);

    // --- Summary metrics ---
    let m = &report.metrics;
    let return_class = if m.total_return_pct >= 0.0 { "pos" } else { "neg" };
    let dd_class = if m.max_drawdown_pct > 15.0 { "neg" } else if m.max_drawdown_pct > 8.0 { "neu" } else { "pos" };
    let pf_class = if m.profit_factor >= 1.5 { "pos" } else if m.profit_factor >= 1.0 { "neu" } else { "neg" };
    let sh_class = if m.sharpe_ratio >= 1.0 { "pos" } else if m.sharpe_ratio >= 0.0 { "neu" } else { "neg" };

    let _ = write!(html, r#"<h2>Summary</h2><div class="metrics-grid">"#);
    metric_card(&mut html, "Total Return", &format!("{:+.2}%", m.total_return_pct), return_class);
    metric_card(&mut html, "Max Drawdown", &format!("{:.2}%", m.max_drawdown_pct), dd_class);
    metric_card(&mut html, "Sharpe Ratio", &format!("{:.2}", m.sharpe_ratio), sh_class);
    metric_card(&mut html, "Profit Factor", &format_pf(m.profit_factor), pf_class);
    metric_card(&mut html, "Win Rate", &format!("{:.1}%", m.win_rate * 100.0), "neu");
    metric_card(&mut html, "Trades", &m.trade_count.to_string(), "neu");
    metric_card(&mut html, "Avg Winner", &format!("{:+.2}%", m.avg_winner_pct), "pos");
    metric_card(&mut html, "Avg Loser", &format!("{:+.2}%", m.avg_loser_pct), "neg");
    metric_card(&mut html, "Final Equity", &format!("${:.2}", m.final_equity), return_class);
    metric_card(&mut html, "Total Fees", &format!("${:.2}", m.total_fees_usdt), "neg");
    metric_card(&mut html, "Total Slippage", &format!("${:.2}", m.total_slippage_usdt), "neg");
    let avg_dur_min = m.avg_trade_duration_ms / 60_000;
    metric_card(&mut html, "Avg Duration", &format!("{avg_dur_min} min"), "neu");
    let _ = write!(html, "</div>");

    // --- Equity curve ---
    html.push_str(r#"<h2>Equity Curve</h2>"#);
    html.push_str(&render_equity_svg(&report.equity_curve, 780, 200, "#58a6ff"));

    // --- Drawdown curve ---
    html.push_str(r#"<h2>Drawdown</h2>"#);
    // Drawdown is a positive percentage of loss from peak — invert for display.
    let dd_points: Vec<(u64, f64)> = report.drawdown_curve.iter().map(|(t, d)| (*t, -*d)).collect();
    html.push_str(&render_equity_svg(&dd_points, 780, 140, "#f85149"));

    // --- Score bucket calibration ---
    html.push_str(r#"<h2>Score Bucket Calibration</h2>"#);
    html.push_str(&render_score_buckets_svg(&report.score_buckets, 780, 200));

    // --- Regime stats table ---
    html.push_str(r#"<h2>Regime Performance</h2>"#);
    html.push_str(r#"<table><tr><th>Regime</th><th>Trades</th><th>Win Rate</th><th>Avg Return</th><th>Expectancy</th><th>Profit Factor</th></tr>"#);
    for rs in &report.regime_stats {
        let wr_class = if rs.win_rate >= 0.5 { "pos" } else { "neg" };
        let _ = write!(html,
            "<tr><td>{}</td><td>{}</td><td class=\"{}\">{:.1}%</td><td class=\"{}\">{:+.2}%</td><td class=\"{}\">{:+.2}%</td><td class=\"{}\">{}</td></tr>",
            rs.regime, rs.trades,
            wr_class, rs.win_rate * 100.0,
            if rs.avg_return_pct >= 0.0 { "pos" } else { "neg" }, rs.avg_return_pct,
            if rs.expectancy_pct >= 0.0 { "pos" } else { "neg" }, rs.expectancy_pct,
            pf_class, format_pf(rs.profit_factor),
        );
    }
    html.push_str("</table>");

    // --- Score bucket table ---
    html.push_str(r#"<h2>Score Bucket Details</h2>"#);
    html.push_str(r#"<table><tr><th>Score Range</th><th>Trades</th><th>Win Rate</th><th>Avg Return</th><th>Expectancy</th><th>Profit Factor</th></tr>"#);
    for b in &report.score_buckets {
        if b.count == 0 { continue; }
        let wr_class = if b.win_rate >= 0.5 { "pos" } else { "neg" };
        let _ = write!(html,
            "<tr><td>{}</td><td>{}</td><td class=\"{}\">{:.1}%</td><td class=\"{}\">{:+.2}%</td><td class=\"{}\">{:+.2}%</td><td>{}</td></tr>",
            b.label, b.count,
            wr_class, b.win_rate * 100.0,
            if b.avg_return_pct >= 0.0 { "pos" } else { "neg" }, b.avg_return_pct,
            if b.expectancy_pct >= 0.0 { "pos" } else { "neg" }, b.expectancy_pct,
            format_pf(b.profit_factor),
        );
    }
    html.push_str("</table>");

    // --- Trade ledger (last 200) ---
    html.push_str(r#"<h2>Trade Ledger (last 200)</h2>"#);
    html.push_str(r#"<table><tr><th>#</th><th>Regime</th><th>Strategy</th><th>Score</th>
<th>Entry</th><th>Exit</th><th>Net PnL</th><th>Return</th><th>MFE</th><th>MAE</th><th>Duration</th><th>Exit Reason</th></tr>"#);

    let trades_to_show: Vec<_> = report.trades.iter().rev().take(200).collect();
    for (idx, t) in trades_to_show.iter().enumerate() {
        let net = t.net_pnl_usdt.unwrap_or(0.0);
        let ret_pct = if t.position_value_usdt > 0.0 { net / t.position_value_usdt * 100.0 } else { 0.0 };
        let pnl_class = if net >= 0.0 { "pos" } else { "neg" };
        let dur_min = t.duration_ms.unwrap_or(0) / 60_000;
        let mfe_str = t.mfe_usdt.map_or("-".to_string(), |v| format!("${:.2}", v));
        let mae_str = t.mae_usdt.map_or("-".to_string(), |v| format!("${:.2}", v));
        let exit_reason = t.exit_reason.as_deref().unwrap_or("-");
        let _ = write!(html,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{:.3}</td><td>{:.2}</td><td>{}</td>\
             <td class=\"{}\">{}</td><td class=\"{}\">{:+.2}%</td><td>{}</td><td>{}</td>\
             <td>{} min</td><td>{}</td></tr>",
            idx + 1,
            t.regime, t.strategy, t.score,
            t.entry_price,
            t.exit_price.map_or("-".to_string(), |p| format!("{p:.2}")),
            pnl_class, format_usdt(net),
            pnl_class, ret_pct,
            mfe_str, mae_str,
            dur_min,
            exit_reason,
        );
    }
    html.push_str("</table>");

    html.push_str("\n</body></html>");
    html
}

pub fn write_report(report: &BacktestReport, path: &str) -> std::io::Result<()> {
    let html = generate_html(report);
    std::fs::write(path, html)
}

// ---------------------------------------------------------------------------
// SVG chart helpers
// ---------------------------------------------------------------------------

fn render_equity_svg(points: &[(u64, f64)], width: usize, height: usize, stroke: &str) -> String {
    if points.len() < 2 {
        return format!(
            r#"<div class="chart-wrap"><svg width="{width}" height="{height}"><text x="10" y="20">No data</text></svg></div>"#
        );
    }

    let pad_l = 60usize;
    let pad_r = 20usize;
    let pad_t = 16usize;
    let pad_b = 28usize;
    let w = width - pad_l - pad_r;
    let h = height - pad_t - pad_b;

    let min_y = points.iter().map(|(_, v)| *v).fold(f64::INFINITY, f64::min);
    let max_y = points.iter().map(|(_, v)| *v).fold(f64::NEG_INFINITY, f64::max);
    let range_y = (max_y - min_y).max(1e-12);
    let min_x = points.first().unwrap().0 as f64;
    let max_x = points.last().unwrap().0 as f64;
    let range_x = (max_x - min_x).max(1.0);

    let to_px = |ts: u64, val: f64| -> (f64, f64) {
        let px = pad_l as f64 + (ts as f64 - min_x) / range_x * w as f64;
        let py = pad_t as f64 + (1.0 - (val - min_y) / range_y) * h as f64;
        (px, py)
    };

    let mut path = String::new();
    for (i, &(ts, val)) in points.iter().enumerate() {
        let (px, py) = to_px(ts, val);
        if i == 0 {
            let _ = write!(path, "M {px:.1} {py:.1}");
        } else {
            let _ = write!(path, " L {px:.1} {py:.1}");
        }
    }

    // Y-axis labels (5 ticks).
    let mut y_labels = String::new();
    for i in 0..=4 {
        let v = min_y + range_y * i as f64 / 4.0;
        let py = pad_t as f64 + (1.0 - i as f64 / 4.0) * h as f64;
        let label = if max_y.abs() > 100.0 { format!("{v:.0}") } else { format!("{v:.2}") };
        let _ = write!(
            y_labels,
            r#"<text x="{}" y="{:.1}" text-anchor="end">{}</text>"#,
            pad_l - 4, py + 4.0, label
        );
        let _ = write!(
            y_labels,
            r##"<line x1="{}" y1="{:.1}" x2="{}" y2="{:.1}" stroke="#30363d" stroke-width="1"/>"##,
            pad_l, py, pad_l + w, py
        );
    }

    format!(
        r#"<div class="chart-wrap"><svg width="{width}" height="{height}" viewBox="0 0 {width} {height}">
{y_labels}
<path d="{path}" fill="none" stroke="{stroke}" stroke-width="1.5"/>
</svg></div>"#
    )
}

fn render_score_buckets_svg(buckets: &[ScoreBucket], width: usize, height: usize) -> String {
    let active: Vec<&ScoreBucket> = buckets.iter().filter(|b| b.count > 0).collect();
    if active.is_empty() {
        return format!(
            r#"<div class="chart-wrap"><svg width="{width}" height="{height}"><text x="10" y="20">No trades</text></svg></div>"#
        );
    }

    let pad_l = 60usize;
    let pad_r = 20usize;
    let pad_t = 16usize;
    let pad_b = 28usize;
    let w = width - pad_l - pad_r;
    let h = height - pad_t - pad_b;

    let n = buckets.len();
    let bar_w = w / n;

    // Use win_rate as the bar metric (0..1).
    let mut bars = String::new();
    for (i, b) in buckets.iter().enumerate() {
        let bar_h = if b.count > 0 { (b.win_rate * h as f64) as usize } else { 0 };
        let x = pad_l + i * bar_w + 2;
        let y = pad_t + h - bar_h;
        let color = if b.win_rate >= 0.5 { "#3fb950" } else { "#f85149" };
        let _ = write!(
            bars,
            r#"<rect x="{x}" y="{y}" width="{}" height="{bar_h}" fill="{color}" opacity="0.8"/>"#,
            bar_w.saturating_sub(4)
        );
        // Label inside bar or below.
        if b.count > 0 {
            let label_y = y + bar_h + 14;
            let _ = write!(
                bars,
                r#"<text x="{}" y="{label_y}" text-anchor="middle">{}</text>"#,
                x + (bar_w / 2),
                b.label,
            );
            let wr_y = if bar_h > 18 { y + bar_h - 4 } else { y.saturating_sub(2) };
            let _ = write!(
                bars,
                r##"<text x="{}" y="{wr_y}" text-anchor="middle" fill="#c9d1d9">{:.0}%</text>"##,
                x + (bar_w / 2),
                b.win_rate * 100.0,
            );
        }
    }

    // Y-axis: 0% to 100%.
    let mut y_labels = String::new();
    for i in 0..=4 {
        let v = i as f64 * 25.0;
        let py = pad_t as f64 + (1.0 - i as f64 / 4.0) * h as f64;
        let _ = write!(
            y_labels,
            r#"<text x="{}" y="{:.1}" text-anchor="end">{:.0}%</text>"#,
            pad_l - 4, py + 4.0, v
        );
        let _ = write!(
            y_labels,
            r##"<line x1="{}" y1="{:.1}" x2="{}" y2="{:.1}" stroke="#30363d" stroke-width="1"/>"##,
            pad_l, py, pad_l + w, py
        );
    }

    format!(
        r#"<div class="chart-wrap"><svg width="{width}" height="{height}" viewBox="0 0 {width} {height}">
{y_labels}{bars}
</svg></div>"#
    )
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn metric_card(html: &mut String, label: &str, value: &str, class: &str) {
    let _ = write!(
        html,
        r#"<div class="metric"><div class="metric-label">{label}</div><div class="metric-value {class}">{value}</div></div>"#
    );
}

fn format_pf(pf: f64) -> String {
    if pf.is_infinite() { "∞".to_string() } else { format!("{pf:.2}") }
}

fn format_usdt(v: f64) -> String {
    if v >= 0.0 { format!("${v:.2}") } else { format!("-${:.2}", v.abs()) }
}
