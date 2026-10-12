#!/usr/bin/env python3
"""Generates the Grafana dashboards (grafana/dashboards/*.json) from the panel definitions below.

python3 grafana/build_dashboards.py            # write the JSON files (Grafana reloads them within 10 s)
python3 grafana/build_dashboards.py --check    # also EXPLAIN every query against the local database
"""

import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any

OUT = Path(__file__).resolve().parent / "dashboards"
DS = {"type": "grafana-postgresql-datasource", "uid": "pm-db"}
# (uid, tab label) in tab order
PAGES = [
    ("pm-overview", "Overview"),
    ("pm-spreads", "Spreads"),
    ("pm-convergence", "Convergence"),
    ("pm-trades", "Bot trades"),
    ("pm-fills", "Fills and costs"),
    ("pm-risk", "Risk"),
]
TAB = "padding:4px 14px; border:1px solid #3a4250; border-radius:4px; text-decoration:none; color:#c8d0da; font-size:13px;"
TAB_ON = "padding:4px 14px; border:1px solid #3d71d9; border-radius:4px; text-decoration:none; color:#fff; background:#1f3b6e; font-size:13px;"


def nav(current):
    """Tabs in a full-width text panel at the top: native dashboard links share the controls bar with the variable
    pickers and move sideways between dashboards with different variables. The links keep the time range."""
    tabs = "".join(
        f'<a href="/d/{uid}?${{__url_time_range}}" style="{TAB_ON if uid == current else TAB}">{label}</a>'
        for uid, label in PAGES
    )
    return {
        "type": "text",
        "title": "",
        "transparent": True,
        "options": {
            "mode": "html",
            "content": f'<div style="display:flex; gap:6px; flex-wrap:wrap;">{tabs}</div>',
        },
    }


class Board:
    def __init__(self, uid, title, time_from="now-6h", variables=()):
        self.uid, self.title, self.time_from = uid, title, time_from
        self.variables = list(variables)
        self.panels, self.y, self.x, self.row_h, self.next_id = [], 0, 0, 0, 1
        self.annotations = []
        self.add(nav(uid), 24, 2)

    def row(self, title):
        self.newline()
        self.panels.append(
            {
                "type": "row",
                "title": title,
                "id": self._id(),
                "collapsed": False,
                "gridPos": {"h": 1, "w": 24, "x": 0, "y": self.y},
                "panels": [],
            }
        )
        self.y += 1

    def newline(self):
        if self.x:
            self.y += self.row_h
            self.x, self.row_h = 0, 0

    def _id(self):
        self.next_id += 1
        return self.next_id

    def add(self, panel, w=12, h=8):
        if self.x + w > 24:
            self.newline()
        panel["id"] = self._id()
        panel["gridPos"] = {"h": h, "w": w, "x": self.x, "y": self.y}
        if panel["type"] != "text":
            panel.setdefault("datasource", DS)
        self.panels.append(panel)
        self.x += w
        self.row_h = max(self.row_h, h)

    def json(self):
        return {
            "uid": self.uid,
            "title": self.title,
            "tags": ["pair_monitor"],
            "timezone": "",
            "editable": True,
            "refresh": "30s",
            "schemaVersion": 39,
            "time": {"from": self.time_from, "to": "now"},
            "graphTooltip": 1,
            "links": [],
            "panels": self.panels,
            "templating": {"list": self.variables},
            "annotations": {
                "list": [
                    {
                        "builtIn": 1,
                        "datasource": {"type": "grafana", "uid": "-- Grafana --"},
                        "enable": True,
                        "hide": True,
                        "name": "Annotations & Alerts",
                        "type": "dashboard",
                    }
                ]
                + self.annotations
            },
        }


def target(sql, fmt="time_series", ref="A"):
    return {
        "refId": ref,
        "datasource": DS,
        "rawQuery": True,
        "editorMode": "code",
        "format": fmt,
        "rawSql": sql.strip(),
    }


def ts(
    title,
    *sqls,
    unit=None,
    desc=None,
    stack=False,
    points=False,
    thresholds=None,
    overrides=None,
    minv=None,
    legend="list",
    decimals=None,
):
    custom = {
        "lineWidth": 1,
        "fillOpacity": 0,
        "showPoints": "auto",
        "spanNulls": False,
        "pointSize": 4,
    }
    if stack:
        custom.update({"stacking": {"mode": "normal"}, "fillOpacity": 30})
    if points:
        custom.update({"drawStyle": "points", "pointSize": 6, "showPoints": "always"})
    defaults = {"custom": custom}
    if unit:
        defaults["unit"] = unit
    if minv is not None:
        defaults["min"] = minv
    if decimals is not None:
        defaults["decimals"] = decimals
    if thresholds:
        defaults["thresholds"] = {"mode": "absolute", "steps": thresholds}
        custom["thresholdsStyle"] = {"mode": "line+area" if len(thresholds) > 2 else "line"}
    return {
        "type": "timeseries",
        "title": title,
        "description": desc or "",
        "targets": [target(s, ref=chr(65 + i)) for i, s in enumerate(sqls)],
        "fieldConfig": {"defaults": defaults, "overrides": overrides or []},
        "options": {
            "legend": {
                "displayMode": legend,
                "placement": "bottom",
                "calcs": ["lastNotNull", "mean"] if legend == "table" else [],
            },
            "tooltip": {"mode": "multi", "sort": "desc"},
        },
    }


def stat(title, sql, unit=None, desc=None, thresholds=None, decimals=2, color="value"):
    defaults: dict[str, Any] = {"decimals": decimals}
    if unit:
        defaults["unit"] = unit
    defaults["thresholds"] = {
        "mode": "absolute",
        "steps": thresholds or [{"color": "text", "value": None}],
    }
    return {
        "type": "stat",
        "title": title,
        "description": desc or "",
        "targets": [target(sql, "table")],
        "fieldConfig": {"defaults": defaults, "overrides": []},
        "options": {
            "reduceOptions": {"calcs": ["lastNotNull"], "fields": "", "values": False},
            "colorMode": color,
            "graphMode": "none",
            "textMode": "auto",
            "justifyMode": "auto",
        },
    }


