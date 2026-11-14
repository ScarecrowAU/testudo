//! Persistent run history.
//!
//! Layout under the data dir (`$TESTUDO_DATA_DIR`, else `$XDG_DATA_HOME/testudo`,
//! else `~/.local/share/testudo`):
//!
//! ```text
//! testudo.db                      SQLite (WAL): runs, tests, pages, events
//! runs/<run_id>/frames/<page>.tfr raw screencast frames (see `frames`)
//! exports/                        MP4s produced on demand
//! ```
//!
//! `events` is the source of truth: every hub event of a run with its receive
//! time, so a run can be replayed exactly as it was seen live. `runs`, `tests` and
//! `pages` are derived summaries for listing and cross-run queries.

pub mod export;
pub mod frames;
pub mod recorder;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use testudo_protocol::Event;

pub use recorder::Recorder;

#[cfg(test)]
mod tests;

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS runs (
    id          TEXT PRIMARY KEY,
    label       TEXT NOT NULL DEFAULT '',
    runner      TEXT NOT NULL DEFAULT '',
    cwd         TEXT NOT NULL DEFAULT '',
    args        TEXT NOT NULL DEFAULT '[]',
    owner_pid   INTEGER,
    started_at  REAL NOT NULL,
    ended_at    REAL,
    exit_code   INTEGER,
    status      TEXT NOT NULL DEFAULT 'running',
    total       INTEGER,
    passed      INTEGER NOT NULL DEFAULT 0,
    failed      INTEGER NOT NULL DEFAULT 0,
    skipped     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS runs_started ON runs(started_at);

CREATE TABLE IF NOT EXISTS tests (
    run_id      TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    test_id     TEXT NOT NULL,
    title       TEXT NOT NULL,
    title_path  TEXT NOT NULL DEFAULT '[]',
    file        TEXT,
    line        INTEGER,
    project     TEXT,
    status      TEXT,
    duration_ms REAL,
    error       TEXT,
    started_at  REAL,
    ended_at    REAL,
    PRIMARY KEY (run_id, test_id)
);
CREATE INDEX IF NOT EXISTS tests_identity ON tests(file, title);

CREATE TABLE IF NOT EXISTS pages (
    run_id      TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    page_id     TEXT NOT NULL,
    test_id     TEXT,
    browser     TEXT,
    opened_at   REAL,
    closed_at   REAL,
    frames      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (run_id, page_id)
);

CREATE TABLE IF NOT EXISTS events (
    run_id      TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,
    at          REAL NOT NULL,
    json        TEXT NOT NULL,
    PRIMARY KEY (run_id, seq)
) WITHOUT ROWID;
"#;

/// Which frame recordings to keep once a run finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepFrames {
    All,
    /// Keep frames only for pages whose test failed (or, for pages not tied to a
    /// test, whose run failed). Like Playwright's `retain-on-failure`.
    Failed,
}

/// How much history to keep. Events and summaries are small; frames are what
/// grows, so frames are trimmed first and run history last.
#[derive(Debug, Clone, Copy)]
pub struct Retention {
    /// Delete whole runs (history and frames) beyond the newest N.
    pub keep_runs: usize,
    /// Cap on total frame storage. Oldest runs lose their frames first; their
    /// test results and events stay browsable.
    pub max_frame_bytes: u64,
    pub keep_frames: KeepFrames,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            keep_runs: 200,
            max_frame_bytes: 5 * 1024 * 1024 * 1024,
            keep_frames: KeepFrames::All,
        }
    }
}

impl Retention {
    /// Defaults overridden by `TESTUDO_KEEP_RUNS`, `TESTUDO_MAX_FRAMES_MB`,
    /// `TESTUDO_KEEP_FRAMES=all|failed`.
    pub fn from_env() -> Self {
        let mut r = Self::default();
        if let Some(n) = env_parse("TESTUDO_KEEP_RUNS") {
            r.keep_runs = n;
        }
        if let Some(mb) = env_parse::<u64>("TESTUDO_MAX_FRAMES_MB") {
            r.max_frame_bytes = mb * 1024 * 1024;
        }
        if std::env::var("TESTUDO_KEEP_FRAMES").is_ok_and(|v| v.eq_ignore_ascii_case("failed")) {
            r.keep_frames = KeepFrames::Failed;
        }
        r
    }
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok()?.parse().ok()
}

