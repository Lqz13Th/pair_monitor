-- Applied by the collector at startup; every statement is idempotent.
CREATE EXTENSION IF NOT EXISTS timescaledb;

-- Best bid/offer of every symbol on every venue, sampled once a second.
CREATE TABLE IF NOT EXISTS book (
    ts timestamptz NOT NULL,
    sym text NOT NULL,
    venue text NOT NULL,
    bid double precision,
    ask double precision,
    bid_sz double precision,
    ask_sz double precision,
    mid double precision,
    spread_bp double precision,
    age_ms double precision
);
SELECT create_hypertable('book', 'ts', if_not_exists => TRUE, chunk_time_interval => INTERVAL '1 day');
CREATE INDEX IF NOT EXISTS book_sym_venue_ts ON book (sym, venue, ts DESC);

-- Premium of venue a over venue b (bp), pair_arb's midline (rolling in-session median) and the deviation from it.
-- buy_a_bp / sell_a_bp are touch-to-touch edges; *_vs_mid_bp the same edges against the midline. rt_fee_bp: taker fees
-- of a round trip on both legs (2 x (fee_a + fee_b)), the hurdle a deviation has to clear.
CREATE TABLE IF NOT EXISTS spread (
    ts timestamptz NOT NULL,
    sym text NOT NULL,
    pair text NOT NULL,
    prem_bp double precision,
    midline_bp double precision,
    dev_bp double precision,
    buy_a_bp double precision,
    sell_a_bp double precision,
    buy_a_vs_mid_bp double precision,
    sell_a_vs_mid_bp double precision,
    spread_a_bp double precision,
    spread_b_bp double precision,
    phase text,
    in_episode boolean,
    rt_fee_bp double precision
);
SELECT create_hypertable('spread', 'ts', if_not_exists => TRUE, chunk_time_interval => INTERVAL '1 day');
CREATE INDEX IF NOT EXISTS spread_sym_pair_ts ON spread (sym, pair, ts DESC);

-- Deviation episodes: |dev| >= band for persist_ms (confirm), until |dev| <= exit_bp (converged), the timeout or the
-- session end. dir +1: a rich against the midline. peak_edge_bp: best touch edge toward the midline while open.
CREATE TABLE IF NOT EXISTS episode (
    id text PRIMARY KEY,
    sym text NOT NULL,
    pair text NOT NULL,
    dir bigint,
    phase text,
    start_ts timestamptz NOT NULL,
    confirm_ts timestamptz,
    half_ts timestamptz,
    end_ts timestamptz,
    midline_bp double precision,
    confirm_dev_bp double precision,
    peak_dev_bp double precision,
    peak_edge_bp double precision,
    end_dev_bp double precision,
    end_reason text,
    duration_s double precision,
    half_s double precision,
    rt_fee_bp double precision
);
CREATE INDEX IF NOT EXISTS episode_start ON episode (start_ts DESC);

-- Rolling deviation statistics per pair: AR(1) half-life, dispersion, share of time outside the band.
CREATE TABLE IF NOT EXISTS spread_stats (
    ts timestamptz NOT NULL,
    sym text NOT NULL,
    pair text NOT NULL,
    n bigint,
    dev_std_bp double precision,
    frac_outside double precision,
    ar1 double precision,
    half_life_s double precision
);
SELECT create_hypertable('spread_stats', 'ts', if_not_exists => TRUE);

CREATE TABLE IF NOT EXISTS account (
    ts timestamptz NOT NULL,
    venue text NOT NULL,
    equity double precision,
    free double precision,
    margin_used double precision,
    upnl double precision,
    notional double precision
);
SELECT create_hypertable('account', 'ts', if_not_exists => TRUE);

-- liq_dist_pct: adverse move left before the liquidation price, % of mark.
CREATE TABLE IF NOT EXISTS position (
    ts timestamptz NOT NULL,
    venue text NOT NULL,
    sym text NOT NULL,
    size double precision,
    entry_px double precision,
    mark_px double precision,
    notional double precision,
    upnl double precision,
    liq_px double precision,
    liq_dist_pct double precision,
    margin double precision,
    leverage double precision
);
SELECT create_hypertable('position', 'ts', if_not_exists => TRUE);
CREATE INDEX IF NOT EXISTS position_sym_ts ON position (sym, ts DESC);

CREATE TABLE IF NOT EXISTS open_orders (
    ts timestamptz NOT NULL,
    venue text NOT NULL,
    sym text NOT NULL,
    n bigint,
    notional double precision
);
SELECT create_hypertable('open_orders', 'ts', if_not_exists => TRUE);

-- Hyperliquid address request quota: cap = 10000 + cumulative volume.
CREATE TABLE IF NOT EXISTS rate_limit (
    ts timestamptz NOT NULL,
    venue text NOT NULL,
    cum_vlm double precision,
    used double precision,
    cap double precision,
    headroom double precision
);
SELECT create_hypertable('rate_limit', 'ts', if_not_exists => TRUE);

