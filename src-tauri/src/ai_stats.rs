//! Analytics for the AI Router, in SQLite.
//!
//! The request log in [`crate::ai_logs`] is deliberately bounded: it keeps the
//! newest 50 or 100 entries and "Clear log" deletes it outright. That is the
//! right shape for a list of what just happened, and the wrong shape for a
//! dashboard — clearing the list used to wipe the chart with it.
//!
//! So the aggregates live here instead, in `logs\ai-stats.sqlite`, one row per
//! (hour, model, endpoint). Three consequences worth stating:
//!
//! - **Clearing the log does not clear the chart.** They are different files.
//! - **Pruning the log does not lose history.** A pruned entry was already
//!   counted the moment it finished.
//! - **A row is one hour, not one request.** A year of heavy use is a few
//!   thousand rows, so this file never needs pruning of its own.
//!
//! The write path is best-effort on purpose: a request that was served must not
//! be reported as failed because a statistics file could not be opened.

use crate::ai_logs::{Bucket, Entry, ModelStat, Stats};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::PathBuf;

/// How far back the dashboard looks. The chart has one bar per hour, so this is
/// also the number of bars.
pub const WINDOW_HOURS: u64 = 24;

const HOUR_MS: u64 = 3_600_000;

/// The database file, next to the request log.
pub fn file() -> PathBuf {
    crate::ai_logs::dir().join("ai-stats.sqlite")
}

/// One row per hour, model and endpoint. `hour` is unix milliseconds divided by
/// an hour, so bucketing is a range scan instead of date parsing in SQL.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS hourly (
  hour          INTEGER NOT NULL,
  model         TEXT    NOT NULL,
  endpoint      TEXT    NOT NULL,
  requests      INTEGER NOT NULL DEFAULT 0,
  failed        INTEGER NOT NULL DEFAULT 0,
  input_tokens  INTEGER NOT NULL DEFAULT 0,
  output_tokens INTEGER NOT NULL DEFAULT 0,
  cost          REAL    NOT NULL DEFAULT 0,
  ms_total      INTEGER NOT NULL DEFAULT 0,
  ttft_total    INTEGER NOT NULL DEFAULT 0,
  ttft_count    INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (hour, model, endpoint)
);
CREATE INDEX IF NOT EXISTS hourly_hour ON hourly (hour);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

/// Columns summed for one aggregate row, in the order [`Row::read`] expects.
const SUMS: &str = "SUM(requests), SUM(failed), SUM(input_tokens), SUM(output_tokens), \
                    SUM(cost), SUM(ms_total), SUM(ttft_total), SUM(ttft_count)";

fn open() -> rusqlite::Result<Connection> {
    let _ = std::fs::create_dir_all(crate::ai_logs::dir());
    let conn = Connection::open(file())?;
    // The router writes from one thread per request while the settings page
    // reads. WAL lets the reader see a consistent snapshot without blocking a
    // writer, and the timeout covers the brief moment both want the write lock.
    conn.busy_timeout(std::time::Duration::from_millis(2_000))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

/// Count one finished request.
///
/// Called from the router's own logging path, so it must never panic and never
/// block for long. A failure here loses one row of statistics, not the request.
pub fn record(entry: &Entry) {
    let Ok(conn) = open() else {
        return;
    };
    let _ = migrate(&conn);
    let _ = add(&conn, entry);
}

fn add(conn: &Connection, entry: &Entry) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO hourly
           (hour, model, endpoint, requests, failed, input_tokens, output_tokens,
            cost, ms_total, ttft_total, ttft_count)
         VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(hour, model, endpoint) DO UPDATE SET
           requests      = requests      + excluded.requests,
           failed        = failed        + excluded.failed,
           input_tokens  = input_tokens  + excluded.input_tokens,
           output_tokens = output_tokens + excluded.output_tokens,
           cost          = cost          + excluded.cost,
           ms_total      = ms_total      + excluded.ms_total,
           ttft_total    = ttft_total    + excluded.ttft_total,
           ttft_count    = ttft_count    + excluded.ttft_count",
        params![
            (entry.at / HOUR_MS) as i64,
            entry.model,
            entry.endpoint,
            i64::from(!entry.ok()),
            entry.input_tokens as i64,
            entry.output_tokens as i64,
            entry.cost,
            entry.ms as i64,
            entry.ttft_ms as i64,
            i64::from(entry.ttft_ms > 0),
        ],
    )?;
    Ok(())
}

