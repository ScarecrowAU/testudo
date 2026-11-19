//! Hub subscriber that persists every run: events to SQLite, frames to per-page
//! frame files. Runs on its own thread so slow disks never stall the UI or ingest.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use anyhow::Result;
use rusqlite::{Connection, params};
use testudo_core::{Hub, HubMessage};
use testudo_protocol::{Event, Message};
use tracing::{debug_span, info, info_span, warn};

use crate::frames::FrameWriter;
use crate::{KeepFrames, PruneReport, Retention, Store, frame_path, open_conn, prune};

/// Commit at most this many events per transaction under load.
const MAX_BATCH: usize = 1000;

pub struct Recorder {
    thread: Option<JoinHandle<()>>,
}

impl Recorder {
    /// Recover runs orphaned by a crashed Testudo, prune old runs, then start
    /// recording everything published on `hub`.
    pub fn spawn(dir: &Path, hub: &Hub, retention: Retention) -> Result<Self> {
        let store = Store::open(dir)?;
        recover_orphans(&store.conn)?;
        log_prune(store.prune(&retention)?);
        drop(store);

        let mut writer = Writer {
            dir: dir.to_owned(),
            conn: open_conn(dir)?,
            seq: HashMap::new(),
            run_end: HashMap::new(),
            frames: HashMap::new(),
            in_tx: false,
            batched: 0,
            retention,
        };
        let mut rx = hub.subscribe();
        let thread = std::thread::Builder::new()
            .name("testudo-recorder".into())
            .spawn(move || {
                while let Some(msg) = rx.blocking_recv() {
                    if let Err(e) = writer.handle(msg) {
                        warn!("recorder: {e:#}");
                    }
                    // Commit when idle or when the batch is large; under load this
                    // groups thousands of network events into few transactions.
                    if rx.is_empty() || writer.batched >= MAX_BATCH {
                        writer.commit();
                    }
                }
                writer.commit();
            })?;
        Ok(Self { thread: Some(thread) })
    }

