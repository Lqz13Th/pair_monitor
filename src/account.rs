//! Account snapshots every `poll.account_s`: equity, margin, positions (with liquidation distance) and open orders.
//! Hyperliquid open orders and the address request quota weigh 20 each against the 1200 a minute per IP (shared
//! with the browser on this machine), so they are read every `HL_SLOW_S`. Watched symbols without a position get a zero row, so the latest
//! row per venue and symbol is always current.

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tracing::warn;

use crate::{
    api::{hl_info, lighter_get, liq_dist_pct, num, num0},
    config::Config,
    db::{Rec, Sink},
    venue::{Venue, split_hl_coin},
};

const POSITION_COLS: &[&str] = &[
    "ts",
    "venue",
    "sym",
    "size",
    "entry_px",
    "mark_px",
    "notional",
    "upnl",
    "liq_px",
    "liq_dist_pct",
    "margin",
    "leverage",
];

#[derive(Default)]
struct Pos {
    size: f64,
    entry: Option<f64>,
    mark: Option<f64>,
    upnl: Option<f64>,
    liq: Option<f64>,
    margin: Option<f64>,
    leverage: Option<f64>,
}

fn position_rec(ts: DateTime<Utc>, venue: &str, sym: &str, p: &Pos) -> Rec {
    let notional = p.mark.map(|m| m * p.size);
    Rec::new(
        "position",
        POSITION_COLS,
        vec![
            ts.into(),
            venue.into(),
            sym.into(),
            p.size.into(),
            p.entry.into(),
            p.mark.into(),
            notional.into(),
            p.upnl.into(),
            p.liq.into(),
            liq_dist_pct(p.size, p.mark.unwrap_or_default(), p.liq).into(),
            p.margin.into(),
            p.leverage.into(),
        ],
    )
}

fn account_rec(
    ts: DateTime<Utc>,
    venue: &str,
    equity: f64,
    free: Option<f64>,
    margin: Option<f64>,
    upnl: Option<f64>,
    notional: Option<f64>,
) -> Rec {
    Rec::new(
        "account",
        &[
            "ts",
            "venue",
            "equity",
            "free",
            "margin_used",
            "upnl",
            "notional",
        ],
        vec![
            ts.into(),
            venue.into(),
            equity.into(),
            free.into(),
            margin.into(),
            upnl.into(),
            notional.into(),
        ],
    )
}

fn orders_rec(ts: DateTime<Utc>, venue: &str, sym: &str, n: i64, notional: Option<f64>) -> Rec {
    Rec::new(
        "open_orders",
        &["ts", "venue", "sym", "n", "notional"],
        vec![
            ts.into(),
            venue.into(),
            sym.into(),
            n.into(),
            notional.into(),
        ],
    )
}

const HL_SLOW_S: u64 = 60;

async fn hl_once(
    http: &reqwest::Client,
    user: &str,
    cfg: &Config,
    watched: &BTreeMap<Venue, Vec<String>>,
    sink: &Sink,
    slow: bool,
) -> anyhow::Result<()> {
    let ts = Utc::now();
    for venue in cfg.venues.iter().copied().filter(|v| v.hl_dex().is_some()) {
        let dex = venue.hl_dex().expect("hl venue");
        let st = hl_info(
            http,
            json!({"type": "clearinghouseState", "user": user, "dex": dex}),
        )
        .await?;
        let ms = &st["marginSummary"];
        let mut positions: BTreeMap<String, Pos> = BTreeMap::new();
        let mut upnl = 0.0;
        for ap in st["assetPositions"].as_array().into_iter().flatten() {
            let p = &ap["position"];
            let coin = p["coin"].as_str().unwrap_or_default();
            let sym = split_hl_coin(coin).map_or_else(|| coin.to_string(), |(_, s)| s);
            let size = num0(&p["szi"]);
            let value = num(&p["positionValue"]);
            upnl += num0(&p["unrealizedPnl"]);
            positions.insert(
                sym,
                Pos {
                    size,
                    entry: num(&p["entryPx"]),
                    mark: value.filter(|_| size != 0.0).map(|v| v / size.abs()),
                    upnl: num(&p["unrealizedPnl"]),
                    liq: num(&p["liquidationPx"]),
                    margin: num(&p["marginUsed"]),
                    leverage: num(&p["leverage"]["value"]),
                },
            );
        }
        sink.send(account_rec(
            ts,
            venue.name(),
            num0(&ms["accountValue"]),
            num(&st["withdrawable"]),
            num(&ms["totalMarginUsed"]),
            Some(upnl),
            num(&ms["totalNtlPos"]),
        ));
        for sym in watched.get(&venue).into_iter().flatten() {
            if !positions.contains_key(sym) {
                positions.insert(sym.clone(), Pos::default());
            }
        }
        for (sym, p) in &positions {
            sink.send(position_rec(ts, venue.name(), sym, p));
        }
        if !slow {
            continue;
        }

        let orders = hl_info(
            http,
            json!({"type": "openOrders", "user": user, "dex": dex}),
        )
        .await?;
        let mut by_sym: BTreeMap<String, (i64, f64)> = watched
            .get(&venue)
            .into_iter()
            .flatten()
            .map(|s| (s.clone(), (0, 0.0)))
            .collect();
        for o in orders.as_array().into_iter().flatten() {
            let coin = o["coin"].as_str().unwrap_or_default();
            let sym = split_hl_coin(coin).map_or_else(|| coin.to_string(), |(_, s)| s);
            let e = by_sym.entry(sym).or_default();
            e.0 += 1;
            e.1 += num0(&o["sz"]) * num0(&o["limitPx"]);
        }
        for (sym, (n, notional)) in by_sym {
            sink.send(orders_rec(ts, venue.name(), &sym, n, Some(notional)));
        }
    }

    let spot = hl_info(
        http,
        json!({"type": "spotClearinghouseState", "user": user}),
    )
    .await?;
    if let Some(usdc) = spot["balances"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|b| b["coin"] == "USDC")
    {
        let free = num0(&usdc["total"]) - num0(&usdc["hold"]);
        sink.send(account_rec(
            ts,
            "hl_spot",
            free,
            Some(free),
            Some(num0(&usdc["hold"])),
            None,
            None,
        ));
    }

    if !slow {
        return Ok(());
    }
    let rate = hl_info(http, json!({"type": "userRateLimit", "user": user})).await?;
    let (used, cap) = (num0(&rate["nRequestsUsed"]), num0(&rate["nRequestsCap"]));
    sink.send(Rec::new(
        "rate_limit",
        &["ts", "venue", "cum_vlm", "used", "cap", "headroom"],
        vec![
            ts.into(),
            "hl".into(),
            num(&rate["cumVlm"]).into(),
            used.into(),
            cap.into(),
            (cap - used).into(),
        ],
    ));
    Ok(())
}