/// Import the existing request log the first time this database is created.
///
/// Without this the dashboard would start at zero on the upgrade, which reads as
/// "all your history is gone". The flag is written after the import so a crash
/// halfway through repeats the import rather than skipping it — and repeating it
/// is safe, because a re-import only ever happens against a database that has no
/// flag, i.e. one that holds nothing yet.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let done: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'imported'", [], |row| {
            row.get(0)
        })
        .optional()?;
    if done.is_some() {
        return Ok(());
    }
    for entry in crate::ai_logs::all() {
        let _ = add(conn, &entry);
    }
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('imported', '1')",
        [],
    )?;
    Ok(())
}

/// One aggregated row, read back from the database.
#[derive(Default)]
struct Row {
    model: String,
    endpoint: String,
    requests: u64,
    failed: u64,
    input_tokens: u64,
    output_tokens: u64,
    cost: f64,
    ms_total: u64,
    ttft_total: u64,
    ttft_count: u64,
}

impl Row {
    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
        Ok(Row {
            model: row.get(0)?,
            endpoint: row.get(1)?,
            requests: row.get::<_, i64>(2)? as u64,
            failed: row.get::<_, i64>(3)? as u64,
            input_tokens: row.get::<_, i64>(4)? as u64,
            output_tokens: row.get::<_, i64>(5)? as u64,
            cost: row.get(6)?,
            ms_total: row.get::<_, i64>(7)? as u64,
            ttft_total: row.get::<_, i64>(8)? as u64,
            ttft_count: row.get::<_, i64>(9)? as u64,
        })
    }
}

/// The dashboard numbers for the last [`WINDOW_HOURS`], or `None` when the
/// database cannot be opened.
///
/// `None` is not an error the page should show: the caller falls back to
/// aggregating the request log, which is what the page did before this file
/// existed. A broken statistics file degrades the dashboard to a shorter
/// history; it does not blank it.
pub fn summary(now_ms: u64) -> Option<Stats> {
    let conn = open().ok()?;
    migrate(&conn).ok()?;

    let now_hour = (now_ms / HOUR_MS) as i64;
    let oldest = now_hour - (WINDOW_HOURS as i64 - 1);

    // Per-model totals, highest token count first — the "top models" bars.
    let mut statement = conn
        .prepare(&format!(
            "SELECT model, endpoint, {SUMS} FROM hourly WHERE hour >= ?1 \
             GROUP BY model, endpoint"
        ))
        .ok()?;
    let rows: Vec<Row> = statement
        .query_map(params![oldest], Row::read)
        .ok()?
        .filter_map(Result::ok)
        .collect();

    let mut stats = Stats {
        window_hours: WINDOW_HOURS,
        ..Stats::default()
    };
    let mut ms_total = 0u64;
    let mut ttft_total = 0u64;
    let mut ttft_count = 0u64;
    let mut models: std::collections::HashMap<String, ModelStat> = Default::default();
    let mut endpoints: std::collections::HashMap<String, ModelStat> = Default::default();

    for row in &rows {
        stats.requests += row.requests;
        stats.failed += row.failed;
        stats.input_tokens += row.input_tokens;
        stats.output_tokens += row.output_tokens;
        stats.cost += row.cost;
        ms_total += row.ms_total;
        ttft_total += row.ttft_total;
        ttft_count += row.ttft_count;

        let model = models
            .entry(row.model.clone())
            .or_insert_with(|| ModelStat {
                name: row.model.clone(),
                ..ModelStat::default()
            });
        model.requests += row.requests;
        model.input_tokens += row.input_tokens;
        model.output_tokens += row.output_tokens;
        model.cost += row.cost;

        let endpoint = endpoints
            .entry(row.endpoint.clone())
            .or_insert_with(|| ModelStat {
                name: row.endpoint.clone(),
                ..ModelStat::default()
            });
        endpoint.requests += row.requests;
        endpoint.input_tokens += row.input_tokens;
        endpoint.output_tokens += row.output_tokens;
        endpoint.cost += row.cost;
    }

    if stats.requests > 0 {
        stats.avg_ms = ms_total / stats.requests;
    }
    if ttft_count > 0 {
        stats.avg_ttft_ms = ttft_total / ttft_count;
    }
    // Aggregate throughput, not the mean of per-request rates: a 2-token answer
    // at 400 t/s must not outweigh a 2000-token answer at 40 t/s. `ms_total`
    // covers every request while `ttft_total` only covers those that reported a
    // first token, so the difference is exactly the summed generation time.
    if stats.output_tokens > 0 && ms_total > ttft_total {
        stats.avg_tps = round1(stats.output_tokens as f64 / ((ms_total - ttft_total) as f64 / 1000.0));
    }

    stats.models = models.into_values().collect();
    stats
        .models
        .sort_by(|a, b| b.total_tokens().cmp(&a.total_tokens()).then(a.name.cmp(&b.name)));
    stats.endpoints = endpoints.into_values().collect();
    stats
        .endpoints
        .sort_by(|a, b| b.requests.cmp(&a.requests).then(a.name.cmp(&b.name)));
    stats.series = series(&conn, now_hour).unwrap_or_else(|| {
        (0..WINDOW_HOURS)
            .rev()
            .map(|hours_ago| Bucket {
                hours_ago,
                ..Bucket::default()
            })
            .collect()
    });
    Some(stats)
}

