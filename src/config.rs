use std::{collections::BTreeMap, path::PathBuf};

use anyhow::Context;
use serde::Deserialize;

use crate::venue::Venue;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    pub db_url: String,
    /// Underlyings to watch; each is sampled on every venue that lists it.
    pub symbols: Vec<String>,
    pub venues: Vec<Venue>,
    #[serde(default)]
    pub signal: SignalCfg,
    /// Taker fee per venue in bp, for net-of-fee edges.
    #[serde(default)]
    pub taker_fee_bp: BTreeMap<Venue, f64>,
    pub accounts: AccountsCfg,
    #[serde(default)]
    pub poll: PollCfg,
    pub bot: Option<BotCfg>,
}

/// Mirrors pair_arb's signal: premium against a rolling in-session median, entered beyond `band_bp` for
/// `persist_ms`. Episodes end once the deviation is back within `exit_bp`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct SignalCfg {
    pub band_bp: f64,
    pub persist_ms: u64,
    pub midline_window_s: usize,
    pub midline_min_samples: usize,
    pub exit_bp: f64,
    pub episode_timeout_s: u64,
    pub fresh_ms: u64,
    pub stats_window_s: usize,
    pub stats_every_s: u64,
}

impl Default for SignalCfg {
    fn default() -> Self {
        Self {
            band_bp: 2.0,
            persist_ms: 2_000,
            midline_window_s: 3_600,
            midline_min_samples: 1_800,
            exit_bp: 0.5,
            episode_timeout_s: 1_800,
            fresh_ms: 10_000,
            stats_window_s: 3_600,
            stats_every_s: 60,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct AccountsCfg {
    /// Hyperliquid owner address; public, no key needed.
    pub hl_user: Option<String>,
    pub lighter_account: Option<i64>,
    /// With `LIGHTER_API_PRIVATE_KEY` in the environment, auth tokens unlock Lighter fills and funding.
    pub lighter_api_key_index: Option<u8>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct PollCfg {
    pub account_s: u64,
    pub fills_s: u64,
    pub fills_backfill_h: u64,
}

impl Default for PollCfg {
    fn default() -> Self {
        Self {
            account_s: 10,
            fills_s: 30,
            fills_backfill_h: 168,
        }
    }
}

/// The bot's data dir, mirrored read-only over rsync.
#[derive(Clone, Debug, Deserialize)]
pub struct BotCfg {
    pub host: String,
    pub ssh_key: String,
    pub remote_dir: String,
    pub local_dir: String,
    #[serde(default = "default_sync_s")]
    pub sync_s: u64,
}

fn default_sync_s() -> u64 {
    60
}

pub fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(path),
    }
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
        toml::from_str(&raw).with_context(|| format!("parsing {path}"))
    }

    pub fn fee(&self, v: Venue) -> f64 {
        self.taker_fee_bp.get(&v).copied().unwrap_or_default()
    }
}