def table(title, sql, desc=None, units=None, sort=None):
    overrides = [
        {"matcher": {"id": "byName", "options": k}, "properties": [{"id": "unit", "value": v}]}
        for k, v in (units or {}).items()
    ]
    return {
        "type": "table",
        "title": title,
        "description": desc or "",
        "targets": [target(sql, "table")],
        "fieldConfig": {
            "defaults": {"custom": {"align": "auto", "filterable": True}, "decimals": 2},
            "overrides": overrides,
        },
        "options": {"showHeader": True, "cellHeight": "sm", "sortBy": sort or []},
    }


def bars(title, sql, unit=None, desc=None, stack=False):
    defaults = {"unit": unit} if unit else {}
    return {
        "type": "barchart",
        "title": title,
        "description": desc or "",
        "targets": [target(sql, "table")],
        "fieldConfig": {"defaults": defaults, "overrides": []},
        "options": {
            "orientation": "auto",
            "stacking": "normal" if stack else "none",
            "showValue": "auto",
            "legend": {"displayMode": "list", "placement": "bottom"},
            "xTickLabelRotation": -45,
        },
    }


def xy(title, sql, x, y, desc=None):
    return {
        "type": "xychart",
        "title": title,
        "description": desc or "",
        "targets": [target(sql, "table")],
        "fieldConfig": {
            "defaults": {"custom": {"show": "points", "pointSize": {"fixed": 6}}},
            "overrides": [],
        },
        "options": {
            "seriesMapping": "auto",
            "dims": {"x": x},
            "series": [{"x": x, "y": y}],
            "legend": {"displayMode": "list", "placement": "bottom"},
        },
    }


def var_query(name, sql, multi=False, include_all=False, label=None):
    return {
        "type": "query",
        "name": name,
        "label": label or name,
        "datasource": DS,
        "query": sql,
        "definition": sql,
        "refresh": 2,
        "sort": 1,
        "multi": multi,
        "includeAll": include_all,
        "allValue": None,
        "current": {},
        "options": [],
    }


def var_const(name, value):
    """Hidden, so every dashboard's controls bar stays one line and the tabs below it do not move."""
    return {
        "type": "constant",
        "name": name,
        "query": value,
        "hide": 2,
        "current": {"text": value, "value": value},
        "options": [{"text": value, "value": value, "selected": True}],
    }


SYM = var_query(
    "sym", "SELECT DISTINCT sym FROM spread WHERE ts > now() - INTERVAL '2 days' ORDER BY 1"
)
SYMS = var_query(
    "sym",
    "SELECT DISTINCT sym FROM spread WHERE ts > now() - INTERVAL '2 days' ORDER BY 1",
    multi=True,
    include_all=True,
)
PAIR = var_query(
    "pair",
    "SELECT DISTINCT pair FROM spread WHERE sym IN (${sym:sqlstring}) AND ts > now() - INTERVAL '2 days' ORDER BY 1",
)
PAIRS = var_query(
    "pair",
    "SELECT DISTINCT pair FROM spread WHERE ts > now() - INTERVAL '2 days' ORDER BY 1",
    multi=True,
    include_all=True,
)
BAND = var_const("band", "2")
EXIT = var_const("exitbp", "0.5")
EP_FILTER = "sym IN (${sym:sqlstring}) AND pair IN (${pair:sqlstring}) AND $__timeFilter(start_ts)"
# edge of a maker/taker trade in USD from the journal: realised bp on the io notional
MAKER_USD = "real_bp / 1e4 * sz * px_io"
TAKER_USD = "real_bp / 1e4 * sz_io * px_io"

RED, ORANGE, GREEN = "red", "orange", "green"