/// One bucket per hour, oldest first, always [`WINDOW_HOURS`] long so the chart
/// keeps its width whether the window holds two requests or two thousand.
fn series(conn: &Connection, now_hour: i64) -> Option<Vec<Bucket>> {
    let oldest = now_hour - (WINDOW_HOURS as i64 - 1);
    let mut statement = conn
        .prepare(
            "SELECT hour, SUM(requests), SUM(input_tokens + output_tokens) \
             FROM hourly WHERE hour >= ?1 GROUP BY hour",
        )
        .ok()?;
    let mut buckets: Vec<Bucket> = (0..WINDOW_HOURS)
        .rev()
        .map(|hours_ago| Bucket {
            hours_ago,
            ..Bucket::default()
        })
        .collect();
    let rows = statement
        .query_map(params![oldest], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)? as u64,
                row.get::<_, i64>(2)? as u64,
            ))
        })
        .ok()?;
    for row in rows.filter_map(Result::ok) {
        let (hour, requests, tokens) = row;
        let ago = now_hour - hour;
        if ago < 0 || ago as usize >= buckets.len() {
            continue;
        }
        let slot = buckets.len() - 1 - ago as usize;
        buckets[slot].requests = requests;
        buckets[slot].tokens = tokens;
    }
    Some(buckets)
}

