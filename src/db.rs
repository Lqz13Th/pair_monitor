//! Postgres (TimescaleDB) sink: rows go through a channel to one writer that batches them into multi-row inserts.

use std::{
    collections::BTreeMap,
    error::Error,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use tokio::sync::mpsc;
use tokio_postgres::{
    Client, NoTls,
    types::{IsNull, ToSql, Type, to_sql_checked},
};
use tracing::{error, info, warn};

const SCHEMA: &str = include_str!("../sql/schema.sql");
const MAX_PARAMS: usize = 30_000;

#[derive(Clone, Debug)]
pub enum Val {
    F(Option<f64>),
    I(Option<i64>),
    S(Option<String>),
    B(Option<bool>),
    T(Option<DateTime<Utc>>),
    J(serde_json::Value),
}

impl From<f64> for Val {
    fn from(v: f64) -> Self {
        Val::F(v.is_finite().then_some(v))
    }
}

impl From<Option<f64>> for Val {
    fn from(v: Option<f64>) -> Self {
        Val::F(v.filter(|x| x.is_finite()))
    }
}

impl From<i64> for Val {
    fn from(v: i64) -> Self {
        Val::I(Some(v))
    }
}

impl From<&str> for Val {
    fn from(v: &str) -> Self {
        Val::S(Some(v.to_string()))
    }
}

impl From<String> for Val {
    fn from(v: String) -> Self {
        Val::S(Some(v))
    }
}

impl From<Option<String>> for Val {
    fn from(v: Option<String>) -> Self {
        Val::S(v)
    }
}

impl From<bool> for Val {
    fn from(v: bool) -> Self {
        Val::B(Some(v))
    }
}

impl From<Option<bool>> for Val {
    fn from(v: Option<bool>) -> Self {
        Val::B(v)
    }
}

impl From<DateTime<Utc>> for Val {
    fn from(v: DateTime<Utc>) -> Self {
        Val::T(Some(v))
    }
}

impl From<Option<DateTime<Utc>>> for Val {
    fn from(v: Option<DateTime<Utc>>) -> Self {
        Val::T(v)
    }
}

impl From<serde_json::Value> for Val {
    fn from(v: serde_json::Value) -> Self {
        Val::J(v)
    }
}

impl ToSql for Val {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut tokio_postgres::types::private::BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        match self {
            Val::F(v) => v.to_sql(ty, out),
            Val::I(v) => v.to_sql(ty, out),
            Val::S(v) => v.to_sql(ty, out),
            Val::B(v) => v.to_sql(ty, out),
            Val::T(v) => v.to_sql(ty, out),
            Val::J(v) => v.to_sql(ty, out),
        }
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

pub fn ts_us(t_us: u64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_micros(t_us as i64).unwrap_or_default()
}

pub fn ts_ms(t_ms: u64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(t_ms as i64).unwrap_or_default()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OnConflict {
    Nothing,
    /// Overwrite every column but the key.
    Update(&'static [&'static str]),
}

#[derive(Clone, Debug)]
pub struct Rec {
    pub table: &'static str,
    pub cols: &'static [&'static str],
    pub vals: Vec<Val>,
    pub conflict: OnConflict,
}

impl Rec {
    pub fn new(table: &'static str, cols: &'static [&'static str], vals: Vec<Val>) -> Self {
        debug_assert_eq!(cols.len(), vals.len(), "{table}");
        Self {
            table,
            cols,
            vals,
            conflict: OnConflict::Nothing,
        }
    }

    pub fn upsert(mut self, key: &'static [&'static str]) -> Self {
        self.conflict = OnConflict::Update(key);
        self
    }
}

type Group = (&'static str, &'static [&'static str], OnConflict);

#[derive(Clone)]
pub struct Sink {
    tx: mpsc::UnboundedSender<Rec>,
    pending: Arc<AtomicUsize>,
}

impl Sink {
    pub fn send(&self, r: Rec) {
        self.pending.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(r);
    }

    /// Rows sent but not yet written.
    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::Relaxed)
    }
}

pub async fn connect(url: &str) -> anyhow::Result<Client> {
    let (client, conn) = tokio_postgres::connect(url, NoTls).await?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            error!("postgres connection: {e}");
        }
    });
    Ok(client)
}

