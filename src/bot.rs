//! pair_arb's data dir, mirrored read-only with rsync every `bot.sync_s` and ingested incrementally: taker and maker
//! trade CSVs, the JSONL event journal and the once-a-minute status lines of stdout.log. Offsets (lines, or bytes for
//! stdout.log) live in `ingest_offset`, so a restart resumes where it stopped.

use std::{collections::BTreeMap, path::Path, time::Duration};

use chrono::{DateTime, Datelike, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Asia::Shanghai;
use serde_json::Value;
use tokio::process::Command;
use tokio_postgres::Client;
use tracing::{info, warn};

use crate::{
    config::{BotCfg, expand},
    db::{Rec, Sink, ts_us},
};

const TAKER_COLS: &[&str] = &[
    "ts",
    "sym",
    "dir",
    "sz_io",
    "sz_xyz",
    "px_io",
    "px_xyz",
    "plan_bp",
    "real_bp",
    "pnl_after",
];
const MAKER_COLS: &[&str] = &[
    "ts",
    "sym",
    "dir",
    "sz",
    "px_io",
    "px_xyz",
    "plan_bp",
    "real_bp",
    "hedge_delay_ms",
    "pnl_after",
];

fn f(s: Option<&str>) -> Option<f64> {
    s.and_then(|x| x.trim().trim_start_matches('+').parse().ok())
}

/// `sent_us,sym,dir,sz_io,sz_xyz,px_io,px_xyz,plan_bp,real_bp,pnl_after`
pub fn taker_row(line: &str) -> Option<Rec> {
    let c: Vec<&str> = line.split(',').collect();
    let t: u64 = c.first()?.parse().ok()?;
    Some(Rec::new(
        "bot_taker",
        TAKER_COLS,
        vec![
            ts_us(t).into(),
            (*c.get(1)?).into(),
            c.get(2)?
                .parse::<i64>()
                .ok()
                .map_or(crate::db::Val::I(None), Into::into),
            f(c.get(3).copied()).into(),
            f(c.get(4).copied()).into(),
            f(c.get(5).copied()).into(),
            f(c.get(6).copied()).into(),
            f(c.get(7).copied()).into(),
            f(c.get(8).copied()).into(),
            f(c.get(9).copied()).into(),
        ],
    ))
}

/// `fill_us,sym,dir,sz,px_io,px_xyz,plan_bp,real_bp,hedge_delay_ms,pnl_after`
pub fn maker_row(line: &str) -> Option<Rec> {
    let c: Vec<&str> = line.split(',').collect();
    let t: u64 = c.first()?.parse().ok()?;
    Some(Rec::new(
        "bot_maker",
        MAKER_COLS,
        vec![
            ts_us(t).into(),
            (*c.get(1)?).into(),
            c.get(2)?
                .parse::<i64>()
                .ok()
                .map_or(crate::db::Val::I(None), Into::into),
            f(c.get(3).copied()).into(),
            f(c.get(4).copied()).into(),
            f(c.get(5).copied()).into(),
            f(c.get(6).copied()).into(),
            f(c.get(7).copied()).into(),
            f(c.get(8).copied()).into(),
            f(c.get(9).copied()).into(),
        ],
    ))
}

pub fn event_row(file: &str, line_no: i64, line: &str) -> Option<Rec> {
    let v: Value = serde_json::from_str(line).ok()?;
    let t = v["t_us"].as_u64()?;
    Some(Rec::new(
        "bot_event",
        &["file", "line", "ts", "ev", "sym", "body"],
        vec![
            file.into(),
            line_no.into(),
            ts_us(t).into(),
            v["ev"].as_str().unwrap_or_default().into(),
            v["sym"].as_str().map(str::to_string).into(),
            v.into(),
        ],
    ))
}

/// `MM-DD HH:MM:SS` in Beijing time, in the year that puts it closest before `now`.
fn status_time(stamp: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let year = now.with_timezone(&Shanghai).year();
    for y in [year, year - 1] {
        let naive =
            NaiveDateTime::parse_from_str(&format!("{y}-{stamp}"), "%Y-%m-%d %H:%M:%S").ok()?;
        let t = Shanghai
            .from_local_datetime(&naive)
            .single()?
            .with_timezone(&Utc);
        if t <= now + chrono::Duration::hours(1) {
            return Some(t);
        }
    }
    None
}

/// `10-09 15:52:19 session=true paused=None pnl=+0.3413 cash=+1.5606 headroom=Some(1743) free=33.3 quotes=1 liq=14.3%
/// | SNDK +0.0500/-0.0500 mid 1.52 prem +1.8 age 837/397ms | ... | {"hedge": 101, ...}`
pub fn status_rows(line: &str, now: DateTime<Utc>) -> Vec<Rec> {
    let parts: Vec<&str> = line.split(" | ").collect();
    let head = parts[0];
    if !head.contains(" session=") || head.len() < 15 {
        return Vec::new();
    }
    let Some(ts) = status_time(&head[..14], now) else {
        return Vec::new();
    };
    let kv: BTreeMap<&str, &str> = head[15..]
        .split(' ')
        .filter_map(|t| t.split_once('='))
        .collect();
    let inner = |k: &str| {
        kv.get(k).map(|v| {
            v.trim_start_matches("Some(")
                .trim_end_matches(')')
                .trim_end_matches('%')
        })
    };
    let counts = parts
        .last()
        .filter(|p| p.starts_with('{'))
        .and_then(|p| serde_json::from_str::<Value>(p).ok())
        .unwrap_or(Value::Null);
    let mut out = vec![
        Rec::new(
            "bot_status",
            &[
                "ts",
                "in_session",
                "paused",
                "pnl",
                "cash",
                "headroom",
                "free",
                "quotes",
                "liq_pct",
                "counts",
            ],
            vec![
                ts.into(),
                kv.get("session").map(|s| *s == "true").into(),
                kv.get("paused").map(|s| s.to_string()).into(),
                f(inner("pnl")).into(),
                f(inner("cash")).into(),
                f(inner("headroom")).into(),
                f(inner("free")).into(),
                inner("quotes")
                    .and_then(|q| q.parse::<i64>().ok())
                    .map_or(crate::db::Val::I(None), Into::into),
                f(inner("liq")).into(),
                counts.into(),
            ],
        )
        .upsert(&["ts"]),
    ];
    for p in &parts[1..] {
        let tok: Vec<&str> = p.split_whitespace().collect();
        if tok.len() < 2 || tok[0].starts_with('{') {
            continue;
        }
        let (io, xyz) = tok[1].split_once('/').unwrap_or((tok[1], ""));
        let after = |k: &str| {
            tok.iter()
                .position(|t| *t == k)
                .and_then(|i| tok.get(i + 1))
                .copied()
        };
        let (age_io, age_xyz) = after("age")
            .map(|a| a.trim_end_matches("ms"))
            .and_then(|a| a.split_once('/'))
            .map_or((None, None), |(a, b)| (f(Some(a)), f(Some(b))));
        out.push(
            Rec::new(
                "bot_status_sym",
                &[
                    "ts",
                    "sym",
                    "pos_io",
                    "pos_xyz",
                    "midline_bp",
                    "prem_bp",
                    "age_io_ms",
                    "age_xyz_ms",
                ],
                vec![
                    ts.into(),
                    tok[0].into(),
                    f(Some(io)).into(),
                    f(Some(xyz)).into(),
                    f(after("mid")).into(),
                    f(after("prem")).into(),
                    age_io.into(),
                    age_xyz.into(),
                ],
            )
            .upsert(&["ts", "sym"]),
        );
    }
    out
}

async fn offsets(db: &Client) -> BTreeMap<String, i64> {
    match db.query("SELECT source, pos FROM ingest_offset", &[]).await {
        Ok(rows) => rows.iter().map(|r| (r.get(0), r.get(1))).collect(),
        Err(e) => {
            warn!("ingest offsets: {e}");
            BTreeMap::new()
        },
    }
}

async fn save_offset(db: &Client, source: &str, pos: i64) {
    let sql = "INSERT INTO ingest_offset (source, pos) VALUES ($1, $2) ON CONFLICT (source) DO UPDATE SET pos = EXCLUDED.pos";
    if let Err(e) = db.execute(sql, &[&source, &pos]).await {
        warn!("save offset {source}: {e}");
    }
}

async fn rsync(cfg: &BotCfg, local: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(local)?;
    let ssh = format!(
        "ssh -T -o BatchMode=yes -o IdentitiesOnly=yes -o ConnectTimeout=20 -i {}",
        expand(&cfg.ssh_key).display()
    );
    let out = Command::new("rsync")
        .args(["-az", "--rsync-path=nice -n 10 rsync", "-e", &ssh])
        .arg(format!("{}:{}", cfg.host, cfg.remote_dir))
        .arg(format!("{}/", local.display()))
        .stdin(std::process::Stdio::null())
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!("rsync: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Lines of `path` after the first `skip`; returns them with the new line count.
fn new_lines(path: &Path, skip: i64) -> anyhow::Result<(Vec<String>, i64)> {
    let text = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len() as i64;
    let start = if skip > total { 0 } else { skip };
    // a line without its newline may still be growing
    let complete = if text.ends_with('\n') {
        total
    } else {
        total - 1
    };
    let fresh = lines[start as usize..complete.max(start) as usize]
        .iter()
        .map(|s| s.to_string())
        .collect();
    Ok((fresh, complete.max(start)))
}

async fn ingest(db: &Client, local: &Path, sink: &Sink) -> anyhow::Result<usize> {
    let mut seen = offsets(db).await;
    let mut n = 0;
    let mut names: Vec<String> = std::fs::read_dir(local)?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    names.sort();
    for name in names {
        let path = local.join(&name);
        if name == "stdout.log" {
            let bytes = std::fs::read(&path)?;
            let mut pos = seen.get(&name).copied().unwrap_or_default();
            if pos as usize > bytes.len() {
                pos = 0;
            }
            let chunk = &bytes[pos as usize..];
            let end = chunk.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            let now = Utc::now();
            for line in String::from_utf8_lossy(&chunk[..end]).lines() {
                for r in status_rows(line, now) {
                    sink.send(r);
                    n += 1;
                }
            }
            let new_pos = pos + end as i64;
            seen.insert(name.clone(), new_pos);
            save_offset(db, &name, new_pos).await;
            continue;
        }
        let parse: fn(&str, i64, &str) -> Option<Rec> = match name.as_str() {
            "trades.csv" => |_, _, l| taker_row(l),
            "maker_trades.csv" => |_, _, l| maker_row(l),
            n if n.starts_with("events-") && n.ends_with(".jsonl") => event_row,
            _ => continue,
        };
        let skip = seen.get(&name).copied().unwrap_or_default();
        let (lines, total) = new_lines(&path, skip)?;
        if lines.is_empty() {
            continue;
        }
        let first = if skip > total { 0 } else { skip };
        for (i, l) in lines.iter().enumerate() {
            if let Some(r) = parse(&name, first + i as i64, l) {
                sink.send(r);
                n += 1;
            }
        }
        save_offset(db, &name, total).await;
    }
    Ok(n)
}

pub fn spawn(cfg: BotCfg, db: Client, sink: Sink) {
    tokio::spawn(async move {
        let local = expand(&cfg.local_dir);
        let mut tick = tokio::time::interval(Duration::from_secs(cfg.sync_s));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = rsync(&cfg, &local).await {
                warn!("bot sync: {e:#}");
            }
            match ingest(&db, &local, &sink).await {
                Ok(n) if n > 0 => info!("bot journal: {n} rows"),
                Ok(_) => {},
                Err(e) => warn!("bot ingest: {e:#}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Val;

    fn vals(r: &Rec) -> &[Val] {
        &r.vals
    }

    #[test]
    fn trade_csv_rows_parse() {
        let r = taker_row(
            "1791160461442326,EWY,1,0.150000,0.150000,192.11,192.1,-2.249,-2.249,-0.0190",
        )
        .unwrap();
        assert_eq!(r.table, "bot_taker");
        assert!(matches!(vals(&r)[1], Val::S(Some(ref s)) if s == "EWY"));
        assert!(matches!(vals(&r)[7], Val::F(Some(v)) if (v + 2.249).abs() < 1e-9));
        let m =
            maker_row("1791293290152949,DRAM,1,0.600000,61.14,61.108,-3.115,-6.386,655,-0.0184")
                .unwrap();
        assert!(matches!(vals(&m)[8], Val::F(Some(v)) if v == 655.0));
        assert!(taker_row("sent_us,sym,dir").is_none());
    }

    #[test]
    fn status_lines_parse_into_status_and_symbols() {
        let now = DateTime::parse_from_rfc3339("2026-10-09T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let line = r#"10-09 15:52:19 session=true paused=None pnl=+0.3413 cash=+1.5606 headroom=Some(1743) free=33.3 quotes=1 liq=14.3% | SNDK +0.0500/-0.0500 mid 1.52 prem +1.8 age 837/397ms | EWY -0.3800/+0.3800 mid 4.49 prem +7.6 age 24373/397ms | {"hedge": 101, "maker_fill": 101}"#;
        let rows = status_rows(line, now);
        assert_eq!(rows.len(), 3);
        let s = vals(&rows[0]);
        assert!(matches!(s[0], Val::T(Some(t)) if t.to_rfc3339() == "2026-10-09T07:52:19+00:00"));
        assert!(matches!(s[3], Val::F(Some(v)) if (v - 0.3413).abs() < 1e-9));
        assert!(matches!(s[5], Val::F(Some(v)) if v == 1743.0));
        assert!(matches!(s[8], Val::F(Some(v)) if v == 14.3));
        assert!(matches!(s[9], Val::J(ref j) if j["hedge"] == 101));
        let e = vals(&rows[2]);
        assert!(matches!(e[1], Val::S(Some(ref x)) if x == "EWY"));
        assert!(matches!(e[2], Val::F(Some(v)) if v == -0.38));
        assert!(matches!(e[5], Val::F(Some(v)) if v == 7.6));
        assert!(matches!(e[6], Val::F(Some(v)) if v == 24373.0));
        assert!(status_rows("10-09 15:52:19 some other log line", now).is_empty());
    }

    #[test]
    fn events_keep_the_whole_body() {
        let r = event_row(
            "events-1.jsonl",
            7,
            r#"{"ev":"quote_ack","rtt_ms":430,"sym":"DRAM","t_us":1791532276653206}"#,
        )
        .unwrap();
        assert!(matches!(vals(&r)[3], Val::S(Some(ref e)) if e == "quote_ack"));
        assert!(matches!(vals(&r)[5], Val::J(ref j) if j["rtt_ms"] == 430));
    }
}
