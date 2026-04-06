//! SQLite storage for cross-run benchmark summaries.

use rusqlite::{params, Connection, Result as SqlResult};
use std::path::Path;

/// Initialize the database schema.
pub fn init_db(path: &Path) -> SqlResult<Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS runs (
    run_id TEXT PRIMARY KEY,
    asset_key TEXT NOT NULL,
    provider TEXT NOT NULL DEFAULT '',
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    config_json TEXT NOT NULL DEFAULT '{}',
    envelope_json TEXT,
    summary_json TEXT
);

CREATE TABLE IF NOT EXISTS requests (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL REFERENCES runs(run_id),
    request_id INTEGER NOT NULL,
    chunk_id INTEGER,
    lane TEXT,
    attempt INTEGER,
    requested_range TEXT,
    served_range TEXT,
    response_code INTEGER,
    connection_id INTEGER,
    new_connection INTEGER,
    queue_ms REAL,
    dns_ms REAL,
    connect_ms REAL,
    tls_ms REAL,
    ttfb_ms REAL,
    total_ms REAL,
    total_bytes INTEGER,
    http_version TEXT,
    error_class TEXT
);

CREATE TABLE IF NOT EXISTS connections (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL REFERENCES runs(run_id),
    connection_id INTEGER NOT NULL,
    protocol TEXT,
    local_endpoint TEXT,
    remote_endpoint TEXT,
    first_use_ms REAL,
    last_use_ms REAL,
    request_count INTEGER,
    total_bytes INTEGER
);

CREATE TABLE IF NOT EXISTS frontier_snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL REFERENCES runs(run_id),
    timestamp_ms INTEGER NOT NULL,
    frontier_byte INTEGER NOT NULL,
    contiguous_bytes INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_requests_run ON requests(run_id);
CREATE INDEX IF NOT EXISTS idx_connections_run ON connections(run_id);
CREATE INDEX IF NOT EXISTS idx_frontier_run ON frontier_snapshots(run_id);
"#;

/// A stored run summary for listing/display.
#[derive(Debug, Clone)]
pub struct RunRecord {
    pub run_id: String,
    pub asset_key: String,
    pub provider: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub envelope_json: Option<String>,
    pub summary_json: Option<String>,
}

/// Insert a new run record.
pub fn insert_run(
    conn: &Connection,
    run_id: &str,
    asset_key: &str,
    provider: &str,
    started_at: i64,
    config_json: &str,
) -> SqlResult<()> {
    conn.execute(
        "INSERT INTO runs (run_id, asset_key, provider, started_at, config_json) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![run_id, asset_key, provider, started_at, config_json],
    )?;
    Ok(())
}

/// Update a run with completion data.
pub fn finish_run(
    conn: &Connection,
    run_id: &str,
    finished_at: i64,
    envelope_json: &str,
    summary_json: &str,
) -> SqlResult<()> {
    conn.execute(
        "UPDATE runs SET finished_at = ?1, envelope_json = ?2, summary_json = ?3 WHERE run_id = ?4",
        params![finished_at, envelope_json, summary_json, run_id],
    )?;
    Ok(())
}

/// Insert a request record.
pub fn insert_request(
    conn: &Connection,
    run_id: &str,
    request_id: u64,
    chunk_id: u64,
    lane: &str,
    attempt: u32,
    requested_range: &str,
    total_bytes: u64,
    dns_ms: f64,
    connect_ms: f64,
    tls_ms: f64,
    ttfb_ms: f64,
    queue_ms: f64,
    total_ms: f64,
    connection_id: i64,
    new_connection: bool,
    http_version: &str,
    error_class: Option<&str>,
) -> SqlResult<()> {
    conn.execute(
        "INSERT INTO requests (run_id, request_id, chunk_id, lane, attempt, requested_range, \
         total_bytes, dns_ms, connect_ms, tls_ms, ttfb_ms, queue_ms, total_ms, \
         connection_id, new_connection, http_version, error_class) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![
            run_id,
            request_id as i64,
            chunk_id as i64,
            lane,
            attempt,
            requested_range,
            total_bytes as i64,
            dns_ms,
            connect_ms,
            tls_ms,
            ttfb_ms,
            queue_ms,
            total_ms,
            connection_id,
            new_connection as i32,
            http_version,
            error_class,
        ],
    )?;
    Ok(())
}

/// Insert a connection record.
pub fn insert_connection(
    conn: &Connection,
    run_id: &str,
    connection_id: i64,
    protocol: &str,
    local_endpoint: &str,
    remote_endpoint: &str,
    first_use_ms: f64,
    last_use_ms: f64,
    request_count: u32,
    total_bytes: u64,
) -> SqlResult<()> {
    conn.execute(
        "INSERT INTO connections (run_id, connection_id, protocol, local_endpoint, remote_endpoint, \
         first_use_ms, last_use_ms, request_count, total_bytes) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            run_id,
            connection_id,
            protocol,
            local_endpoint,
            remote_endpoint,
            first_use_ms,
            last_use_ms,
            request_count,
            total_bytes as i64,
        ],
    )?;
    Ok(())
}

/// Insert a frontier snapshot.
pub fn insert_frontier_snapshot(
    conn: &Connection,
    run_id: &str,
    timestamp_ms: u64,
    frontier_byte: u64,
    contiguous_bytes: u64,
) -> SqlResult<()> {
    conn.execute(
        "INSERT INTO frontier_snapshots (run_id, timestamp_ms, frontier_byte, contiguous_bytes) \
         VALUES (?1, ?2, ?3, ?4)",
        params![
            run_id,
            timestamp_ms as i64,
            frontier_byte as i64,
            contiguous_bytes as i64,
        ],
    )?;
    Ok(())
}