def overview():
    b = Board("pm-overview", "Pair monitor / Overview", time_from="now-24h")
    b.row("Now")
    b.add(
        stat(
            "Total equity",
            "SELECT equity FROM equity_1m ORDER BY ts DESC LIMIT 1",
            "currencyUSD",
            "Sum of the latest snapshot of every venue (HL spot free USDC + xyz + io account values + Lighter).",
        ),
        4,
        4,
    )
    b.add(
        stat(
            "Equity change (range)",
            """
        WITH e AS (SELECT ts, equity FROM equity_1m WHERE $__timeFilter(ts))
        SELECT (SELECT equity FROM e ORDER BY ts DESC LIMIT 1) - (SELECT equity FROM e ORDER BY ts LIMIT 1)
             - coalesce((SELECT sum(usdc) FROM flow WHERE $__timeFilter(ts) AND kind IN ('deposit','withdraw')), 0) AS change""",
            "currencyUSD",
            "Last minus first total equity in the range, net of deposits/withdrawals.",
            [{"color": RED, "value": None}, {"color": GREEN, "value": 0}],
        ),
        4,
        4,
    )
    b.add(
        stat(
            "Bot P&L (status line)",
            "SELECT pnl FROM bot_status ORDER BY ts DESC LIMIT 1",
            "currencyUSD",
            "pair_arb's own pnl since its start (status line).",
            [{"color": RED, "value": None}, {"color": GREEN, "value": 0}],
            4,
        ),
        4,
        4,
    )
    b.add(
        stat(
            "Maker+taker edge (range)",
            f"""
        SELECT coalesce((SELECT sum({MAKER_USD}) FROM bot_maker WHERE $__timeFilter(ts)),0)
             + coalesce((SELECT sum({TAKER_USD}) FROM bot_taker WHERE $__timeFilter(ts)),0) AS edge""",
            "currencyUSD",
            "Sum of realised pair edge (real_bp x io notional) of the bot's maker and taker trades.",
            [{"color": RED, "value": None}, {"color": GREEN, "value": 0}],
            4,
        ),
        4,
        4,
    )
    b.add(
        stat(
            "Fees (range)",
            "SELECT -coalesce(sum(fee),0) AS fees FROM fill WHERE $__timeFilter(ts)",
            "currencyUSD",
            "Exchange fees paid (negative = cost).",
        ),
        4,
        4,
    )
    b.add(
        stat(
            "Funding (range)",
            "SELECT coalesce(sum(usdc),0) FROM funding WHERE $__timeFilter(ts)",
            "currencyUSD",
            "Funding received (+) / paid (-).",
            [{"color": RED, "value": None}, {"color": GREEN, "value": 0}],
        ),
        4,
        4,
    )
    b.add(
        stat(
            "HL request headroom",
            "SELECT headroom FROM rate_limit ORDER BY ts DESC LIMIT 1",
            "none",
            "Address quota left: maker quoting stops below 1500, everything below 300.",
            [
                {"color": RED, "value": None},
                {"color": ORANGE, "value": 300},
                {"color": GREEN, "value": 1500},
            ],
            0,
        ),
        4,
        4,
    )
    b.add(
        stat(
            "Min liquidation distance",
            "SELECT min(liq_dist_pct) FROM position WHERE ts > now() - INTERVAL '1 minute' AND size <> 0",
            "percent",
            "Smallest adverse move any leg can take before liquidation (guard tops up below 8%, closes below 5%).",
            [
                {"color": RED, "value": None},
                {"color": ORANGE, "value": 5},
                {"color": GREEN, "value": 8},
            ],
        ),
        4,
        4,
    )
    b.add(
        stat(
            "Gross notional",
            "SELECT sum(abs(notional)) FROM position WHERE ts > now() - INTERVAL '1 minute'",
            "currencyUSD",
        ),
        4,
        4,
    )
    b.add(
        stat(
            "Open episodes",
            "SELECT count(*) FROM episode WHERE end_ts IS NULL AND start_ts > now() - INTERVAL '1 hour'",
            "none",
            "Pairs currently deviating beyond the band.",
            decimals=0,
        ),
        4,
        4,
    )
    b.add(
        stat(
            "Episodes today (CST)",
            "SELECT count(*) FROM episode WHERE start_ts > date_trunc('day', now() AT TIME ZONE 'Asia/Shanghai') AT TIME ZONE 'Asia/Shanghai'",
            "none",
            decimals=0,
        ),
        4,
        4,
    )
    b.add(
        stat(
            "Bot status age",
            "SELECT extract(epoch FROM now() - max(ts)) FROM bot_status",
            "s",
            "Seconds since the bot's last status line (written every minute; mirrored every 60 s).",
            [
                {"color": GREEN, "value": None},
                {"color": ORANGE, "value": 180},
                {"color": RED, "value": 600},
            ],
            0,
        ),
        4,
        4,
    )
    b.row("Equity and P&L")
    b.add(
        ts(
            "Total equity",
            'SELECT ts AS time, equity AS "total equity" FROM equity_1m WHERE $__timeFilter(ts) ORDER BY 1',
            unit="currencyUSD",
        ),
        12,
    )
    b.add(
        ts(
            "Equity by venue",
            "SELECT $__timeGroupAlias(ts, '1m'), venue AS metric, last(equity, ts) AS value FROM account WHERE $__timeFilter(ts) GROUP BY 1, 2 ORDER BY 1",
            unit="currencyUSD",
            stack=True,
        ),
        12,
    )
    b.add(
        ts(
            "Bot P&L and cash",
            "SELECT ts AS time, pnl, cash FROM bot_status WHERE $__timeFilter(ts) ORDER BY 1",
            unit="currencyUSD",
            desc="pnl: mark-to-market since the bot's start; cash: realised cash flow of its trades.",
        ),
        12,
    )
    b.add(
        ts(
            "Cumulative edge by symbol (bot journal)",
            f"""
        SELECT ts AS time, sym AS metric, sum(usd) OVER (PARTITION BY sym ORDER BY ts) AS value FROM (
          SELECT ts, sym, {MAKER_USD} AS usd FROM bot_maker WHERE $__timeFilter(ts)
          UNION ALL SELECT ts, sym, {TAKER_USD} FROM bot_taker WHERE $__timeFilter(ts)) t ORDER BY 1""",
            unit="currencyUSD",
        ),
        12,
    )
    b.row("Positions")
    b.add(
        table(
            "Positions (latest)",
            """
        SELECT DISTINCT ON (venue, sym) sym, venue, size, entry_px, mark_px, notional, upnl, liq_px, liq_dist_pct AS "liq %", margin, leverage, ts
        FROM position WHERE ts > now() - INTERVAL '2 minutes' AND size <> 0 ORDER BY venue, sym, ts DESC""",
            units={
                "notional": "currencyUSD",
                "upnl": "currencyUSD",
                "margin": "currencyUSD",
                "liq %": "percent",
            },
        ),
        14,
        8,
    )
    b.add(
        table(
            "Net exposure per symbol",
            "SELECT sym, net_size, net_notional, gross_notional, legs FROM net_exposure ORDER BY gross_notional DESC",
            units={"net_notional": "currencyUSD", "gross_notional": "currencyUSD"},
        ),
        10,
        8,
    )
    return b