#[derive(Debug, Default)]
pub struct PruneReport {
    pub runs_deleted: usize,
    pub recordings_deleted: usize,
    pub frame_bytes: u64,
}

pub fn default_data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("TESTUDO_DATA_DIR") {
        return PathBuf::from(d);
    }
    if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(d).join("testudo");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    home.join(".local/share/testudo")
}

#[derive(Debug, Clone)]
pub struct RunRow {
    pub id: String,
    pub label: String,
    pub runner: String,
    pub cwd: String,
    pub started_at: f64,
    pub ended_at: Option<f64>,
    pub exit_code: Option<i64>,
    pub status: String,
    pub total: Option<i64>,
    pub passed: i64,
    pub failed: i64,
    pub skipped: i64,
}

#[derive(Debug, Clone)]
pub struct PageRow {
    pub page_id: String,
    pub test_id: Option<String>,
    pub browser: Option<String>,
    pub opened_at: Option<f64>,
    pub closed_at: Option<f64>,
    pub frames: i64,
}

#[derive(Debug, Clone)]
pub struct FlakyTest {
    pub title: String,
    pub file: Option<String>,
    pub passes: i64,
    pub fails: i64,
}

#[derive(Debug, Clone)]
pub struct TestResultRow {
    pub run_id: String,
    pub status: Option<String>,
    pub duration_ms: Option<f64>,
    pub started_at: Option<f64>,
}

/// Read access (and pruning) for the GUI and CLI. Writes go through `Recorder`.
pub struct Store {
    dir: PathBuf,
    conn: Connection,
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let conn = open_conn(dir)?;
        Ok(Self { dir: dir.to_owned(), conn })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn frame_path(&self, run_id: &str, page_id: &str) -> PathBuf {
        frame_path(&self.dir, run_id, page_id)
    }

    pub fn exports_dir(&self) -> PathBuf {
        self.dir.join("exports")
    }