async fn lighter_once(
    http: &reqwest::Client,
    account: i64,
    watched: &[String],
    sink: &Sink,
) -> anyhow::Result<()> {
    let ts = Utc::now();
    let res = lighter_get(http, &format!("account?by=index&value={account}")).await?;
    let a: &Value = &res["accounts"][0];
    let equity = num0(&a["total_asset_value"]);
    let free = num(&a["available_balance"]);
    let mut positions: BTreeMap<String, Pos> = BTreeMap::new();
    let mut orders: BTreeMap<String, i64> = BTreeMap::new();
    let (mut upnl, mut margin, mut notional) = (0.0, 0.0, 0.0);
    for p in a["positions"].as_array().into_iter().flatten() {
        let sym = p["symbol"].as_str().unwrap_or_default().to_string();
        let size = num0(&p["sign"]).signum() * num0(&p["position"]);
        let value = num0(&p["position_value"]).abs();
        let imf = num0(&p["initial_margin_fraction"]);
        upnl += num0(&p["unrealized_pnl"]);
        margin += num0(&p["allocated_margin"]);
        notional += value;
        orders.insert(
            sym.clone(),
            p["open_order_count"].as_i64().unwrap_or_default(),
        );
        if size == 0.0 && !watched.contains(&sym) {
            continue;
        }
        positions.insert(
            sym,
            Pos {
                size,
                entry: num(&p["avg_entry_price"]).filter(|_| size != 0.0),
                mark: (size != 0.0).then(|| value / size.abs()),
                upnl: num(&p["unrealized_pnl"]),
                liq: num(&p["liquidation_price"]),
                margin: num(&p["allocated_margin"]),
                leverage: (imf > 0.0).then(|| 100.0 / imf),
            },
        );
    }
    sink.send(account_rec(
        ts,
        "lighter",
        equity,
        free,
        Some(margin),
        Some(upnl),
        Some(notional),
    ));
    for sym in watched {
        positions.entry(sym.clone()).or_default();
    }
    for (sym, p) in &positions {
        sink.send(position_rec(ts, "lighter", sym, p));
    }
    for sym in watched {
        sink.send(orders_rec(
            ts,
            "lighter",
            sym,
            orders.get(sym).copied().unwrap_or_default(),
            None,
        ));
    }
    Ok(())
}

pub fn spawn(
    cfg: Arc<Config>,
    http: Arc<reqwest::Client>,
    watched: BTreeMap<Venue, Vec<String>>,
    sink: Sink,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(cfg.poll.account_s));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_slow: Option<Instant> = None;
        loop {
            tick.tick().await;
            let slow = last_slow.is_none_or(|t| t.elapsed() >= Duration::from_secs(HL_SLOW_S));
            if slow {
                last_slow = Some(Instant::now());
            }
            if let Some(user) = cfg.accounts.hl_user.as_deref()
                && let Err(e) = hl_once(&http, user, &cfg, &watched, &sink, slow).await
            {
                warn!("hyperliquid account poll: {e:#}");
            }
            if let Some(account) = cfg.accounts.lighter_account
                && cfg.venues.contains(&Venue::Lighter)
            {
                let syms = watched.get(&Venue::Lighter).cloned().unwrap_or_default();
                if let Err(e) = lighter_once(&http, account, &syms, &sink).await {
                    warn!("lighter account poll: {e:#}");
                }
            }
        }
    });
}