def spreads():
    b = Board(
        "pm-spreads",
        "Pair monitor / Spreads",
        time_from="now-3h",
        variables=[SYM, PAIR, BAND, EXIT],
    )
    f = "sym IN (${sym:sqlstring}) AND pair IN (${pair:sqlstring}) AND $__timeFilter(ts)"
    b.annotations.append(
        {
            "name": "Episodes",
            "enable": True,
            "hide": True,
            "iconColor": "orange",
            "datasource": DS,
            "target": target(
                f"""SELECT confirm_ts AS time, coalesce(end_ts, now()) AS "timeEnd",
                              pair || ' dir ' || dir || ' peak ' || round(peak_dev_bp::numeric, 1) || ' bp, ' || coalesce(end_reason, 'open') AS text
                              FROM episode WHERE {EP_FILTER}""",
                "table",
            ),
        }
    )
    b.row("Premium against the midline")
    b.add(
        ts(
            "Premium, midline and band",
            f"""
        SELECT ts AS time, pair || ' prem' AS metric, prem_bp AS value FROM spread WHERE {f}
        UNION ALL SELECT ts, pair || ' midline', midline_bp FROM spread WHERE {f}
        UNION ALL SELECT ts, pair || ' +band', midline_bp + $band FROM spread WHERE {f}
        UNION ALL SELECT ts, pair || ' -band', midline_bp - $band FROM spread WHERE {f} ORDER BY 1""",
            unit="none",
            decimals=2,
            desc="prem = (mid_a / mid_b - 1) x 1e4; midline = rolling 1 h in-session median (pair_arb's).",
        ),
        24,
        9,
    )
    b.add(
        ts(
            "Deviation from the midline",
            f"SELECT ts AS time, pair AS metric, dev_bp AS value FROM spread WHERE {f} ORDER BY 1",
            unit="none",
            decimals=2,
            thresholds=[{"color": "transparent", "value": None}, {"color": ORANGE, "value": 2}],
            desc="Orange line: band. Episodes are shaded (annotations).",
        ),
        24,
        8,
    )
    b.add(
        ts(
            "Edge toward the midline vs round-trip fees",
            f"""
        SELECT ts AS time, pair || ' sell a' AS metric, sell_a_vs_mid_bp AS value FROM spread WHERE {f}
        UNION ALL SELECT ts, pair || ' buy a', buy_a_vs_mid_bp FROM spread WHERE {f}
        UNION ALL SELECT ts, pair || ' rt fee', rt_fee_bp FROM spread WHERE {f} ORDER BY 1""",
            unit="none",
            decimals=2,
            desc="Touch-to-touch edge of trading toward the midline now (positive = capturable before exit costs); rt fee = taker fees of a full round trip.",
        ),
        12,
    )
    b.add(
        ts(
            "Touch spreads",
            f"""
        SELECT ts AS time, pair || ' a' AS metric, spread_a_bp AS value FROM spread WHERE {f}
        UNION ALL SELECT ts, pair || ' b', spread_b_bp FROM spread WHERE {f} ORDER BY 1""",
            unit="none",
            decimals=2,
        ),
        12,
    )
    b.row("Books")
    bf = "sym IN (${sym:sqlstring}) AND $__timeFilter(ts)"
    b.add(
        ts(
            "Mid by venue",
            f"SELECT ts AS time, venue AS metric, mid AS value FROM book WHERE {bf} ORDER BY 1",
        ),
        12,
    )
    b.add(
        ts(
            "Top-of-book size (USD)",
            f"SELECT $__timeGroupAlias(ts, '10s'), venue || ' bid' AS metric, avg(bid_sz * bid) AS value FROM book WHERE {bf} GROUP BY 1, 2 "
            f"UNION ALL SELECT $__timeGroupAlias(ts, '10s'), venue || ' ask', avg(ask_sz * ask) FROM book WHERE {bf} GROUP BY 1, 2 ORDER BY 1",
            unit="currencyUSD",
        ),
        12,
    )
    b.add(
        ts(
            "Book age (ms)",
            f"SELECT $__timeGroupAlias(ts, '10s'), venue AS metric, max(age_ms) AS value FROM book WHERE {bf} GROUP BY 1, 2 ORDER BY 1",
            unit="ms",
            desc="Time since the last BBO update at sampling; samples older than 10 s are excluded from the premium.",
        ),
        12,
    )
    b.add(
        ts(
            "Bot's view (io/xyz): premium and midline",
            "SELECT ts AS time, sym || ' bot prem' AS metric, prem_bp AS value FROM bot_status_sym WHERE sym IN (${sym:sqlstring}) AND $__timeFilter(ts) "
            "UNION ALL SELECT ts, sym || ' bot midline', midline_bp FROM bot_status_sym WHERE sym IN (${sym:sqlstring}) AND $__timeFilter(ts) ORDER BY 1",
            desc="From the bot's once-a-minute status line; compare with hl_io/hl_xyz above.",
        ),
        12,
    )
    b.row("Episodes")
    b.add(
        table(
            "Episodes",
            f"""
        SELECT start_ts, pair, dir, phase, round(confirm_dev_bp::numeric,2) AS confirm_dev, round(peak_dev_bp::numeric,2) AS peak_dev,
               round(peak_edge_bp::numeric,2) AS peak_edge, round((peak_edge_bp - rt_fee_bp)::numeric,2) AS peak_edge_net,
               round(half_s::numeric,1) AS half_s, round(duration_s::numeric,1) AS duration_s, coalesce(end_reason,'open') AS end
        FROM episode WHERE {EP_FILTER} ORDER BY start_ts DESC LIMIT 500""",
            units={"half_s": "s", "duration_s": "s"},
        ),
        24,
        10,
    )
    b.add(
        ts(
            "Bot trades on this symbol (realised bp)",
            "SELECT ts AS time, 'maker' AS metric, real_bp AS value FROM bot_maker WHERE sym IN (${sym:sqlstring}) AND $__timeFilter(ts) "
            "UNION ALL SELECT ts, 'taker', real_bp FROM bot_taker WHERE sym IN (${sym:sqlstring}) AND $__timeFilter(ts) ORDER BY 1",
            points=True,
        ),
        24,
        7,
    )
    return b


