//! Fills, funding and account flows every `poll.fills_s`. Hyperliquid pages by time from the latest stored row;
//! Lighter returns the latest 100 of each (auth token needed, so only with the API key) and duplicates are dropped
//! by the primary keys.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use chrono::Utc;
use extrema_infra::prelude::OrderSide;
use perp_dex::prelude::*;
use serde_json::{Value, json};
use tokio_postgres::Client;
use tracing::{info, warn};

use crate::{
    api::{hl_info, num, num0},
    config::Config,
    db::{Rec, Sink, ts_ms, ts_us},
    venue::split_hl_coin,
};

const FILL_COLS: &[&str] = &[
    "venue",
    "id",
    "ts",
    "sym",
    "side",
    "px",
    "sz",
    "notional",
    "fee",
    "maker",
    "order_id",
    "cli_order_id",
    "closed_pnl",
];
const DAY_MS: u64 = 86_400_000;
const HL_OVERLAP_MS: u64 = 600_000;

fn hl_venue_sym(coin: &str) -> (String, String) {
    match split_hl_coin(coin) {
        Some((v, s)) => (v.name().to_string(), s),
        None => ("hl".to_string(), coin.to_string()),
    }
}

/// Latest stored time (ms) of a table/venue prefix, or `fallback_ms`.
async fn resume_ms(db: &Client, sql: &str, fallback_ms: u64) -> u64 {
    match db.query_one(sql, &[]).await {
        Ok(row) => row
            .get::<_, Option<chrono::DateTime<Utc>>>(0)
            .map_or(fallback_ms, |t| t.timestamp_millis() as u64),
        Err(_) => fallback_ms,
    }
}

struct Hl {
    user: String,
    fills_ms: u64,
    funding_ms: u64,
    flows_ms: u64,
}

/// Day windows from `since`, so no window exceeds the API's page cap; returns rows and where the next poll starts:
/// the newest row, or `HL_OVERLAP_MS` before now when nothing newer came back.
async fn hl_windows(
    http: &reqwest::Client,
    kind: &str,
    user: &str,
    since: u64,
) -> anyhow::Result<(Vec<Value>, u64)> {
    let now = Utc::now().timestamp_millis() as u64;
    let mut rows = Vec::new();
    let mut newest = since;
    let mut t = since;
    while t < now {
        let end = (t + DAY_MS).min(now);
        let mut body = json!({"type": kind, "user": user, "startTime": t, "endTime": end});
        if kind == "userFillsByTime" {
            body["aggregateByTime"] = json!(false);
        }
        let page = hl_info(http, body).await?;
        for r in page.as_array().into_iter().flatten() {
            newest = newest.max(r["time"].as_u64().unwrap_or_default());
            rows.push(r.clone());
        }
        t = end;
    }
    Ok((rows, newest.max(now.saturating_sub(HL_OVERLAP_MS))))
}

impl Hl {
    async fn poll(&mut self, http: &reqwest::Client, sink: &Sink) -> anyhow::Result<()> {
        let (fills, newest) =
            hl_windows(http, "userFillsByTime", &self.user, self.fills_ms).await?;
        for f in &fills {
            let (venue, sym) = hl_venue_sym(f["coin"].as_str().unwrap_or_default());
            let (px, sz) = (num0(&f["px"]), num0(&f["sz"]));
            sink.send(Rec::new(
                "fill",
                FILL_COLS,
                vec![
                    venue.into(),
                    f["tid"].to_string().into(),
                    ts_ms(f["time"].as_u64().unwrap_or_default()).into(),
                    sym.into(),
                    (if f["side"] == "B" { "buy" } else { "sell" }).into(),
                    px.into(),
                    sz.into(),
                    (px * sz).into(),
                    num(&f["fee"]).into(),
                    f["crossed"].as_bool().map(|c| !c).into(),
                    f["oid"].to_string().into(),
                    f["cloid"].as_str().map(str::to_string).into(),
                    num(&f["closedPnl"]).into(),
                ],
            ));
        }
        self.fills_ms = newest.saturating_sub(1_000);

        let (funding, newest) =
            hl_windows(http, "userFunding", &self.user, self.funding_ms).await?;
        for f in &funding {
            let d = &f["delta"];
            let (venue, sym) = hl_venue_sym(d["coin"].as_str().unwrap_or_default());
            sink.send(Rec::new(
                "funding",
                &["venue", "ts", "sym", "usdc", "rate", "size"],
                vec![
                    venue.into(),
                    ts_ms(f["time"].as_u64().unwrap_or_default()).into(),
                    sym.into(),
                    num(&d["usdc"]).into(),
                    num(&d["fundingRate"]).into(),
                    num(&d["szi"]).into(),
                ],
            ));
        }
        self.funding_ms = newest.saturating_sub(1_000);

        let (flows, newest) = hl_windows(
            http,
            "userNonFundingLedgerUpdates",
            &self.user,
            self.flows_ms,
        )
        .await?;
        for f in &flows {
            let d = &f["delta"];
            let kind = d["type"].as_str().unwrap_or_default().to_string();
            let usdc = num(&d["usdc"]).map(|u| if kind == "withdraw" { -u } else { u });
            sink.send(Rec::new(
                "flow",
                &["venue", "id", "ts", "kind", "usdc", "body"],
                vec![
                    "hl".into(),
                    format!("{}|{}", f["hash"].as_str().unwrap_or_default(), f["time"]).into(),
                    ts_ms(f["time"].as_u64().unwrap_or_default()).into(),
                    kind.into(),
                    usdc.into(),
                    f.clone().into(),
                ],
            ));
        }
        self.flows_ms = newest.saturating_sub(1_000);
        if !fills.is_empty() {
            info!(
                "hyperliquid: {} fills, {} funding, {} flows",
                fills.len(),
                funding.len(),
                flows.len()
            );
        }
        Ok(())
    }
}