    /// Wait for the recorder to drain. Call after `Hub::close`.
    pub fn join(mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Writer {
    dir: PathBuf,
    conn: Connection,
    seq: HashMap<String, i64>,
    run_end: HashMap<String, String>,
    frames: HashMap<(String, String), FrameWriter>,
    in_tx: bool,
    batched: usize,
    retention: Retention,
}

impl Writer {
    fn begin(&mut self) -> Result<()> {
        if !self.in_tx {
            self.conn.execute_batch("BEGIN")?;
            self.in_tx = true;
        }
        Ok(())
    }

    fn commit(&mut self) {
        // Debug level: under light load this runs per message.
        let _span = debug_span!(
            "recorder_commit",
            events = self.batched,
            frame_files = self.frames.len()
        )
        .entered();
        for w in self.frames.values_mut() {
            if let Err(e) = w.flush() {
                warn!("recorder: flushing frames: {e:#}");
            }
        }
        if self.in_tx {
            if let Err(e) = self.conn.execute_batch("COMMIT") {
                warn!("recorder: commit failed: {e:#}");
            }
            self.in_tx = false;
            self.batched = 0;
        }
    }

    fn handle(&mut self, msg: HubMessage) -> Result<()> {
        self.begin()?;
        let HubMessage { run_id, at, message } = msg;
        self.conn.execute(
            "INSERT OR IGNORE INTO runs (id, started_at, owner_pid) VALUES (?1, ?2, ?3)",
            params![run_id, at, std::process::id()],
        )?;

        match message {
            Message::Frame { header, jpeg } => {
                let key = (run_id, header.page_id);
                if !self.frames.contains_key(&key) {
                    let w = FrameWriter::create(&frame_path(&self.dir, &key.0, &key.1))?;
                    self.frames.insert(key.clone(), w);
                }
                self.frames.get_mut(&key).unwrap().append(at, &jpeg)?;
            }
            Message::Event(event) => {
                let seq = self.next_seq(&run_id)?;
                self.conn.execute(
                    "INSERT INTO events (run_id, seq, at, json) VALUES (?1, ?2, ?3, ?4)",
                    params![run_id, seq, at, serde_json::to_string(&event)?],
                )?;
                self.batched += 1;
                self.derive(&run_id, at, event)?;
            }
        }
        Ok(())
    }

    fn next_seq(&mut self, run_id: &str) -> Result<i64> {
        if !self.seq.contains_key(run_id) {
            let max: Option<i64> =
                self.conn
                    .query_row("SELECT MAX(seq) FROM events WHERE run_id = ?1", [run_id], |r| r.get(0))?;
            self.seq.insert(run_id.to_owned(), max.map_or(0, |m| m + 1));
        }
        let seq = self.seq.get_mut(run_id).unwrap();
        let out = *seq;
        *seq += 1;
        Ok(out)
    }

    /// Maintain the summary tables.
    fn derive(&mut self, run_id: &str, at: f64, event: Event) -> Result<()> {
        let c = &self.conn;
        match event {
            Event::RunStarted {
                label,
                runner,
                cwd,
                args,
                ..
            } => {
                c.execute(
                    "UPDATE runs SET label = ?2, runner = ?3, cwd = ?4, args = ?5, started_at = ?6 WHERE id = ?1",
                    params![run_id, label, runner, cwd, serde_json::to_string(&args)?, at],
                )?;
            }
            Event::RunBegin { total_tests } => {
                c.execute("UPDATE runs SET total = ?2 WHERE id = ?1", params![run_id, total_tests])?;
            }
            Event::TestBegin {
                test_id,
                title,
                title_path,
                file,
                line,
                project,
                ..
            } => {
                c.execute(
                    "INSERT INTO tests (run_id, test_id, title, title_path, file, line, project, started_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT (run_id, test_id) DO UPDATE SET
                        status = NULL, error = NULL, duration_ms = NULL, ended_at = NULL,
                        started_at = excluded.started_at",
                    params![
                        run_id,
                        test_id,
                        title,
                        serde_json::to_string(&title_path)?,
                        file,
                        line,
                        project,
                        at
                    ],
                )?;
            }
            Event::TestEnd {
                test_id,
                status,
                duration_ms,
                error,
                ..
            } => {
                let status = serde_json::to_value(status)?.as_str().unwrap_or("unknown").to_owned();
                c.execute(
                    "UPDATE tests SET status = ?3, duration_ms = ?4, error = ?5, ended_at = ?6
                     WHERE run_id = ?1 AND test_id = ?2",
                    params![run_id, test_id, status, duration_ms, error, at],
                )?;
                // Safety net: a result that lands after the run was finalised
                // must still be reflected in the summary.
                c.execute(
                    "UPDATE runs SET
                        passed  = (SELECT COUNT(*) FROM tests WHERE run_id = ?1 AND status = 'passed'),
                        failed  = (SELECT COUNT(*) FROM tests WHERE run_id = ?1 AND status IN ('failed', 'timedOut')),
                        skipped = (SELECT COUNT(*) FROM tests WHERE run_id = ?1 AND status = 'skipped')
                     WHERE id = ?1 AND status != 'running'",
                    [run_id],
                )?;
            }
            Event::PageOpened {
                page_id,
                test_id,
                browser,
                ..
            } => {
                c.execute(
                    "INSERT OR REPLACE INTO pages (run_id, page_id, test_id, browser, opened_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![run_id, page_id, test_id, browser, at],
                )?;
            }
            Event::PageClosed { page_id } => {
                let frames = self
                    .frames
                    .remove(&(run_id.to_owned(), page_id.clone()))
                    .map(|mut w| {
                        let _ = w.flush();
                        w.frames as i64
                    })
                    .unwrap_or(0);
                c.execute(
                    "UPDATE pages SET closed_at = ?3, frames = ?4 WHERE run_id = ?1 AND page_id = ?2",
                    params![run_id, page_id, at, frames],
                )?;
            }
            Event::RunEnd { status } => {
                self.run_end.insert(run_id.to_owned(), status);
            }
            Event::ProcessExited { code } => self.finish_run(run_id, at, code)?,
            _ => {}
        }
        Ok(())
    }

    fn finish_run(&mut self, run_id: &str, at: f64, code: Option<i32>) -> Result<()> {
        let _span = info_span!("recorder_finish_run", run.id = run_id, exit_code = code).entered();
        let reported = self.run_end.remove(run_id);
        let status = match (reported.as_deref(), code) {
            (Some(s @ ("passed" | "failed" | "interrupted" | "timedout")), _) => s.to_owned(),
            (_, Some(0)) => "passed".into(),
            (_, None) => "killed".into(),
            _ => "failed".into(),
        };

        let open: Vec<(String, String)> = self.frames.keys().filter(|k| k.0 == run_id).cloned().collect();
        for key in open {
            if let Some(mut w) = self.frames.remove(&key) {
                let _ = w.flush();
                self.conn.execute(
                    "UPDATE pages SET frames = ?3, closed_at = COALESCE(closed_at, ?4)
                     WHERE run_id = ?1 AND page_id = ?2",
                    params![key.0, key.1, w.frames as i64, at],
                )?;
            }
        }

        self.conn.execute(
            "UPDATE runs SET
                ended_at = ?2, exit_code = ?3, status = ?4,
                passed  = (SELECT COUNT(*) FROM tests WHERE run_id = ?1 AND status = 'passed'),
                failed  = (SELECT COUNT(*) FROM tests WHERE run_id = ?1 AND status IN ('failed', 'timedOut')),
                skipped = (SELECT COUNT(*) FROM tests WHERE run_id = ?1 AND status = 'skipped')
             WHERE id = ?1",
            params![run_id, at, code, status],
        )?;
        self.seq.remove(run_id);

        if self.retention.keep_frames == KeepFrames::Failed {
            self.drop_passing_frames(run_id, &status)?;
        }
        // Commit first so prune sees this run as finished.
        self.commit();
        log_prune(prune(&self.conn, &self.dir, &self.retention)?);
        Ok(())
    }

    fn drop_passing_frames(&mut self, run_id: &str, run_status: &str) -> Result<()> {
        let pages: Vec<(String, Option<String>)> = {
            let mut stmt = self.conn.prepare(
                "SELECT p.page_id, t.status FROM pages p
                 LEFT JOIN tests t ON t.run_id = p.run_id AND t.test_id = p.test_id
                 WHERE p.run_id = ?1",
            )?;
            stmt.query_map([run_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?
        };
        for (page_id, test_status) in pages {
            let keep = match test_status.as_deref() {
                Some(s) => matches!(s, "failed" | "timedOut" | "interrupted"),
                None => run_status != "passed",
            };
            if !keep {
                let _ = std::fs::remove_file(frame_path(&self.dir, run_id, &page_id));
            }
        }
        Ok(())
    }
}

fn log_prune(r: PruneReport) {
    if r.runs_deleted + r.recordings_deleted > 0 {
        info!(
            runs_deleted = r.runs_deleted,
            recordings_deleted = r.recordings_deleted,
            frame_mb = r.frame_bytes / (1024 * 1024),
            "applied retention"
        );
    }
}

/// Runs still marked `running` whose owning Testudo process is gone were cut short
/// by a crash or kill: close them out.
fn recover_orphans(conn: &Connection) -> Result<()> {
    let me = std::process::id() as i64;
    let orphans: Vec<(String, Option<i64>)> = {
        let mut stmt = conn.prepare("SELECT id, owner_pid FROM runs WHERE status = 'running'")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?
    };
    for (id, pid) in orphans {
        if pid.is_some_and(|p| p != me && process_alive(p)) {
            continue; // another live Testudo instance owns it
        }
        conn.execute(
            "UPDATE runs SET status = 'interrupted',
                ended_at = COALESCE(ended_at, (SELECT MAX(at) FROM events WHERE run_id = ?1), started_at)
             WHERE id = ?1",
            [&id],
        )?;
        info!(run = %id, "marked orphaned run as interrupted");
    }
    Ok(())
}

#[cfg(unix)]
fn process_alive(pid: i64) -> bool {
    // Signal 0 checks existence without sending anything.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
fn process_alive(_pid: i64) -> bool {
    true
}