def convergence():
    b = Board(
        "pm-convergence", "Pair monitor / Convergence", time_from="now-7d", variables=[SYMS, PAIRS]
    )
    b.row("Summary")
    b.add(
        table(
            "Episodes by pair",
            f"""
        SELECT sym, pair, count(*) AS episodes,
          round(100.0 * avg((end_reason = 'converged')::int), 1) AS "converged %",
          round(100.0 * avg((end_reason = 'timeout')::int), 1) AS "timeout %",
          round(percentile_cont(0.5) WITHIN GROUP (ORDER BY duration_s) FILTER (WHERE end_reason = 'converged')::numeric, 1) AS "p50 converge s",
          round(percentile_cont(0.9) WITHIN GROUP (ORDER BY duration_s) FILTER (WHERE end_reason = 'converged')::numeric, 1) AS "p90 converge s",
          round(percentile_cont(0.5) WITHIN GROUP (ORDER BY half_s)::numeric, 1) AS "p50 half s",
          round(avg(abs(peak_dev_bp))::numeric, 2) AS "avg |peak dev|",
          round(avg(peak_edge_bp)::numeric, 2) AS "avg peak edge",
          round(avg(peak_edge_bp - rt_fee_bp)::numeric, 2) AS "avg net edge",
          round(100.0 * avg((peak_edge_bp > rt_fee_bp)::int), 1) AS "edge > fees %"
        FROM episode WHERE {EP_FILTER} GROUP BY 1, 2 ORDER BY episodes DESC""",
            desc="Duration = confirm to |dev| <= exit. Peak edge = best touch edge toward the midline while open; net = minus round-trip taker fees.",
        ),
        24,
        9,
    )
    b.add(
        table(
            "By US session phase",
            f"""
        SELECT phase, count(*) AS episodes, round(100.0 * avg((end_reason = 'converged')::int), 1) AS "converged %",
          round(percentile_cont(0.5) WITHIN GROUP (ORDER BY duration_s) FILTER (WHERE end_reason = 'converged')::numeric, 1) AS "p50 converge s",
          round(avg(abs(peak_dev_bp))::numeric, 2) AS "avg |peak dev|", round(avg(peak_edge_bp - rt_fee_bp)::numeric, 2) AS "avg net edge"
        FROM episode WHERE {EP_FILTER} GROUP BY 1 ORDER BY 2 DESC""",
        ),
        12,
        7,
    )
    b.add(
        table(
            "By end reason",
            f"""
        SELECT coalesce(end_reason, 'open') AS reason, count(*) AS episodes, round(avg(duration_s)::numeric, 1) AS "avg s",
          round(avg(abs(peak_dev_bp))::numeric, 2) AS "avg |peak dev|", round(avg(abs(end_dev_bp))::numeric, 2) AS "avg |end dev|"
        FROM episode WHERE {EP_FILTER} GROUP BY 1 ORDER BY 2 DESC""",
        ),
        12,
        7,
    )
    b.row("Distributions")
    b.add(
        bars(
            "Time to converge (s)",
            f"""
        SELECT CASE WHEN duration_s < 5 THEN '0-5' WHEN duration_s < 15 THEN '5-15' WHEN duration_s < 30 THEN '15-30'
                    WHEN duration_s < 60 THEN '30-60' WHEN duration_s < 120 THEN '60-120' WHEN duration_s < 300 THEN '120-300'
                    WHEN duration_s < 900 THEN '300-900' ELSE '900+' END AS bucket, count(*) AS episodes
        FROM episode WHERE {EP_FILTER} AND end_reason = 'converged'
        GROUP BY 1 ORDER BY min(duration_s)""",
        ),
        12,
    )
    b.add(
        bars(
            "Peak |deviation| (bp)",
            f"""
        SELECT bucket, episodes FROM (
          SELECT least(floor(abs(peak_dev_bp)), 12) AS k,
                 CASE WHEN abs(peak_dev_bp) >= 12 THEN '12+' ELSE floor(abs(peak_dev_bp))::int::text END AS bucket, count(*) AS episodes
          FROM episode WHERE {EP_FILTER} GROUP BY 1, 2) t ORDER BY k""",
        ),
        12,
    )
    b.add(
        bars(
            "Episodes by hour (New York)",
            f"""
        SELECT lpad(extract(hour FROM start_ts AT TIME ZONE 'America/New_York')::text, 2, '0') AS hour, count(*) AS episodes,
               round(percentile_cont(0.5) WITHIN GROUP (ORDER BY duration_s)::numeric, 1) AS "p50 s"
        FROM episode WHERE {EP_FILTER} GROUP BY 1 ORDER BY 1""",
        ),
        12,
    )
    b.add(
        xy(
            "Peak deviation vs time to converge",
            f"""
        SELECT abs(peak_dev_bp) AS peak_dev_bp, duration_s FROM episode WHERE {EP_FILTER} AND end_reason = 'converged'""",
            "peak_dev_bp",
            "duration_s",
        ),
        12,
    )
    b.row("Over time")
    b.add(
        ts(
            "Episodes per hour",
            f"SELECT time_bucket('1 hour', start_ts) AS time, pair AS metric, count(*) AS value FROM episode WHERE {EP_FILTER} GROUP BY 1, 2 ORDER BY 1",
            stack=True,
        ),
        12,
    )
    b.add(
        ts(
            "Median time to converge per hour (s)",
            f"""
        SELECT time_bucket('1 hour', start_ts) AS time, pair AS metric, percentile_cont(0.5) WITHIN GROUP (ORDER BY duration_s) AS value
        FROM episode WHERE {EP_FILTER} AND end_reason = 'converged' GROUP BY 1, 2 ORDER BY 1""",
            unit="s",
        ),
        12,
    )
    sf = "sym IN (${sym:sqlstring}) AND pair IN (${pair:sqlstring}) AND $__timeFilter(ts)"
    b.add(
        ts(
            "AR(1) half-life of the deviation (s)",
            f"SELECT ts AS time, sym || ' ' || pair AS metric, half_life_s AS value FROM spread_stats WHERE {sf} ORDER BY 1",
            unit="s",
            desc="Rolling 1 h AR(1) fit of 1 s deviations: seconds for a deviation to halve.",
        ),
        12,
    )
    b.add(
        ts(
            "Deviation std (bp) and time outside the band",
            f"""
        SELECT ts AS time, sym || ' ' || pair || ' std' AS metric, dev_std_bp AS value FROM spread_stats WHERE {sf}
        UNION ALL SELECT ts, sym || ' ' || pair || ' % outside', 100 * frac_outside FROM spread_stats WHERE {sf} ORDER BY 1""",
        ),
        12,
    )
    b.add(
        ts(
            "Premium level by pair (1 min mean)",
            f"""
        SELECT $__timeGroupAlias(ts, '1m'), sym || ' ' || pair AS metric, avg(prem_bp) AS value FROM spread WHERE {sf} GROUP BY 1, 2 ORDER BY 1""",
            desc="Persistent cross-venue premium: the level the deviations revert to.",
        ),
        24,
        9,
    )
    return b


