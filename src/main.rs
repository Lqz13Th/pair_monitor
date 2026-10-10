//! pair_monitor: collects cross-venue spreads, accounts, fills and the pair_arb journal into TimescaleDB for Grafana.
//!
//! `cargo run --release -- config/local.toml`                      collect
//! `cargo run --release -- config/local.toml backfill <bbo dir>`    replay recorded BBO files before the first sample

mod account;
mod api;
mod backfill;
mod bot;
mod config;
mod db;
mod fills;
mod market;
mod session;
mod spread;
mod venue;

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use anyhow::Context;
use extrema_infra::prelude::*;
use perp_dex::prelude::*;
use tracing::{info, warn};

use crate::{
    config::Config,
    market::{Market, SAMPLE_TASK_ID, Sampler, bbo_channel, keepalives, pairs_of},
    venue::{Venue, discover},
};

/// Rebuilds each pair's midline from the premiums stored during the current session.
async fn seed_midlines(db: &tokio_postgres::Client, cfg: &Config, pairs: &mut [spread::PairState]) {
    let window = cfg.signal.midline_window_s as f64;
    for p in pairs.iter_mut() {
        let sql = "SELECT prem_bp FROM spread WHERE sym = $1 AND pair = $2 AND phase <> 'closed' \
                   AND ts > now() - make_interval(secs => $3) ORDER BY ts";
        match db.query(sql, &[&p.sym, &p.pair(), &window]).await {
            Ok(rows) => {
                let prems: Vec<f64> = rows
                    .iter()
                    .filter_map(|r| r.get::<_, Option<f64>>(0))
                    .collect();
                if !prems.is_empty() {
                    info!(
                        "{} {}: midline seeded from {} samples",
                        p.sym,
                        p.pair(),
                        prems.len()
                    );
                }
                p.seed(prems, &cfg.signal);
            },
            Err(e) => warn!("seed {} {}: {e}", p.sym, p.pair()),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,extrema_infra=warn".into()),
        )
        .init();
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/local.toml".into());
    let cfg = Arc::new(Config::load(&path)?);
    let http = Arc::new(reqwest::Client::new());

    let db = db::init(&cfg.db_url).await.context("postgres")?;
    let sink = db::spawn_writer(cfg.db_url.clone());

    if std::env::args().nth(2).as_deref() == Some("backfill") {
        let dir = std::env::args().nth(3).context("backfill <dir>")?;
        let sampler = Sampler {
            cfg: cfg.clone(),
            books: Default::default(),
            pairs: Vec::new(),
            sink,
            last_stats_s: 0,
            write_books: false,
        };
        return backfill::run(cfg, &config::expand(&dir), &db, sampler).await;
    }

    let mut feeds = BTreeMap::new();
    for v in &cfg.venues {
        let feed = loop {
            match discover(*v, &cfg.symbols, http.clone()).await {
                Ok(f) => break f,
                Err(e) => {
                    warn!("{v}: {e:#}; retrying in 30 s");
                    tokio::time::sleep(Duration::from_secs(30)).await;
                },
            }
        };
        info!("{v}: {:?}", feed.insts);
        feeds.insert(*v, feed);
    }
    let watched: BTreeMap<Venue, Vec<String>> = feeds
        .iter()
        .map(|(v, f)| (*v, f.insts.keys().cloned().collect()))
        .collect();
    let mut pairs = pairs_of(&cfg, &feeds);
    info!("{} venue pairs", pairs.len());
    seed_midlines(&db, &cfg, &mut pairs).await;

    account::spawn(cfg.clone(), http.clone(), watched, sink.clone());
    fills::spawn(
        cfg.clone(),
        http.clone(),
        db::connect(&cfg.db_url).await?,
        sink.clone(),
    )
    .await;
    if let Some(bot) = cfg.bot.clone() {
        bot::spawn(bot, db::connect(&cfg.db_url).await?, sink.clone());
    }

    let mut builder = EnvBuilder::new().with_ws_decoder(LighterWs);
    for (v, f) in &feeds {
        builder = builder.with_task(WsTaskInfo {
            market: f.client.market(),
            ws_channel: bbo_channel(),
            filter_channels: true,
            chunk: 1,
            task_base_id: Some(v.task_id()),
        });
    }
    let env = builder
        .with_task(AltTaskInfo {
            alt_task_type: AltTaskType::TimeScheduler(Duration::from_millis(200)),
            chunk: 1,
            task_base_id: Some(SAMPLE_TASK_ID),
        })
        .with_strategy_module(Market {
            registry: Arc::new(CommandRegistry::default()),
            keepalive: keepalives(&feeds),
            feeds,
            last_sec: 0,
            sampler: Sampler {
                cfg: cfg.clone(),
                books: Default::default(),
                pairs,
                sink,
                last_stats_s: 0,
                write_books: true,
            },
        })
        .build()?;
    env.execute().await;
    Ok(())
}