pub async fn init(url: &str) -> anyhow::Result<Client> {
    let client = connect(url).await?;
    client.batch_execute(SCHEMA).await?;
    info!("schema applied");
    Ok(client)
}

fn insert_sql(table: &str, cols: &[&str], conflict: OnConflict, rows: usize) -> String {
    let mut sql = format!("INSERT INTO {table} ({}) VALUES ", cols.join(","));
    let mut n = 0;
    for r in 0..rows {
        if r > 0 {
            sql.push(',');
        }
        sql.push('(');
        for c in 0..cols.len() {
            if c > 0 {
                sql.push(',');
            }
            n += 1;
            sql.push_str(&format!("${n}"));
        }
        sql.push(')');
    }
    match conflict {
        OnConflict::Nothing => sql.push_str(" ON CONFLICT DO NOTHING"),
        OnConflict::Update(key) => {
            let set: Vec<String> = cols
                .iter()
                .filter(|c| !key.contains(c))
                .map(|c| format!("{c}=EXCLUDED.{c}"))
                .collect();
            sql.push_str(&format!(
                " ON CONFLICT ({}) DO UPDATE SET {}",
                key.join(","),
                set.join(",")
            ));
        },
    }
    sql
}

/// Starts the writer; rows sent to the sink are inserted at most a second later.
pub fn spawn_writer(url: String) -> Sink {
    let (tx, mut rx) = mpsc::unbounded_channel::<Rec>();
    let pending = Arc::new(AtomicUsize::new(0));
    let done = pending.clone();
    tokio::spawn(async move {
        let mut client: Option<Client> = None;
        let mut buf: Vec<Rec> = Vec::new();
        loop {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
            while buf.len() < 20_000 {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Some(r)) => buf.push(r),
                    Ok(None) => return,
                    Err(_) => break,
                }
            }
            if buf.is_empty() {
                continue;
            }
            let n = buf.len();
            if client.as_ref().is_none_or(|c| c.is_closed()) {
                match connect(&url).await {
                    Ok(c) => client = Some(c),
                    Err(e) => {
                        warn!(
                            "postgres reconnect failed, dropping {} rows: {e}",
                            buf.len()
                        );
                        done.fetch_sub(buf.len(), Ordering::Relaxed);
                        buf.clear();
                        continue;
                    },
                }
            }
            let c = client.as_ref().expect("connected");
            let mut groups: BTreeMap<Group, Vec<Vec<Val>>> = BTreeMap::new();
            for r in buf.drain(..) {
                groups
                    .entry((r.table, r.cols, r.conflict))
                    .or_default()
                    .push(r.vals);
            }
            for ((table, cols, conflict), rows) in groups {
                let rows = if matches!(conflict, OnConflict::Update(_)) {
                    // one row per statement: a batch may touch the same key twice
                    rows.into_iter().map(|r| vec![r]).collect::<Vec<_>>()
                } else {
                    rows.chunks((MAX_PARAMS / cols.len()).max(1))
                        .map(<[Vec<Val>]>::to_vec)
                        .collect()
                };
                for chunk in rows {
                    let sql = insert_sql(table, cols, conflict, chunk.len());
                    let params: Vec<&(dyn ToSql + Sync)> = chunk
                        .iter()
                        .flat_map(|r| r.iter().map(|v| v as &(dyn ToSql + Sync)))
                        .collect();
                    if let Err(e) = c.execute(&sql, &params).await {
                        warn!("insert into {table} ({} rows): {e}", chunk.len());
                    }
                }
            }
            done.fetch_sub(n, Ordering::Relaxed);
        }
    });
    Sink { tx, pending }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_row_inserts_number_their_parameters() {
        assert_eq!(
            insert_sql("t", &["a", "b"], OnConflict::Nothing, 2),
            "INSERT INTO t (a,b) VALUES ($1,$2),($3,$4) ON CONFLICT DO NOTHING"
        );
        assert_eq!(
            insert_sql("t", &["id", "x", "y"], OnConflict::Update(&["id"]), 1),
            "INSERT INTO t (id,x,y) VALUES ($1,$2,$3) ON CONFLICT (id) DO UPDATE SET x=EXCLUDED.x,y=EXCLUDED.y"
        );
    }
}
