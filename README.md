# pair_monitor

Local monitor for the cross-venue pair strategy (pair_arb): cross-venue spreads and when they converge, accounts and
positions on every venue, exchange fills and costs, and the bot's own journal. A Rust collector writes everything to
TimescaleDB; Grafana reads it. The whole stack runs locally, and `deploy/compose.yaml` moves to a server unchanged.

```text
HL xyz / HL io / Lighter BBO (websocket) ─┐
HL + Lighter accounts, fills, funding ────┼─> collector ─> TimescaleDB ─> Grafana (http://127.0.0.1:3200)
pair_arb data dir (rsync, read-only) ─────┘
```

## Run

```sh
cd deploy && docker compose up -d --wait && cd ..   # TimescaleDB on 127.0.0.1:15450, Grafana on 127.0.0.1:3200
cp config/example.toml config/local.toml             # fill in addresses, account index, bot host
LIGHTER_KEY_FILE=~/path/to/lighter_account.json nohup scripts/run_local.sh > data/collector.log 2>&1 &
```

- `LIGHTER_KEY_FILE` is a JSON file with `private_key`. The key only reaches the process environment, and only signs
  auth tokens for reading fills and funding. Without it, the Lighter account is still sampled from public endpoints.
- The schema is applied at every start; all statements are idempotent.
- `python3 grafana/build_dashboards.py` regenerates the dashboards from their definitions, and Grafana reloads them
  within 10 s. `--check` also EXPLAINs every query against the local database.

### Backfill

Recorded BBO files (`recv_us,venue,symbol,exch_us,bid,bid_size,ask,ask_size`, plain or `.zst`, as the perp_dex
`bbo_recorder` writes them) replay through the same sampler. They fill the spread, episode and statistics tables from
before the first live sample:

```sh
./target/release/pair_monitor config/local.toml backfill ~/work/quant/data/pair_monitor/backfill
```

Rerunning only adds rows older than the oldest stored one.

## What is computed

For every symbol, each venue that lists it is priced against every venue before it in `venues`; the first venue
(`hl_xyz`) is the hub. Once a second, while both books are fresh (10 s):

- **premium** `(mid_a / mid_b - 1) x 1e4`;
- **midline**: the rolling 1 h median of the in-session premiums (pair_arb's, including its 30 min warm-up and the
  restart at each session close);
- **deviation**: premium minus midline;
- **touch edges** of buying or selling `a` against `b`, raw and against the midline;
- **rt fee**: the taker fees of a round trip on both legs, `2 x (fee_a + fee_b)`.

**Episodes.** An episode opens when `|deviation| >= band_bp` holds for `persist_ms`, the entry rule of the strategy.
It ends when `|deviation| <= exit_bp` (converged), after `episode_timeout_s`, or when the session closes. Each episode
records:

- the time to half the confirm deviation, and the time to converge;
- the peak deviation;
- the best touch edge toward the midline while it was open, gross and net of the round-trip fee.

**Statistics.** Every minute, rolling statistics of the deviations:

- an AR(1) half-life;
- their standard deviation;
- the share of time outside the band.

## Dashboards

| Dashboard | Content |
|---|---|
| Overview | Total and per-venue equity, P&L (equity net of flows, the bot's status line, journal edge), fees, funding, request headroom, liquidation distance, positions and net exposure |
| Spreads | Per symbol and pair: premium with midline and band, deviation with episode shading, edges against the round-trip fee, touch spreads, mids, top-of-book size, book age, the bot's own premium and midline, the episode list and the bot's trades |
| Convergence | Per pair, phase (regular / pre / post / overnight) and end reason: episode counts, converged %, p50 / p90 time to converge and to half, peak deviation and net edge. Also duration and peak-deviation distributions, episodes by New York hour, peak deviation against duration, episodes and convergence time per hour, AR(1) half-life, deviation std, time outside the band, and premium levels |
| Bot trades | Maker and taker trades per symbol (plan vs realised bp, slippage, USD edge, hedge delay, win rate), plus quoting from the event journal (events per hour, maker fill rate, quote life, order and quote round trips, quote outcomes) and problems (halts, pauses, rejections, errors) |
| Fills and costs | Volume by venue and maker/taker, fees (USD and bp), HL closed P&L, funding by symbol and its rate, every fill, deposits and withdrawals |
| Risk | Leg and net notional, liquidation distance against the guard thresholds, isolated margin, free collateral, unrealised P&L, HL request headroom, open orders, the bot's liquidation buffer and positions, data freshness, pauses |

## Tables

| Table | Rows |
|---|---|
| `book` | BBO per symbol and venue, 1 s |
| `spread` | Premium, midline, deviation, edges and phase per symbol and pair, 1 s |
| `episode` | One per deviation episode |
| `spread_stats` | Rolling deviation statistics per pair, 1 min |
| `account`, `position`, `open_orders`, `rate_limit` | Venue snapshots, 10 s; flat watched symbols are written as zero rows |
| `fill`, `funding`, `flow` | Exchange history, deduplicated by key |
| `bot_taker`, `bot_maker`, `bot_event`, `bot_status`, `bot_status_sym` | The pair_arb journal; events keep their full JSON body |
| `equity_1m`, `net_exposure` | Views |

## License

This project is licensed under the [Apache 2.0 license](LICENSE).
