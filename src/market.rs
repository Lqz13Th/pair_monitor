//! Websocket BBO of every watched symbol on every venue; once a second: book rows, premium/midline/deviation rows
//! per venue pair, deviation episodes and, every `stats_every_s`, the rolling deviation statistics.

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use extrema_infra::{arch::market_assets::api_general::get_micros_timestamp, prelude::*};
use perp_dex::prelude::*;
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::{
    config::Config,
    db::{Rec, Sink, ts_us},
    session::Phase,
    spread::{Book, Episode, PairState, Quote},
    venue::{Client, Venue, VenueFeed},
};

pub const SAMPLE_TASK_ID: u64 = 100;

pub fn bbo_channel() -> WsChannel {
    WsChannel::Lob(Some(LobParam::Bbo { frequency: None }))
}

/// Latest book of every symbol and venue, sampled once a second into book / spread / episode / spread_stats rows.
/// Shared by the live feed and the backfill.
#[derive(Clone)]
pub struct Sampler {
    pub cfg: Arc<Config>,
    pub books: HashMap<(String, Venue), Book>,
    pub pairs: Vec<PairState>,
    pub sink: Sink,
    pub last_stats_s: u64,
    /// The backfill skips book rows (one per symbol and venue per second) and keeps the spread rows.
    pub write_books: bool,
}

#[derive(Clone)]
pub struct Market {
    pub registry: Arc<CommandRegistry>,
    pub feeds: BTreeMap<Venue, VenueFeed>,
    pub keepalive: HashMap<Venue, WsKeepalive>,
    pub last_sec: u64,
    pub sampler: Sampler,
}

/// Venue pairs of each symbol: every later venue in the config order priced against every earlier one, so the first
/// venue (the hub) is always the denominator.
pub fn pairs_of(cfg: &Config, feeds: &BTreeMap<Venue, VenueFeed>) -> Vec<PairState> {
    let listed = feeds
        .iter()
        .map(|(v, f)| (*v, f.insts.keys().cloned().collect()))
        .collect();
    pairs_of_symbols(cfg, &listed, Vec::new())
}

/// Same, from the symbols each venue lists; existing pair states in `keep` carry over.
pub fn pairs_of_symbols(
    cfg: &Config,
    listed: &BTreeMap<Venue, Vec<String>>,
    mut keep: Vec<PairState>,
) -> Vec<PairState> {
    let mut out = Vec::new();
    for sym in &cfg.symbols {
        let on: Vec<Venue> = cfg
            .venues
            .iter()
            .copied()
            .filter(|v| listed.get(v).is_some_and(|s| s.contains(sym)))
            .collect();
        for (j, a) in on.iter().enumerate() {
            for b in &on[..j] {
                let pos = keep
                    .iter()
                    .position(|p| p.sym == *sym && p.a == *a && p.b == *b);
                out.push(match pos {
                    Some(i) => keep.swap_remove(i),
                    None => PairState::new(sym, *a, *b),
                });
            }
        }
    }
    out
}

pub fn episode_rec(ep: &Episode, rt_fee: f64) -> Rec {
    let secs = |t: Option<u64>| t.map(|t| (t as f64 - ep.confirm_us as f64) / 1e6);
    Rec::new(
        "episode",
        &[
            "id",
            "sym",
            "pair",
            "dir",
            "phase",
            "start_ts",
            "confirm_ts",
            "half_ts",
            "end_ts",
            "midline_bp",
            "confirm_dev_bp",
            "peak_dev_bp",
            "peak_edge_bp",
            "end_dev_bp",
            "end_reason",
            "duration_s",
            "half_s",
            "rt_fee_bp",
        ],
        vec![
            ep.id().into(),
            ep.sym.clone().into(),
            ep.pair.clone().into(),
            i64::from(ep.dir).into(),
            ep.phase.name().into(),
            ts_us(ep.start_us).into(),
            ts_us(ep.confirm_us).into(),
            ep.half_us.map(ts_us).into(),
            ep.end_us.map(ts_us).into(),
            ep.midline.into(),
            ep.confirm_dev.into(),
            ep.peak_dev.into(),
            ep.peak_edge.into(),
            ep.end_dev.into(),
            ep.end_reason.map(str::to_string).into(),
            secs(ep.end_us).into(),
            secs(ep.half_us).into(),
            rt_fee.into(),
        ],
    )
    .upsert(&["id"])
}

