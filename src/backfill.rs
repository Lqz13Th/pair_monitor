//! Replays recorded BBO files (`recv_us,venue,symbol,exch_us,bid,bid_size,ask,ask_size`, plain or .zst, one file per
//! hour) through the live sampler, writing spread / episode / spread_stats rows from before the first live sample.
//! Rerunning only adds rows before the earliest stored one, so it never duplicates.

use std::{collections::BTreeMap, path::Path, process::Command, sync::Arc, time::Duration};

use anyhow::{Context, bail};
use tokio_postgres::Client;
use tracing::info;

use crate::{
    config::Config,
    market::{Sampler, pairs_of_symbols},
    spread::Book,
    venue::Venue,
};

const MAX_PENDING: usize = 200_000;
/// Gaps longer than this are skipped instead of sampled second by second.
const MAX_GAP_S: u64 = 60;

struct Row {
    recv_us: u64,
    venue: Venue,
    sym: String,
    book: Book,
}

fn venue_of(name: &str) -> Option<Venue> {
    [Venue::HlXyz, Venue::HlIo, Venue::Lighter]
        .into_iter()
        .find(|v| v.name() == name)
}

fn read(path: &Path) -> anyhow::Result<String> {
    if path.extension().is_some_and(|e| e == "zst") {
        let out = Command::new("zstd")
            .arg("-dcq")
            .arg(path)
            .output()
            .context("zstd")?;
        if !out.status.success() {
            bail!(
                "zstd {}: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8(out.stdout)?)
    } else {
        Ok(std::fs::read_to_string(path)?)
    }
}

fn parse(text: &str, cfg: &Config, until_us: u64) -> Vec<Row> {
    let mut rows: Vec<Row> = text
        .lines()
        .filter_map(|l| {
            let c: Vec<&str> = l.split(',').collect();
            if c.len() < 8 {
                return None;
            }
            let recv_us: u64 = c[0].parse().ok()?;
            let venue = venue_of(c[1])?;
            if recv_us >= until_us
                || !cfg.venues.contains(&venue)
                || !cfg.symbols.iter().any(|s| s == c[2])
            {
                return None;
            }
            Some(Row {
                recv_us,
                venue,
                sym: c[2].to_string(),
                book: Book {
                    bid: c[4].parse().ok()?,
                    bid_sz: c[5].parse().ok()?,
                    ask: c[6].parse().ok()?,
                    ask_sz: c[7].parse().ok()?,
                    recv_us,
                },
            })
        })
        .collect();
    rows.sort_by_key(|r| r.recv_us);
    rows
}

pub async fn run(
    cfg: Arc<Config>,
    dir: &Path,
    db: &Client,
    mut sampler: Sampler,
) -> anyhow::Result<()> {
    let until_us = match db
        .query_one("SELECT min(ts) FROM spread", &[])
        .await?
        .get::<_, Option<chrono::DateTime<chrono::Utc>>>(0)
    {
        Some(t) => t.timestamp_micros() as u64,
        None => chrono::Utc::now().timestamp_micros() as u64,
    };
    let mut files: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("bbo-"))
        })
        .collect();
    files.sort();
    info!(
        "backfill: {} files before {}",
        files.len(),
        crate::db::ts_us(until_us)
    );

    // symbols listed per venue in the recording decide the pairs
    let mut listed: BTreeMap<Venue, Vec<String>> = BTreeMap::new();
    let mut cur_s: Option<u64> = None;
    let (mut samples, mut rows_seen) = (0u64, 0usize);
    for (i, path) in files.iter().enumerate() {
        let rows = parse(&read(path)?, &cfg, until_us);
        rows_seen += rows.len();
        for r in rows {
            let syms = listed.entry(r.venue).or_default();
            if !syms.contains(&r.sym) {
                syms.push(r.sym.clone());
                sampler.pairs = pairs_of_symbols(&cfg, &listed, std::mem::take(&mut sampler.pairs));
            }
            let s = r.recv_us / 1_000_000;
            match cur_s {
                Some(c) if s > c => {
                    let from = if s - c > MAX_GAP_S { s - 1 } else { c };
                    for sec in from..s {
                        sampler.sample((sec + 1) * 1_000_000);
                        samples += 1;
                    }
                },
                None => {},
                _ => {},
            }
            cur_s = Some(cur_s.map_or(s, |c| c.max(s)));
            sampler.update(&r.sym, r.venue, r.book);
        }
        while sampler.sink.pending() > MAX_PENDING {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        info!(
            "backfill {}/{}: {} ({} samples, {} rows read)",
            i + 1,
            files.len(),
            path.display(),
            samples,
            rows_seen
        );
    }
    while sampler.sink.pending() > 0 {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    info!("backfill done: {samples} samples");
    Ok(())
}