/// One decimal place is all a tokens-per-second readout can honestly claim.
pub fn round1(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    (value * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(at: u64, model: &str, input: u64, output: u64, ttft: u64, ms: u64) -> Entry {
        Entry {
            at,
            endpoint: "chat/completions".to_string(),
            model: model.to_string(),
            upstream: model.to_string(),
            provider: "test".to_string(),
            status: 200,
            ms,
            input_tokens: input,
            output_tokens: output,
            cost: 0.0,
            key: "Key".to_string(),
            stream: ttft > 0,
            error: String::new(),
            ttft_ms: ttft,
            tps: 0.0,
            estimated: false,
        }
    }

    /// A connection backed by a throwaway file, with the schema applied.
    ///
    /// The name needs a per-test discriminator, not just the clock: the test
    /// runner is parallel, and two tests starting in the same millisecond would
    /// otherwise share one database and see each other's rows.
    fn scratch() -> (Connection, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rustbox-stats-{}-{}-{}.sqlite",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst),
            crate::ai_logs::now_ms()
        ));
        let conn = Connection::open(&path).expect("open scratch db");
        conn.execute_batch(SCHEMA).expect("schema");
        (conn, path)
    }

    fn rows(conn: &Connection, now_ms: u64) -> Vec<Row> {
        let oldest = (now_ms / HOUR_MS) as i64 - (WINDOW_HOURS as i64 - 1);
        let mut statement = conn
            .prepare(&format!(
                "SELECT model, endpoint, {SUMS} FROM hourly WHERE hour >= ?1 \
                 GROUP BY model, endpoint ORDER BY model"
            ))
            .expect("prepare");
        statement
            .query_map(params![oldest], Row::read)
            .expect("query")
            .filter_map(Result::ok)
            .collect()
    }

    #[test]
    fn a_second_request_in_the_same_hour_adds_to_the_same_row() {
        let (conn, path) = scratch();
        let now = 1_000 * HOUR_MS + 5_000;
        add(&conn, &entry(now - 1_000, "a/one", 10, 5, 0, 100)).expect("first");
        add(&conn, &entry(now - 500, "a/one", 20, 7, 0, 300)).expect("second");

        let counted = rows(&conn, now);
        assert_eq!(counted.len(), 1, "one hour, one model, one endpoint");
        assert_eq!(counted[0].requests, 2);
        assert_eq!(counted[0].input_tokens, 30);
        assert_eq!(counted[0].output_tokens, 12);
        assert_eq!(counted[0].ms_total, 400);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_hour_boundary_splits_rows_instead_of_merging_them() {
        let (conn, path) = scratch();
        let now = 1_000 * HOUR_MS + 5_000;
        add(&conn, &entry(now - 1_000, "a/one", 10, 0, 0, 10)).expect("this hour");
        add(&conn, &entry(now - HOUR_MS - 1_000, "a/one", 10, 0, 0, 10)).expect("last hour");

        // Counted in the table, not through `rows`: `rows` groups by model on
        // purpose, which would hide the split this test is about.
        let stored: i64 = conn
            .query_row("SELECT COUNT(*) FROM hourly", [], |row| row.get(0))
            .expect("count");
        assert_eq!(stored, 2, "different hours are different rows");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_failure_is_counted_apart_from_a_success() {
        let (conn, path) = scratch();
        let now = 1_000 * HOUR_MS + 5_000;
        let mut failed = entry(now - 1_000, "a/one", 0, 0, 0, 50);
        failed.status = 429;
        add(&conn, &failed).expect("failed call");
        add(&conn, &entry(now - 900, "a/one", 1, 1, 0, 50)).expect("good call");

        let counted = rows(&conn, now);
        assert_eq!(counted[0].requests, 2);
        assert_eq!(counted[0].failed, 1);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_first_token_time_is_averaged_over_the_requests_that_had_one() {
        let (conn, path) = scratch();
        let now = 1_000 * HOUR_MS + 5_000;
        // Two streamed calls (ttft 100 and 300) and one that never reported a
        // first token, which must not drag the average toward zero.
        add(&conn, &entry(now - 3_000, "a/one", 0, 10, 100, 1_100)).expect("stream 1");
        add(&conn, &entry(now - 2_000, "a/one", 0, 10, 300, 1_300)).expect("stream 2");
        add(&conn, &entry(now - 1_000, "a/one", 0, 10, 0, 900)).expect("no ttft");

        let counted = rows(&conn, now);
        assert_eq!(counted[0].ttft_total, 400);
        assert_eq!(counted[0].ttft_count, 2, "only calls that reported one");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_window_excludes_rows_older_than_it() {
        let (conn, path) = scratch();
        let now = 1_000 * HOUR_MS;
        add(&conn, &entry(now - 1_000, "a/one", 1, 1, 0, 10)).expect("inside");
        add(&conn, &entry(now - (WINDOW_HOURS + 1) * HOUR_MS, "a/one", 5, 5, 0, 10))
            .expect("outside");

        let counted = rows(&conn, now);
        assert_eq!(counted[0].requests, 1, "the old row is outside the window");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn throughput_is_rounded_to_one_decimal_and_never_infinite() {
        assert_eq!(round1(41.234), 41.2);
        assert_eq!(round1(f64::INFINITY), 0.0);
        assert_eq!(round1(f64::NAN), 0.0);
    }

    #[test]
    fn the_database_lives_next_to_the_request_log() {
        assert_eq!(file(), crate::ai_logs::dir().join("ai-stats.sqlite"));
    }
}