-- Exchange fills. fee in USDC (negative = rebate); closed_pnl as reported by Hyperliquid.
CREATE TABLE IF NOT EXISTS fill (
    venue text NOT NULL,
    id text NOT NULL,
    ts timestamptz NOT NULL,
    sym text NOT NULL,
    side text,
    px double precision,
    sz double precision,
    notional double precision,
    fee double precision,
    maker boolean,
    order_id text,
    cli_order_id text,
    closed_pnl double precision,
    PRIMARY KEY (venue, id, ts)
);
CREATE INDEX IF NOT EXISTS fill_ts ON fill (ts DESC);

-- Funding payments; usdc > 0 received.
CREATE TABLE IF NOT EXISTS funding (
    venue text NOT NULL,
    ts timestamptz NOT NULL,
    sym text NOT NULL,
    usdc double precision,
    rate double precision,
    size double precision,
    PRIMARY KEY (venue, sym, ts)
);

-- Deposits, withdrawals and transfers as the venue reports them; usdc > 0 into the account where the kind says so.
CREATE TABLE IF NOT EXISTS flow (
    venue text NOT NULL,
    id text NOT NULL,
    ts timestamptz NOT NULL,
    kind text,
    usdc double precision,
    body jsonb,
    PRIMARY KEY (venue, id)
);

-- Bot journal (pair_arb data dir).
CREATE TABLE IF NOT EXISTS bot_taker (
    ts timestamptz NOT NULL,
    sym text NOT NULL,
    dir bigint,
    sz_io double precision,
    sz_xyz double precision,
    px_io double precision,
    px_xyz double precision,
    plan_bp double precision,
    real_bp double precision,
    pnl_after double precision,
    PRIMARY KEY (ts, sym)
);

CREATE TABLE IF NOT EXISTS bot_maker (
    ts timestamptz NOT NULL,
    sym text NOT NULL,
    dir bigint,
    sz double precision,
    px_io double precision,
    px_xyz double precision,
    plan_bp double precision,
    real_bp double precision,
    hedge_delay_ms double precision,
    pnl_after double precision,
    PRIMARY KEY (ts, sym)
);

CREATE TABLE IF NOT EXISTS bot_event (
    file text NOT NULL,
    line bigint NOT NULL,
    ts timestamptz NOT NULL,
    ev text NOT NULL,
    sym text,
    body jsonb,
    PRIMARY KEY (file, line)
);
CREATE INDEX IF NOT EXISTS bot_event_ev_ts ON bot_event (ev, ts DESC);

CREATE TABLE IF NOT EXISTS bot_status (
    ts timestamptz PRIMARY KEY,
    in_session boolean,
    paused text,
    pnl double precision,
    cash double precision,
    headroom double precision,
    free double precision,
    quotes bigint,
    liq_pct double precision,
    counts jsonb
);

CREATE TABLE IF NOT EXISTS bot_status_sym (
    ts timestamptz NOT NULL,
    sym text NOT NULL,
    pos_io double precision,
    pos_xyz double precision,
    midline_bp double precision,
    prem_bp double precision,
    age_io_ms double precision,
    age_xyz_ms double precision,
    PRIMARY KEY (ts, sym)
);

CREATE TABLE IF NOT EXISTS ingest_offset (
    source text PRIMARY KEY,
    pos bigint NOT NULL
);

-- Total equity across venues per minute: each venue's last snapshot at or before the minute (at most 10 minutes old),
-- summed, so a minute where one venue has not reported yet still counts it.
CREATE OR REPLACE VIEW equity_1m AS
WITH b AS (
    SELECT time_bucket('1 minute', ts) AS bucket, venue, last(equity, ts) AS equity FROM account GROUP BY 1, 2
), grid AS (SELECT DISTINCT bucket FROM b), venues AS (SELECT DISTINCT venue FROM b)
SELECT g.bucket AS ts, sum(x.equity) AS equity, count(x.equity) AS venues
FROM grid g CROSS JOIN venues v
LEFT JOIN LATERAL (
    SELECT equity FROM b WHERE b.venue = v.venue AND b.bucket <= g.bucket AND b.bucket > g.bucket - INTERVAL '10 minutes'
    ORDER BY b.bucket DESC LIMIT 1
) x ON TRUE
GROUP BY g.bucket;

-- Net position per symbol across venues, from the latest snapshot of each venue.
CREATE OR REPLACE VIEW net_exposure AS
WITH latest AS (
    SELECT DISTINCT ON (venue, sym) venue, sym, size, mark_px, notional, ts
    FROM position WHERE ts > now() - INTERVAL '5 minutes'
    ORDER BY venue, sym, ts DESC
)
SELECT sym, sum(size) AS net_size, sum(size * mark_px) AS net_notional, sum(abs(notional)) AS gross_notional,
       count(*) AS legs, max(ts) AS ts
FROM latest GROUP BY sym;