def trades():
    b = Board("pm-trades", "Pair monitor / Bot trades", time_from="now-24h", variables=[SYMS])
    sf = "sym IN (${sym:sqlstring}) AND $__timeFilter(ts)"
    b.row("Per symbol")
    b.add(
        table(
            "Maker trades by symbol",
            f"""
        SELECT sym, count(*) AS trades, round(avg(plan_bp)::numeric,2) AS "avg plan bp", round(avg(real_bp)::numeric,2) AS "avg real bp",
          round(avg(real_bp - plan_bp)::numeric,2) AS "avg slip bp", round(sum({MAKER_USD})::numeric,4) AS "edge USD",
          round(sum(sz * px_io)::numeric,0) AS "notional USD", round(percentile_cont(0.5) WITHIN GROUP (ORDER BY hedge_delay_ms)::numeric,0) AS "p50 hedge ms",
          round(percentile_cont(0.9) WITHIN GROUP (ORDER BY hedge_delay_ms)::numeric,0) AS "p90 hedge ms", round(100.0*avg((real_bp > 0)::int),1) AS "win %"
        FROM bot_maker WHERE {sf} GROUP BY 1 ORDER BY trades DESC""",
            units={"edge USD": "currencyUSD"},
        ),
        24,
        7,
    )
    b.add(
        table(
            "Taker trades by symbol",
            f"""
        SELECT sym, count(*) AS trades, round(avg(plan_bp)::numeric,2) AS "avg plan bp", round(avg(real_bp)::numeric,2) AS "avg real bp",
          round(avg(real_bp - plan_bp)::numeric,2) AS "avg slip bp", round(sum({TAKER_USD})::numeric,4) AS "edge USD",
          round(sum(sz_io * px_io)::numeric,0) AS "notional USD", round(100.0*avg((real_bp > 0)::int),1) AS "win %"
        FROM bot_taker WHERE {sf} GROUP BY 1 ORDER BY trades DESC""",
            units={"edge USD": "currencyUSD"},
        ),
        24,
        6,
    )
    b.row("Trades over time")
    b.add(
        ts(
            "Maker: planned vs realised bp",
            f"SELECT ts AS time, sym || ' plan' AS metric, plan_bp AS value FROM bot_maker WHERE {sf} "
            f"UNION ALL SELECT ts, sym || ' real', real_bp FROM bot_maker WHERE {sf} ORDER BY 1",
            points=True,
        ),
        12,
    )
    b.add(
        ts(
            "Maker: hedge delay (ms)",
            f"SELECT ts AS time, sym AS metric, hedge_delay_ms AS value FROM bot_maker WHERE {sf} ORDER BY 1",
            points=True,
            unit="ms",
            desc="Maker fill to hedge fill on xyz.",
        ),
        12,
    )
    b.add(
        ts(
            "Taker: planned vs realised bp",
            f"SELECT ts AS time, sym || ' plan' AS metric, plan_bp AS value FROM bot_taker WHERE {sf} "
            f"UNION ALL SELECT ts, sym || ' real', real_bp FROM bot_taker WHERE {sf} ORDER BY 1",
            points=True,
        ),
        12,
    )
    b.add(
        ts(
            "Trades per hour",
            f"SELECT time_bucket('1 hour', ts) AS time, 'maker' AS metric, count(*) AS value FROM bot_maker WHERE {sf} GROUP BY 1 "
            f"UNION ALL SELECT time_bucket('1 hour', ts), 'taker', count(*) FROM bot_taker WHERE {sf} GROUP BY 1 ORDER BY 1",
            stack=True,
        ),
        12,
    )
    b.row("Quoting (event journal)")
    ef = "$__timeFilter(ts) AND (sym IS NULL OR sym IN (${sym:sqlstring}))"
    b.add(
        ts(
            "Events per hour",
            f"SELECT time_bucket('1 hour', ts) AS time, ev AS metric, count(*) AS value FROM bot_event WHERE {ef} "
            "AND ev IN ('quote_post','quote_cancel','maker_fill','hedge','signal','order_sent','quote_rejected') GROUP BY 1, 2 ORDER BY 1",
        ),
        12,
    )
    b.add(
        ts(
            "Maker fill rate per hour (%)",
            f"""
        SELECT time_bucket('1 hour', ts) AS time, 100.0 * count(*) FILTER (WHERE ev = 'maker_fill') / nullif(count(*) FILTER (WHERE ev = 'quote_post'), 0) AS "fills / quotes"
        FROM bot_event WHERE {ef} GROUP BY 1 ORDER BY 1""",
            unit="percent",
        ),
        12,
    )
    b.add(
        ts(
            "Quote life (ms): p50 / p90",
            f"""
        SELECT time_bucket('1 hour', ts) AS time, percentile_cont(0.5) WITHIN GROUP (ORDER BY (body->>'life_ms')::float) AS p50,
               percentile_cont(0.9) WITHIN GROUP (ORDER BY (body->>'life_ms')::float) AS p90
        FROM bot_event WHERE {ef} AND ev = 'quote_done' GROUP BY 1 ORDER BY 1""",
            unit="ms",
        ),
        12,
    )
    b.add(
        ts(
            "Round trips (ms): order and quote ack",
            f"""
        SELECT time_bucket('10 minutes', ts) AS time, ev || ' p50' AS metric, percentile_cont(0.5) WITHIN GROUP (ORDER BY (body->>'rtt_ms')::float) AS value
        FROM bot_event WHERE {ef} AND ev IN ('order_result','quote_ack') GROUP BY 1, 2
        UNION ALL SELECT time_bucket('10 minutes', ts), ev || ' p90', percentile_cont(0.9) WITHIN GROUP (ORDER BY (body->>'rtt_ms')::float)
        FROM bot_event WHERE {ef} AND ev IN ('order_result','quote_ack') GROUP BY 1, 2 ORDER BY 1""",
            unit="ms",
        ),
        12,
    )
    b.add(
        table(
            "Quote outcomes",
            f"""
        SELECT body->>'why' AS outcome, count(*) AS quotes, round(avg((body->>'life_ms')::float)::numeric,0) AS "avg life ms",
               round(avg((body->>'filled')::float)::numeric,4) AS "avg filled"
        FROM bot_event WHERE {ef} AND ev = 'quote_done' GROUP BY 1 ORDER BY 2 DESC""",
        ),
        12,
        7,
    )
    b.add(
        table(
            "Problems (halt, pause, rejections, errors)",
            """
        SELECT ts, ev, sym, body::text AS detail FROM bot_event WHERE $__timeFilter(ts)
        AND ev IN ('halt','pause','recon_adopt','order_error','recon_mismatch','recon_error','quote_rejected','quote_post_error','build_failed','lagged','liq_topup','liq_close','imbalance_fix')
        ORDER BY ts DESC LIMIT 200""",
        ),
        12,
        7,
    )
    b.row("Journal")
    b.add(
        table(
            "Maker trades",
            f"SELECT ts, sym, dir, sz, px_io, px_xyz, plan_bp, real_bp, hedge_delay_ms, pnl_after FROM bot_maker WHERE {sf} ORDER BY ts DESC LIMIT 500",
        ),
        24,
        9,
    )
    b.add(
        table(
            "Taker trades",
            f"SELECT ts, sym, dir, sz_io, sz_xyz, px_io, px_xyz, plan_bp, real_bp, pnl_after FROM bot_taker WHERE {sf} ORDER BY ts DESC LIMIT 500",
        ),
        24,
        8,
    )
    return b


