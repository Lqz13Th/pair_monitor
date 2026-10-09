use std::{collections::BTreeMap, fmt, sync::Arc};

use anyhow::Context;
use extrema_infra::{arch::market_assets::exchange::prelude::HyperliquidCli, prelude::*};
use perp_dex::prelude::*;
use serde::Deserialize;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Venue {
    HlXyz,
    HlIo,
    Lighter,
}

impl Venue {
    pub fn name(self) -> &'static str {
        match self {
            Venue::HlXyz => "hl_xyz",
            Venue::HlIo => "hl_io",
            Venue::Lighter => "lighter",
        }
    }

    /// Hyperliquid builder dex of the venue.
    pub fn hl_dex(self) -> Option<&'static str> {
        match self {
            Venue::HlXyz => Some("xyz"),
            Venue::HlIo => Some("io"),
            Venue::Lighter => None,
        }
    }

    pub fn from_hl_dex(dex: &str) -> Option<Venue> {
        match dex {
            "xyz" => Some(Venue::HlXyz),
            "io" => Some(Venue::HlIo),
            _ => None,
        }
    }

    /// Websocket task id of the venue's BBO feed.
    pub fn task_id(self) -> u64 {
        match self {
            Venue::HlXyz => 1,
            Venue::HlIo => 2,
            Venue::Lighter => 3,
        }
    }

    pub fn from_task_id(id: u64) -> Option<Venue> {
        [Venue::HlXyz, Venue::HlIo, Venue::Lighter]
            .into_iter()
            .find(|v| v.task_id() == id)
    }
}

impl fmt::Display for Venue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Clone)]
pub enum Client {
    Hl(HyperliquidCli),
    Lighter(LighterCli),
}

impl Client {
    pub fn market(&self) -> Market {
        match self {
            Client::Hl(_) => Market::HyperLiquid,
            Client::Lighter(c) => c.venue.market(),
        }
    }

    pub async fn connect_msg(&self, channel: &WsChannel) -> InfraResult<String> {
        match self {
            Client::Hl(c) => c.get_public_connect_msg(channel).await,
            Client::Lighter(c) => c.get_public_connect_msg(channel).await,
        }
    }

    pub async fn sub_msg(&self, channel: &WsChannel, inst: &str) -> InfraResult<String> {
        let insts = [inst.to_string()];
        match self {
            Client::Hl(c) => c.get_public_sub_msg(channel, Some(&insts)).await,
            Client::Lighter(c) => c.get_public_sub_msg(channel, Some(&insts)).await,
        }
    }
}

/// One venue with the instrument of each watched symbol it lists.
#[derive(Clone)]
pub struct VenueFeed {
    pub client: Client,
    /// symbol -> instrument
    pub insts: BTreeMap<String, String>,
}

impl VenueFeed {
    pub fn sym_of(&self, inst: &str) -> Option<&str> {
        self.insts
            .iter()
            .find(|(_, i)| i.as_str() == inst)
            .map(|(s, _)| s.as_str())
    }
}

pub async fn discover(
    venue: Venue,
    symbols: &[String],
    http: Arc<reqwest::Client>,
) -> anyhow::Result<VenueFeed> {
    let (client, infos) = match venue.hl_dex() {
        Some(dex) => {
            let mut cli = HyperliquidCli::new(http);
            cli.set_perp_dex(Some(dex.into()));
            cli.init_inst_index_map()
                .await
                .with_context(|| format!("{venue} instruments"))?;
            let infos = cli.get_instrument_info(InstrumentType::Perpetual).await?;
            (Client::Hl(cli), infos)
        },
        None => {
            let cli = LighterCli::new(http);
            let infos = cli.get_instrument_info(InstrumentType::Perpetual).await?;
            (Client::Lighter(cli), infos)
        },
    };
    let insts = symbols
        .iter()
        .filter_map(|sym| {
            infos
                .iter()
                .filter(|i| i.state == InstrumentStatus::Live)
                .find(|i| match venue {
                    Venue::Lighter => i.inst_code.as_deref() == Some(sym.as_str()),
                    _ => i.inst.split('_').next() == Some(sym.as_str()),
                })
                .map(|i| (sym.clone(), i.inst.clone()))
        })
        .collect();
    Ok(VenueFeed { client, insts })
}

/// `xyz:SNDK` -> (hl_xyz, SNDK).
pub fn split_hl_coin(coin: &str) -> Option<(Venue, String)> {
    let (dex, base) = coin.split_once(':')?;
    Some((Venue::from_hl_dex(dex)?, base.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn venues_round_trip_through_names_and_tasks() {
        for v in [Venue::HlXyz, Venue::HlIo, Venue::Lighter] {
            assert_eq!(Venue::from_task_id(v.task_id()), Some(v));
            let parsed: Venue =
                toml::from_str::<BTreeMap<String, Venue>>(&format!("v = \"{v}\"")).unwrap()["v"];
            assert_eq!(parsed, v);
        }
        assert_eq!(
            split_hl_coin("xyz:SNDK"),
            Some((Venue::HlXyz, "SNDK".into()))
        );
        assert_eq!(split_hl_coin("io:DRAM"), Some((Venue::HlIo, "DRAM".into())));
        assert_eq!(split_hl_coin("BTC"), None);
    }
}