impl Market {
    async fn connect(&mut self, venue: Venue, task_id: u64) {
        let channel = bbo_channel();
        let (Some(handle), Some(feed)) = (
            self.find_ws_handle(&channel, task_id),
            self.feeds.get(&venue),
        ) else {
            return;
        };
        let res: InfraResult<()> = async {
            let (tx, rx) = oneshot::channel();
            let url = feed.client.connect_msg(&channel).await?;
            handle
                .send_command(
                    TaskCommand::WsConnect {
                        msg: url,
                        ack: AckHandle::new(tx),
                    },
                    Some((AckStatus::WsConnect, rx)),
                )
                .await?;
            for inst in feed.insts.values() {
                let msg = feed.client.sub_msg(&channel, inst).await?;
                handle
                    .send_command(
                        TaskCommand::WsMessage {
                            msg,
                            ack: AckHandle::none(),
                        },
                        None,
                    )
                    .await?;
            }
            Ok(())
        }
        .await;
        match res {
            Ok(()) => info!("{venue}: subscribed {} BBO feeds", feed.insts.len()),
            Err(e) => warn!("{venue}: connect failed: {e:?}"),
        }
    }
}

impl Sampler {
    pub fn update(&mut self, sym: &str, venue: Venue, book: Book) {
        if book.valid() {
            self.books.insert((sym.to_string(), venue), book);
        }
    }

    pub fn sample(&mut self, now: u64) {
        let ts = ts_us(now);
        let phase = Phase::at(now);
        let fresh_us = self.cfg.signal.fresh_ms * 1_000;
        for ((sym, venue), b) in self.books.iter().filter(|_| self.write_books) {
            self.sink.send(Rec::new(
                "book",
                &[
                    "ts",
                    "sym",
                    "venue",
                    "bid",
                    "ask",
                    "bid_sz",
                    "ask_sz",
                    "mid",
                    "spread_bp",
                    "age_ms",
                ],
                vec![
                    ts.into(),
                    sym.clone().into(),
                    venue.name().into(),
                    b.bid.into(),
                    b.ask.into(),
                    b.bid_sz.into(),
                    b.ask_sz.into(),
                    b.mid().into(),
                    b.spread_bp().into(),
                    ((now.saturating_sub(b.recv_us)) as f64 / 1e3).into(),
                ],
            ));
        }
        for p in self.pairs.iter_mut() {
            let rt_fee = 2.0 * (self.cfg.fee(p.a) + self.cfg.fee(p.b));
            let (Some(a), Some(b)) = (
                self.books.get(&(p.sym.clone(), p.a)),
                self.books.get(&(p.sym.clone(), p.b)),
            ) else {
                continue;
            };
            if !phase.in_session() {
                if let Some(ep) = p.close_session(now) {
                    self.sink.send(episode_rec(&ep, rt_fee));
                }
                continue;
            }
            if now.saturating_sub(a.recv_us) > fresh_us || now.saturating_sub(b.recv_us) > fresh_us
            {
                continue;
            }
            let q = Quote::of(a, b);
            let step = p.step(now, &q, phase, &self.cfg.signal);
            for ep in step.started.iter().chain(step.ended.iter()) {
                self.sink.send(episode_rec(ep, rt_fee));
            }
            self.sink.send(Rec::new(
                "spread",
                &[
                    "ts",
                    "sym",
                    "pair",
                    "prem_bp",
                    "midline_bp",
                    "dev_bp",
                    "buy_a_bp",
                    "sell_a_bp",
                    "buy_a_vs_mid_bp",
                    "sell_a_vs_mid_bp",
                    "spread_a_bp",
                    "spread_b_bp",
                    "phase",
                    "in_episode",
                    "rt_fee_bp",
                ],
                vec![
                    ts.into(),
                    p.sym.clone().into(),
                    p.pair().into(),
                    q.prem.into(),
                    step.midline.into(),
                    step.dev.into(),
                    q.buy_a.into(),
                    q.sell_a.into(),
                    step.midline.map(|m| q.edge_vs_mid(-1, m)).into(),
                    step.midline.map(|m| q.edge_vs_mid(1, m)).into(),
                    a.spread_bp().into(),
                    b.spread_bp().into(),
                    phase.name().into(),
                    p.episode().is_some().into(),
                    rt_fee.into(),
                ],
            ));
        }
        let sec = now / 1_000_000;
        if sec >= self.last_stats_s + self.cfg.signal.stats_every_s {
            self.last_stats_s = sec;
            for p in &self.pairs {
                let Some(s) = p.stats(&self.cfg.signal) else {
                    continue;
                };
                self.sink.send(Rec::new(
                    "spread_stats",
                    &[
                        "ts",
                        "sym",
                        "pair",
                        "n",
                        "dev_std_bp",
                        "frac_outside",
                        "ar1",
                        "half_life_s",
                    ],
                    vec![
                        ts.into(),
                        p.sym.clone().into(),
                        p.pair().into(),
                        (s.n as i64).into(),
                        s.dev_std.into(),
                        s.frac_outside.into(),
                        s.ar1.into(),
                        s.half_life_s.into(),
                    ],
                ));
            }
        }
    }
}