def fills():
    b = Board("pm-fills", "Pair monitor / Fills and costs", time_from="now-24h", variables=[SYMS])
    sf = "sym IN (${sym:sqlstring}) AND $__timeFilter(ts)"
    b.row("Volume and fees")
    b.add(
        ts(
            "Volume per hour by venue",
            f"SELECT time_bucket('1 hour', ts) AS time, venue || CASE WHEN maker THEN ' maker' ELSE ' taker' END AS metric, sum(notional) AS value "
            f"FROM fill WHERE {sf} GROUP BY 1, 2 ORDER BY 1",
            unit="currencyUSD",
            stack=True,
        ),
        12,
    )
    b.add(
        ts(
            "Cumulative fees by venue",
            f"SELECT ts AS time, venue AS metric, sum(fee) OVER (PARTITION BY venue ORDER BY ts) AS value FROM fill WHERE {sf} ORDER BY 1",
            unit="currencyUSD",
        ),
        12,
    )
    b.add(
        table(
            "By venue and symbol",
            f"""
        SELECT venue, sym, count(*) AS fills, round(sum(notional)::numeric,0) AS volume, round(100.0*avg(maker::int),1) AS "maker %",
               round(sum(fee)::numeric,4) AS fees, round((1e4*sum(fee)/nullif(sum(notional),0))::numeric,3) AS "fee bp",
               round(sum(closed_pnl)::numeric,4) AS "closed pnl (HL)"
        FROM fill WHERE {sf} GROUP BY 1, 2 ORDER BY volume DESC""",
            units={"volume": "currencyUSD", "fees": "currencyUSD"},
        ),
        24,
        7,
    )
    b.row("Funding")
    b.add(
        ts(
            "Cumulative funding by symbol",
            f"SELECT ts AS time, venue || ' ' || sym AS metric, sum(usdc) OVER (PARTITION BY venue, sym ORDER BY ts) AS value FROM funding WHERE {sf} ORDER BY 1",
            unit="currencyUSD",
        ),
        12,
    )
    b.add(
        ts(
            "Funding rate (per interval, bp)",
            f"SELECT ts AS time, venue || ' ' || sym AS metric, 1e4 * rate AS value FROM funding WHERE {sf} ORDER BY 1",
            points=True,
        ),
        12,
    )
    b.row("Fills")
    b.add(
        table(
            "Fills",
            f"SELECT ts, venue, sym, side, px, sz, notional, fee, maker, order_id, cli_order_id, closed_pnl FROM fill WHERE {sf} ORDER BY ts DESC LIMIT 1000",
            units={"notional": "currencyUSD"},
        ),
        24,
        10,
    )
    b.add(
        table(
            "Flows (deposits, withdrawals, transfers)",
            "SELECT ts, venue, kind, usdc FROM flow WHERE $__timeFilter(ts) ORDER BY ts DESC LIMIT 200",
        ),
        24,
        6,
    )
    return b