struct Lighter {
    cli: LighterCli,
    /// market id -> symbol
    symbols: BTreeMap<u16, String>,
}

impl Lighter {
    fn sym(&self, inst: &str) -> String {
        cli_to_lighter_market_id(inst)
            .ok()
            .and_then(|id| self.symbols.get(&id).cloned())
            .unwrap_or_else(|| inst.to_string())
    }

    async fn poll(&self, sink: &Sink) -> anyhow::Result<()> {
        for f in self.cli.get_fills(None, None).await? {
            let notional = f.price * f.size;
            sink.send(Rec::new(
                "fill",
                FILL_COLS,
                vec![
                    "lighter".into(),
                    f.trade_id.to_string().into(),
                    ts_us(f.timestamp).into(),
                    self.sym(&f.inst).into(),
                    (if f.side == OrderSide::BUY {
                        "buy"
                    } else {
                        "sell"
                    })
                    .into(),
                    f.price.into(),
                    f.size.into(),
                    notional.into(),
                    (notional * f.fee_raw as f64 / 1e6).into(),
                    f.is_maker.into(),
                    f.order_id.clone().into(),
                    f.cli_order_id.clone().into(),
                    None::<f64>.into(),
                ],
            ));
        }
        for f in self.cli.get_position_funding(None, None).await? {
            sink.send(Rec::new(
                "funding",
                &["venue", "ts", "sym", "usdc", "rate", "size"],
                vec![
                    "lighter".into(),
                    ts_us(f.timestamp_us()).into(),
                    self.symbols
                        .get(&f.market_id)
                        .cloned()
                        .unwrap_or_else(|| f.inst())
                        .into(),
                    f.change_usdc().into(),
                    f.rate.parse::<f64>().ok().into(),
                    f.position_size
                        .parse::<f64>()
                        .ok()
                        .map(|s| if f.position_side == "short" { -s } else { s })
                        .into(),
                ],
            ));
        }
        Ok(())
    }
}

pub async fn spawn(cfg: Arc<Config>, http: Arc<reqwest::Client>, db: Client, sink: Sink) {
    let backfill = Utc::now().timestamp_millis() as u64 - cfg.poll.fills_backfill_h * 3_600_000;
    let mut hl = match &cfg.accounts.hl_user {
        Some(user) => Some(Hl {
            user: user.clone(),
            fills_ms: resume_ms(
                &db,
                "SELECT max(ts) FROM fill WHERE venue LIKE 'hl%'",
                backfill,
            )
            .await,
            funding_ms: resume_ms(
                &db,
                "SELECT max(ts) FROM funding WHERE venue LIKE 'hl%'",
                backfill,
            )
            .await,
            flows_ms: resume_ms(&db, "SELECT max(ts) FROM flow WHERE venue = 'hl'", backfill).await,
        }),
        None => None,
    };
    let lighter = match (
        cfg.accounts.lighter_account,
        cfg.accounts.lighter_api_key_index,
    ) {
        (Some(account), Some(key_index)) => match std::env::var("LIGHTER_API_PRIVATE_KEY")
            .map_err(anyhow::Error::from)
            .and_then(|k| Ok(LighterAuth::new(account, key_index, &k)?))
        {
            Ok(auth) => {
                let mut cli = LighterCli::new(http.clone());
                cli.set_auth(auth);
                let symbols = match cli.get_order_book_details(None).await {
                    Ok(d) => d
                        .into_markets()
                        .map(|m| (m.market_id, m.symbol.clone()))
                        .collect(),
                    Err(e) => {
                        warn!("lighter markets: {e:?}");
                        BTreeMap::new()
                    },
                };
                Some(Lighter { cli, symbols })
            },
            Err(e) => {
                warn!("lighter fills off (no LIGHTER_API_PRIVATE_KEY?): {e:#}");
                None
            },
        },
        _ => None,
    };
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(cfg.poll.fills_s));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Some(hl) = hl.as_mut()
                && let Err(e) = hl.poll(&http, &sink).await
            {
                warn!("hyperliquid fills poll: {e:#}");
            }
            if let Some(l) = lighter.as_ref()
                && let Err(e) = l.poll(&sink).await
            {
                warn!("lighter fills poll: {e:#}");
            }
        }
    });
}