impl Strategy for Market {
    async fn initialize(&mut self) {}
}

impl CommandEmitter for Market {
    fn command_init(&mut self, registry: Arc<CommandRegistry>) {
        self.registry = registry;
    }

    fn command_registry(&self) -> Arc<CommandRegistry> {
        self.registry.clone()
    }
}

impl EventHandler for Market {
    async fn on_ws_event(&mut self, msg: InfraMsg<WsTaskInfo>) {
        if let Some(v) = Venue::from_task_id(msg.task_id) {
            self.connect(v, msg.task_id).await;
        }
    }

    async fn on_lob(&mut self, msg: InfraMsg<Vec<WsLob>>) {
        let Some(venue) = Venue::from_task_id(msg.task_id) else {
            return;
        };
        if let Some(k) = self.keepalive.get_mut(&venue)
            && let Some(handle) = self.registry.find_ws_handle(&bbo_channel(), msg.task_id)
            && let Err(e) = k.on_frame(&handle).await
        {
            warn!("{venue} keepalive: {e:?}");
        }
        let Some(feed) = self.feeds.get(&venue) else {
            return;
        };
        let t = get_micros_timestamp();
        for lob in msg.data.iter() {
            let (Some(sym), Some(b), Some(a)) =
                (feed.sym_of(&lob.inst), lob.bids.first(), lob.asks.first())
            else {
                continue;
            };
            let book = Book {
                bid: b.price,
                ask: a.price,
                bid_sz: b.size,
                ask_sz: a.size,
                recv_us: t,
            };
            self.sampler.update(sym, venue, book);
        }
    }

    async fn on_schedule(&mut self, _msg: InfraMsg<AltScheduleEvent>) {
        let now = get_micros_timestamp();
        let sec = now / 1_000_000;
        if sec != self.last_sec {
            self.last_sec = sec;
            self.sampler.sample(now);
        }
    }

    async fn on_lagged(&mut self, key: TaskKey, skipped: u64) {
        warn!("lagged {key:?}: skipped {skipped}");
    }
}

pub fn keepalives(feeds: &BTreeMap<Venue, VenueFeed>) -> HashMap<Venue, WsKeepalive> {
    feeds
        .iter()
        .filter(|(_, f)| matches!(f.client, Client::Lighter(_)))
        .map(|(v, _)| (*v, lighter_keepalive()))
        .collect()
}