def risk():
    b = Board("pm-risk", "Pair monitor / Risk", time_from="now-24h", variables=[SYMS])
    sf = "sym IN (${sym:sqlstring}) AND $__timeFilter(ts)"
    b.row("Exposure")
    b.add(
        ts(
            "Position notional by leg",
            f"SELECT $__timeGroupAlias(ts, '1m'), venue || ' ' || sym AS metric, last(notional, ts) AS value FROM position WHERE {sf} GROUP BY 1, 2 ORDER BY 1",
            unit="currencyUSD",
        ),
        12,
    )
    b.add(
        ts(
            "Net notional per symbol (all venues)",
            f"""
        SELECT time, sym AS metric, sum(n) AS value FROM (
          SELECT $__timeGroupAlias(ts, '1m'), venue, sym, last(notional, ts) AS n FROM position WHERE {sf} GROUP BY 1, 2, 3) t
        GROUP BY 1, 2 ORDER BY 1""",
            unit="currencyUSD",
            desc="Should stay near 0: the legs hedge each other.",
        ),
        12,
    )
    b.add(
        ts(
            "Liquidation distance by leg (%)",
            f"SELECT $__timeGroupAlias(ts, '1m'), venue || ' ' || sym AS metric, min(liq_dist_pct) AS value FROM position WHERE {sf} AND size <> 0 GROUP BY 1, 2 ORDER BY 1",
            unit="percent",
            thresholds=[
                {"color": RED, "value": None},
                {"color": ORANGE, "value": 5},
                {"color": "transparent", "value": 8},
            ],
            desc="Guard: top up below 8%, close below 5%.",
        ),
        12,
    )
    b.add(
        ts(
            "Isolated margin by leg",
            f"SELECT $__timeGroupAlias(ts, '1m'), venue || ' ' || sym AS metric, last(margin, ts) AS value FROM position WHERE {sf} AND size <> 0 GROUP BY 1, 2 ORDER BY 1",
            unit="currencyUSD",
        ),
        12,
    )
    b.row("Accounts")
    b.add(
        ts(
            "Free collateral by venue",
            "SELECT $__timeGroupAlias(ts, '1m'), venue AS metric, last(free, ts) AS value FROM account WHERE $__timeFilter(ts) GROUP BY 1, 2 ORDER BY 1",
            unit="currencyUSD",
        ),
        12,
    )
    b.add(
        ts(
            "Unrealised P&L by venue",
            "SELECT $__timeGroupAlias(ts, '1m'), venue AS metric, last(upnl, ts) AS value FROM account WHERE $__timeFilter(ts) AND upnl IS NOT NULL GROUP BY 1, 2 ORDER BY 1",
            unit="currencyUSD",
        ),
        12,
    )
    b.add(
        ts(
            "HL request headroom",
            "SELECT ts AS time, headroom, used, cap FROM rate_limit WHERE $__timeFilter(ts) ORDER BY 1",
            thresholds=[
                {"color": RED, "value": None},
                {"color": ORANGE, "value": 300},
                {"color": "transparent", "value": 1500},
            ],
            desc="cap = 10000 + cumulative volume; maker quoting needs 1500 left.",
        ),
        12,
    )
    b.add(
        ts(
            "Open orders by venue",
            "SELECT $__timeGroupAlias(ts, '1m'), venue AS metric, sum(n) AS value FROM (SELECT ts, venue, n FROM open_orders WHERE $__timeFilter(ts)) o GROUP BY 1, 2 ORDER BY 1",
        ),
        12,
    )
    b.row("Bot")
    b.add(
        ts(
            "Bot: liquidation buffer, quotes, headroom",
            'SELECT ts AS time, liq_pct AS "liq %", quotes, headroom / 100 AS "headroom / 100" FROM bot_status WHERE $__timeFilter(ts) ORDER BY 1',
        ),
        12,
    )
    b.add(
        ts(
            "Bot: positions per symbol (io leg)",
            f"SELECT ts AS time, sym AS metric, pos_io AS value FROM bot_status_sym WHERE {sf} ORDER BY 1",
        ),
        12,
    )
    b.add(
        ts(
            "Data freshness",
            """
        SELECT $__timeGroupAlias(ts, '1m'), venue AS metric, max(age_ms) / 1000 AS value FROM book WHERE $__timeFilter(ts) GROUP BY 1, 2 ORDER BY 1""",
            unit="s",
            desc="Oldest BBO per venue at sampling.",
        ),
        12,
    )
    b.add(
        table(
            "Paused / halted (status line)",
            "SELECT ts, in_session, paused FROM bot_status WHERE $__timeFilter(ts) AND paused <> 'None' ORDER BY ts DESC LIMIT 100",
        ),
        12,
    )
    return b


BOARDS = {
    "overview": overview,
    "spreads": spreads,
    "convergence": convergence,
    "trades": trades,
    "fills": fills,
    "risk": risk,
}


def expand(sql):
    """Grafana macros and variables replaced by plain SQL, for --check."""
    s = sql
    s = re.sub(r"\$__timeFilter\(([^)]+)\)", r"\1 > now() - INTERVAL '6 hours'", s)
    s = re.sub(r"\$__timeGroupAlias\(([^,]+),\s*'([^']+)'\)", r"time_bucket('\2', \1) AS time", s)
    s = s.replace("${sym:sqlstring}", "'SNDK','NBIS'").replace(
        "${pair:sqlstring}", "'hl_io/hl_xyz','lighter/hl_xyz'"
    )
    return s.replace("$band", "2").replace("$exitbp", "0.5")


def queries(board):
    for p in board["panels"]:
        for t in p.get("targets", []):
            yield p["title"], t["rawSql"]
    for a in board["annotations"]["list"]:
        if "target" in a:
            yield "annotation " + a["name"], a["target"]["rawSql"]
    for v in board["templating"]["list"]:
        if v["type"] == "query":
            yield "variable " + v["name"], v["query"]


def main():
    OUT.mkdir(exist_ok=True)
    bad = 0
    for name, build in BOARDS.items():
        board = build().json()
        (OUT / f"{name}.json").write_text(json.dumps(board, indent=2) + "\n")
        if "--check" in sys.argv:
            for title, sql in queries(board):
                r = subprocess.run(
                    [
                        "docker",
                        "exec",
                        "-i",
                        "pair-monitor-db-1",
                        "psql",
                        "-U",
                        "monitor",
                        "-d",
                        "monitor",
                        "-v",
                        "ON_ERROR_STOP=1",
                        "-q",
                    ],
                    input=f"EXPLAIN {expand(sql)};",
                    capture_output=True,
                    text=True,
                )
                if r.returncode:
                    bad += 1
                    print(f"[{name}] {title}: {r.stderr.strip()}")
        print(f"{name}: {len(board['panels'])} panels")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