    pub fn list_runs(&self, limit: usize) -> Result<Vec<RunRow>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, label, runner, cwd, started_at, ended_at, exit_code, status, total, passed, failed, skipped
             FROM runs ORDER BY started_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], run_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn run(&self, run_id: &str) -> Result<Option<RunRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, label, runner, cwd, started_at, ended_at, exit_code, status, total, passed, failed, skipped
                 FROM runs WHERE id = ?1",
                [run_id],
                run_row,
            )
            .optional()?)
    }

    /// Every recorded event of a run in arrival order. Events this build cannot
    /// parse (written by a newer Testudo) are skipped.
    pub fn run_events(&self, run_id: &str) -> Result<Vec<(f64, Event)>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT at, json FROM events WHERE run_id = ?1 ORDER BY seq")?;
        let rows = stmt.query_map([run_id], |r| Ok((r.get::<_, f64>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (at, json) = row?;
            if let Ok(ev) = serde_json::from_str(&json) {
                out.push((at, ev));
            }
        }
        Ok(out)
    }

    pub fn run_pages(&self, run_id: &str) -> Result<Vec<PageRow>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT page_id, test_id, browser, opened_at, closed_at, frames FROM pages
             WHERE run_id = ?1 ORDER BY opened_at",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(PageRow {
                page_id: r.get(0)?,
                test_id: r.get(1)?,
                browser: r.get(2)?,
                opened_at: r.get(3)?,
                closed_at: r.get(4)?,
                frames: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Tests that both passed and failed within the newest `runs` runs,
    /// matched across runs by file + title path; most failures first.
    pub fn flaky_tests(&self, runs: usize) -> Result<Vec<FlakyTest>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT t.title, t.file,
                    SUM(t.status = 'passed') AS passes,
                    SUM(t.status IN ('failed', 'timedOut')) AS fails
             FROM tests t
             JOIN (SELECT id FROM runs ORDER BY started_at DESC LIMIT ?1) r ON r.id = t.run_id
             GROUP BY t.file, t.title_path
             HAVING passes > 0 AND fails > 0
             ORDER BY fails DESC, t.title",
        )?;
        let rows = stmt.query_map([runs as i64], |r| {
            Ok(FlakyTest {
                title: r.get(0)?,
                file: r.get(1)?,
                passes: r.get(2)?,
                fails: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Recent results of "the same test" across runs, matched by file + title path.
    pub fn test_history(&self, file: &str, title_path: &[String], limit: usize) -> Result<Vec<TestResultRow>> {
        let title_path = serde_json::to_string(title_path)?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT run_id, status, duration_ms, started_at FROM tests
             WHERE file = ?1 AND title_path = ?2 ORDER BY started_at DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![file, title_path, limit as i64], |r| {
            Ok(TestResultRow {
                run_id: r.get(0)?,
                status: r.get(1)?,
                duration_ms: r.get(2)?,
                started_at: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn delete_run(&self, run_id: &str) -> Result<()> {
        delete_run(&self.conn, &self.dir, run_id)
    }

    pub fn prune(&self, retention: &Retention) -> Result<PruneReport> {
        prune(&self.conn, &self.dir, retention)
    }
}

fn run_dir(dir: &Path, run_id: &str) -> PathBuf {
    dir.join("runs").join(sanitize(run_id))
}

fn delete_run(conn: &Connection, dir: &Path, run_id: &str) -> Result<()> {
    conn.execute("DELETE FROM runs WHERE id = ?1", [run_id])?;
    let _ = std::fs::remove_dir_all(run_dir(dir, run_id));
    Ok(())
}

fn dir_size(path: &Path) -> u64 {
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

/// Apply retention to finished runs: drop runs beyond `keep_runs`, then drop the
/// frames (only) of the oldest runs until frame storage fits `max_frame_bytes`.
pub(crate) fn prune(conn: &Connection, dir: &Path, retention: &Retention) -> Result<PruneReport> {
    let mut report = PruneReport::default();
    let finished: Vec<String> = {
        let mut stmt = conn.prepare("SELECT id FROM runs WHERE status != 'running' ORDER BY started_at DESC")?;
        stmt.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?
    };
    for id in finished.iter().skip(retention.keep_runs) {
        delete_run(conn, dir, id)?;
        report.runs_deleted += 1;
    }

    let mut total = 0u64;
    for id in finished.iter().take(retention.keep_runs) {
        let frames = run_dir(dir, id).join("frames");
        let size = dir_size(&frames);
        if size == 0 {
            continue;
        }
        if total + size > retention.max_frame_bytes {
            let _ = std::fs::remove_dir_all(&frames);
            report.recordings_deleted += 1;
        } else {
            total += size;
        }
    }
    report.frame_bytes = total;
    Ok(report)
}

fn run_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        id: r.get(0)?,
        label: r.get(1)?,
        runner: r.get(2)?,
        cwd: r.get(3)?,
        started_at: r.get(4)?,
        ended_at: r.get(5)?,
        exit_code: r.get(6)?,
        status: r.get(7)?,
        total: r.get(8)?,
        passed: r.get(9)?,
        failed: r.get(10)?,
        skipped: r.get(11)?,
    })
}

pub(crate) fn open_conn(dir: &Path) -> Result<Connection> {
    let conn = Connection::open(dir.join("testudo.db"))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;")?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < SCHEMA_VERSION {
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
    }
    Ok(conn)
}

pub(crate) fn frame_path(dir: &Path, run_id: &str, page_id: &str) -> PathBuf {
    dir.join("runs")
        .join(sanitize(run_id))
        .join("frames")
        .join(format!("{}.tfr", sanitize(page_id)))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}