/// List all runs, most recent first.
pub fn list_runs(conn: &Connection) -> SqlResult<Vec<RunRecord>> {
    let mut stmt = conn.prepare(
        "SELECT run_id, asset_key, provider, started_at, finished_at, envelope_json, summary_json \
         FROM runs ORDER BY started_at DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(RunRecord {
            run_id: row.get(0)?,
            asset_key: row.get(1)?,
            provider: row.get(2)?,
            started_at: row.get(3)?,
            finished_at: row.get(4)?,
            envelope_json: row.get(5)?,
            summary_json: row.get(6)?,
        })
    })?;
    rows.collect()
}

/// Get a single run by ID.
pub fn get_run(conn: &Connection, run_id: &str) -> SqlResult<Option<RunRecord>> {
    let mut stmt = conn.prepare(
        "SELECT run_id, asset_key, provider, started_at, finished_at, envelope_json, summary_json \
         FROM runs WHERE run_id = ?1",
    )?;
    let mut rows = stmt.query_map(params![run_id], |row| {
        Ok(RunRecord {
            run_id: row.get(0)?,
            asset_key: row.get(1)?,
            provider: row.get(2)?,
            started_at: row.get(3)?,
            finished_at: row.get(4)?,
            envelope_json: row.get(5)?,
            summary_json: row.get(6)?,
        })
    })?;
    match rows.next() {
        Some(Ok(r)) => Ok(Some(r)),
        Some(Err(e)) => Err(e),
        None => Ok(None),
    }
}

/// Get request stats for a run.
pub fn get_request_stats(
    conn: &Connection,
    run_id: &str,
) -> SqlResult<(u32, f64, f64, f64)> {
    let mut stmt = conn.prepare(
        "SELECT COUNT(*), \
         COALESCE(AVG(ttfb_ms), 0), \
         COALESCE(AVG(total_ms), 0), \
         COALESCE(SUM(total_bytes), 0) \
         FROM requests WHERE run_id = ?1",
    )?;
    stmt.query_row(params![run_id], |row| {
        Ok((
            row.get::<_, i32>(0)? as u32,
            row.get::<_, f64>(1)?,
            row.get::<_, f64>(2)?,
            row.get::<_, f64>(3)?,
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Connection {
        init_db(Path::new(":memory:")).unwrap()
    }

    #[test]
    fn schema_creates_tables() {
        let conn = test_db();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(tables.contains(&"runs".to_string()));
        assert!(tables.contains(&"requests".to_string()));
        assert!(tables.contains(&"connections".to_string()));
        assert!(tables.contains(&"frontier_snapshots".to_string()));
    }

    #[test]
    fn insert_and_list_run() {
        let conn = test_db();
        insert_run(&conn, "run_1", "rd:123:0:1000", "realdebrid", 1000, "{}").unwrap();
        finish_run(&conn, "run_1", 2000, r#"{"measuredAtMs":2000}"#, "{}").unwrap();

        let runs = list_runs(&conn).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, "run_1");
        assert_eq!(runs[0].asset_key, "rd:123:0:1000");
        assert!(runs[0].envelope_json.is_some());
    }

    #[test]
    fn get_run_by_id() {
        let conn = test_db();
        insert_run(&conn, "run_42", "pm:abc:5000", "premiumize", 3000, "{}").unwrap();

        let found = get_run(&conn, "run_42").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().provider, "premiumize");

        let missing = get_run(&conn, "run_99").unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn insert_request_and_stats() {
        let conn = test_db();
        insert_run(&conn, "run_1", "key", "rd", 1000, "{}").unwrap();
        insert_request(
            &conn, "run_1", 1, 0, "urgent", 1, "0-1023", 1024, 5.0, 10.0, 8.0, 20.0, 1.0,
            50.0, 42, true, "h1.1", None,
        )
        .unwrap();
        insert_request(
            &conn, "run_1", 2, 1, "prefetch", 1, "1024-2047", 1024, 0.0, 0.0, 0.0, 15.0, 0.5,
            30.0, 42, false, "h1.1", None,
        )
        .unwrap();

        let (count, avg_ttfb, avg_total, sum_bytes) =
            get_request_stats(&conn, "run_1").unwrap();
        assert_eq!(count, 2);
        assert!((avg_ttfb - 17.5).abs() < 0.01);
        assert!((avg_total - 40.0).abs() < 0.01);
        assert!((sum_bytes - 2048.0).abs() < 0.01);
    }

    #[test]
    fn insert_connection_and_frontier() {
        let conn = test_db();
        insert_run(&conn, "run_1", "key", "rd", 1000, "{}").unwrap();

        insert_connection(&conn, "run_1", 1, "https", "127.0.0.1:5000", "93.184.216.34:443", 0.0, 100.0, 3, 50000).unwrap();
        insert_frontier_snapshot(&conn, "run_1", 100, 131072, 131072).unwrap();
        insert_frontier_snapshot(&conn, "run_1", 200, 262144, 262144).unwrap();

        let count: i32 = conn
            .query_row("SELECT COUNT(*) FROM frontier_snapshots WHERE run_id = 'run_1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }
}
